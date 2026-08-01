//! Port forward handler for HDC daemon.

use hdc_protocol::config::{HdcCommand, MessageLevel, TaskMessage};
use hdc_protocol::serializer::concat_pack;
use std::collections::HashMap;
use std::io::{self, Error, ErrorKind};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::{mpsc, Mutex};
use tokio::time::Duration;
use tracing::{error, info, warn};

use crate::task::{send_shared, SharedWriter};

// ============================================================================
// Shared state
// ============================================================================

#[derive(Clone)]
struct ForwardContext {
    channel_id: u32,
    wr: SharedWriter,
}

struct FportConn {
    channel_id: u32,
    tx: mpsc::UnboundedSender<Vec<u8>>,
}

#[derive(Clone)]
struct RportListenerHandle {
    tx: mpsc::UnboundedSender<RportMsg>,
    abort_handle: tokio::task::AbortHandle,
}

#[derive(Default)]
struct ForwardStateInner {
    // fport: per-forward-id sender to the bridge task (for TCP data from host -> remote)
    fport_senders: HashMap<u32, FportConn>,
    // rport: listeners keyed by the original channel_id from ForwardInit
    rport_listeners: HashMap<u32, RportListenerHandle>,
}

#[derive(Clone, Default)]
struct ForwardState {
    inner: Arc<Mutex<ForwardStateInner>>,
}

static STATE: std::sync::OnceLock<ForwardState> = std::sync::OnceLock::new();

fn get_state() -> ForwardState {
    STATE.get_or_init(ForwardState::default).clone()
}

enum RportMsg {
    CheckResult { ctx_id: u32, ok: bool },
    ActiveMaster { conn_id: u32 },
    Data { conn_id: u32, data: Vec<u8> },
    FreeContext { conn_id: u32 },
    Shutdown,
}

enum RportConnMsg {
    ActiveMaster,
    Data(Vec<u8>),
    Shutdown,
}

// ============================================================================
// Public API
// ============================================================================

/// Called when the host sends KernelChannelClose for a channel. Clean up any
/// forward resources associated with that channel. Returns true if the channel
/// was a known forward channel and should not cause the whole session to abort.
pub async fn handle_channel_close(channel_id: u32) -> bool {
    let state = get_state();
    let mut guard = state.inner.lock().await;
    let mut handled = false;

    if let Some(listener) = guard.rport_listeners.remove(&channel_id) {
        let _ = listener.tx.send(RportMsg::Shutdown);
        listener.abort_handle.abort();
        handled = true;
    }

    let before = guard.fport_senders.len();
    guard.fport_senders.retain(|_, conn| conn.channel_id != channel_id);
    if guard.fport_senders.len() != before {
        handled = true;
    }
    handled
}

pub async fn handle_forward_task(
    msg: TaskMessage,
    _session_id: u32,
    wr: SharedWriter,
) -> io::Result<()> {
    let state = get_state();

    // Route rport-related messages to the listener task for this channel.
    {
        let guard = state.inner.lock().await;
        if let Some(listener) = guard.rport_listeners.get(&msg.channel_id) {
            let tx = listener.tx.clone();
            drop(guard);
            match msg.command {
                HdcCommand::ForwardCheckResult => {
                    if msg.payload.len() >= 5 {
                        let ctx_id = u32::from_be_bytes([
                            msg.payload[0], msg.payload[1], msg.payload[2], msg.payload[3],
                        ]);
                        let ok = msg.payload[4] != 0;
                        let _ = tx.send(RportMsg::CheckResult { ctx_id, ok });
                    }
                    return Ok(());
                }
                HdcCommand::ForwardActiveMaster => {
                    if msg.payload.len() >= 4 {
                        let conn_id = u32::from_be_bytes([
                            msg.payload[0], msg.payload[1], msg.payload[2], msg.payload[3],
                        ]);
                        let _ = tx.send(RportMsg::ActiveMaster { conn_id });
                    }
                    return Ok(());
                }
                HdcCommand::ForwardData => {
                    if msg.payload.len() >= 4 {
                        let conn_id = u32::from_be_bytes([
                            msg.payload[0], msg.payload[1], msg.payload[2], msg.payload[3],
                        ]);
                        let data = msg.payload[4..].to_vec();
                        let _ = tx.send(RportMsg::Data { conn_id, data });
                    }
                    return Ok(());
                }
                HdcCommand::ForwardFreeContext => {
                    if msg.payload.len() >= 4 {
                        let conn_id = u32::from_be_bytes([
                            msg.payload[0], msg.payload[1], msg.payload[2], msg.payload[3],
                        ]);
                        let _ = tx.send(RportMsg::FreeContext { conn_id });
                    }
                    return Ok(());
                }
                _ => {}
            }
        }
    }

    match msg.command {
        HdcCommand::ForwardInit | HdcCommand::ForwardRportInit => {
            handle_rport_init(msg, wr, state).await
        }
        HdcCommand::ForwardCheck => {
            if msg.payload.len() < 12 + 1 {
                warn!("ForwardCheck payload too short: {} bytes", msg.payload.len());
                return Ok(());
            }
            let ctx_id = u32::from_be_bytes([msg.payload[0], msg.payload[1], msg.payload[2], msg.payload[3]]);
            let remote_spec = parse_null_terminated(&msg.payload[12..]);
            info!("ForwardCheck received: ctx_id={ctx_id}, remote={remote_spec}");

            let result = check_remote_reachable(&remote_spec).await;
            let flag: u8 = if result.is_ok() { 1 } else { 0 };
            if let Err(ref e) = result {
                warn!("ForwardCheck failed for {remote_spec}: {e}");
            }

            // Official daemon treats any non-null payload as success; send a single byte flag.
            let mut payload = vec![0u8; 4 + 8 + 1];
            payload[..4].copy_from_slice(&ctx_id.to_be_bytes());
            payload[12] = flag;
            let response = TaskMessage {
                channel_id: msg.channel_id,
                command: HdcCommand::ForwardCheckResult,
                payload,
            };
            send_shared(&wr, &response).await
        }
        HdcCommand::ForwardActiveSlave => {
            if msg.payload.len() < 12 + 1 {
                warn!("ForwardActiveSlave payload too short: {} bytes", msg.payload.len());
                return Ok(());
            }
            let forward_id = u32::from_be_bytes([msg.payload[0], msg.payload[1], msg.payload[2], msg.payload[3]]);
            let remote_spec = parse_null_terminated(&msg.payload[12..]);
            info!("ForwardActiveSlave received: forward_id={forward_id}, remote={remote_spec}");

            match connect_remote(&remote_spec).await {
                Ok(stream) => {
                    info!("ForwardActiveSlave connected to {remote_spec}, forward_id={forward_id}");
                    let (mut remote_rd, mut remote_wr) = stream.into_split();

                    // Channel: host -> remote writer
                    let (host_to_remote_tx, mut host_to_remote_rx) = mpsc::unbounded_channel::<Vec<u8>>();
                    state
                        .inner
                        .lock()
                        .await
                        .fport_senders
                        .insert(forward_id, FportConn { channel_id: msg.channel_id, tx: host_to_remote_tx });

                    let ctx = ForwardContext { channel_id: msg.channel_id, wr: wr.clone() };

                    // Task: remote -> host (read from remote, send ForwardData)
                    let ctx_rd = ctx.clone();
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 4096];
                        loop {
                            match remote_rd.read(&mut buf).await {
                                Ok(0) => break,
                                Ok(n) => {
                                    let mut payload = vec![0u8; 4 + n];
                                    payload[..4].copy_from_slice(&forward_id.to_be_bytes());
                                    payload[4..].copy_from_slice(&buf[..n]);
                                    let data_msg = TaskMessage {
                                        channel_id: ctx_rd.channel_id,
                                        command: HdcCommand::ForwardData,
                                        payload,
                                    };
                                    if send_shared(&ctx_rd.wr, &data_msg).await.is_err() {
                                        break;
                                    }
                                }
                                Err(e) => {
                                    warn!("Forward read error for forward_id={forward_id}: {e}");
                                    break;
                                }
                            }
                        }
                        // Send ForwardFreeContext when remote closes
                        let free_msg = TaskMessage {
                            channel_id: ctx_rd.channel_id,
                            command: HdcCommand::ForwardFreeContext,
                            payload: forward_id.to_be_bytes().to_vec(),
                        };
                        let _ = send_shared(&ctx_rd.wr, &free_msg).await;
                    });

                    // Task: host -> remote (write to remote)
                    tokio::spawn(async move {
                        let mut remote_wr = remote_wr;
                        while let Some(data) = host_to_remote_rx.recv().await {
                            if remote_wr.write_all(&data).await.is_err() {
                                break;
                            }
                        }
                        let _ = remote_wr.shutdown().await;
                    });

                    // Send ForwardActiveMaster
                    let mut active_payload = vec![0u8; 4 + 8 + remote_spec.len() + 1];
                    active_payload[..4].copy_from_slice(&forward_id.to_be_bytes());
                    active_payload[12..12 + remote_spec.len()].copy_from_slice(remote_spec.as_bytes());
                    let active_master = TaskMessage {
                        channel_id: msg.channel_id,
                        command: HdcCommand::ForwardActiveMaster,
                        payload: active_payload,
                    };
                    send_shared(&wr, &active_master).await
                }
                Err(e) => {
                    warn!("ForwardActiveSlave failed to connect {remote_spec}: {e}");
                    // Notify failure via ForwardFreeContext
                    let free_msg = TaskMessage {
                        channel_id: msg.channel_id,
                        command: HdcCommand::ForwardFreeContext,
                        payload: forward_id.to_be_bytes().to_vec(),
                    };
                    let _ = send_shared(&wr, &free_msg).await;
                    Ok(())
                }
            }
        }
        HdcCommand::ForwardData => {
            if msg.payload.len() < 4 {
                warn!("ForwardData payload too short");
                return Ok(());
            }
            let forward_id = u32::from_be_bytes([msg.payload[0], msg.payload[1], msg.payload[2], msg.payload[3]]);
            let data = msg.payload[4..].to_vec();
            let mut inner = state.inner.lock().await;
            if let Some(conn) = inner.fport_senders.get(&forward_id) {
                let _ = conn.tx.send(data);
            } else {
                warn!("ForwardData for unknown forward_id={forward_id}");
            }
            Ok(())
        }
        HdcCommand::ForwardFreeContext => {
            if msg.payload.len() < 4 {
                return Ok(());
            }
            let forward_id = u32::from_be_bytes([msg.payload[0], msg.payload[1], msg.payload[2], msg.payload[3]]);
            info!("ForwardFreeContext received: forward_id={forward_id}");
            state.inner.lock().await.fport_senders.remove(&forward_id);
            Ok(())
        }
        HdcCommand::ForwardList | HdcCommand::ForwardRportList => {
            info!("forward list requested");
            echo_client(&wr, msg.channel_id, "", MessageLevel::Ok).await
        }
        HdcCommand::ForwardRemove | HdcCommand::ForwardRportRemove => {
            let payload = String::from_utf8_lossy(&msg.payload);
            info!("forward remove: {payload}");
            echo_client(&wr, msg.channel_id, "Forward removed", MessageLevel::Ok).await
        }
        HdcCommand::ForwardSuccess => {
            // Daemon sends ForwardSuccess; it does not expect to receive it.
            info!("ForwardSuccess received on daemon, ignoring");
            Ok(())
        }
        _ => {
            warn!("unhandled forward command: {:?}", msg.command);
            echo_client(&wr, msg.channel_id, "Command not implemented", MessageLevel::Fail).await
        }
    }
}

// ============================================================================
// Reverse port forwarding (daemon side listener)
// ============================================================================

async fn handle_rport_init(
    msg: TaskMessage,
    wr: SharedWriter,
    state: ForwardState,
) -> io::Result<()> {
    let payload = String::from_utf8_lossy(&msg.payload);
    let parts: Vec<&str> = payload.split_whitespace().collect();
    if parts.len() < 2 {
        return echo_client(
            &wr,
            msg.channel_id,
            "Invalid rport parameters, expected: remote_spec local_spec",
            MessageLevel::Fail,
        )
        .await;
    }
    let remote_spec = parts[0].to_string();
    let local_spec = parts[1].to_string();

    info!(
        "rport init: channel_id={}, remote={}, local={}",
        msg.channel_id, remote_spec, local_spec
    );

    let listener = match start_listener(&remote_spec).await {
        Ok(l) => l,
        Err(e) => {
            return echo_client(
                &wr,
                msg.channel_id,
                &format!("Failed to bind {remote_spec}: {e}"),
                MessageLevel::Fail,
            )
            .await;
        }
    };

    let (tx, rx) = mpsc::unbounded_channel();
    let handle = tokio::spawn(run_rport_listener(
        msg.channel_id,
        remote_spec,
        local_spec,
        listener,
        wr.clone(),
        rx,
    ));

    let mut guard = state.inner.lock().await;
    guard.rport_listeners.insert(
        msg.channel_id,
        RportListenerHandle {
            tx,
            abort_handle: handle.abort_handle(),
        },
    );
    Ok(())
}

async fn run_rport_listener(
    channel_id: u32,
    remote_spec: String,
    local_spec: String,
    listener: LocalListener,
    wr: SharedWriter,
    mut rx: mpsc::UnboundedReceiver<RportMsg>,
) {
    let listener_id = rand::random::<u32>();

    // 1. Ask the host to verify the local endpoint is reachable.
    let mut check_payload = vec![0u8; 4 + 8 + local_spec.len() + 1];
    check_payload[..4].copy_from_slice(&listener_id.to_be_bytes());
    check_payload[12..12 + local_spec.len()].copy_from_slice(local_spec.as_bytes());
    let check_msg = TaskMessage {
        channel_id,
        command: HdcCommand::ForwardCheck,
        payload: check_payload,
    };
    info!("rport listener asking host to check local={local_spec}, listener_id={listener_id}");
    if send_shared(&wr, &check_msg).await.is_err() {
        cleanup_rport_listener(channel_id).await;
        return;
    }

    // 2. Wait for the host's ForwardCheckResult.
    let mut ok = false;
    while let Some(msg) = rx.recv().await {
        match msg {
            RportMsg::CheckResult { ctx_id, ok: result } if ctx_id == listener_id => {
                info!("rport listener got CheckResult ok={result} for listener_id={listener_id}");
                ok = result;
                break;
            }
            RportMsg::Shutdown => {
                cleanup_rport_listener(channel_id).await;
                return;
            }
            _ => {}
        }
    }
    if !ok {
        warn!("rport listener check failed for local={local_spec}, closing");
        cleanup_rport_listener(channel_id).await;
        return;
    }

    // 3. Confirm to the host so it can add the entry to its list.
    let success_payload = format!("0|{} {}", remote_spec, local_spec).into_bytes();
    let _ = send_shared(
        &wr,
        &TaskMessage {
            channel_id,
            command: HdcCommand::ForwardSuccess,
            payload: success_payload,
        },
    )
    .await;

    // 4. Run the accept loop and a dispatcher for per-connection messages.
    info!("rport listener ready for {remote_spec}, waiting for connections");
    let conns: Arc<Mutex<HashMap<u32, mpsc::UnboundedSender<RportConnMsg>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let conns_disp = conns.clone();
    let mut dispatcher = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            match msg {
                RportMsg::Shutdown => break,
                RportMsg::ActiveMaster { conn_id } => {
                    info!("rport dispatcher: ForwardActiveMaster for conn_id={conn_id}");
                    if let Some(tx) = conns_disp.lock().await.get(&conn_id) {
                        let _ = tx.send(RportConnMsg::ActiveMaster);
                    }
                }
                RportMsg::Data { conn_id, data } => {
                    if let Some(tx) = conns_disp.lock().await.get(&conn_id) {
                        let _ = tx.send(RportConnMsg::Data(data));
                    }
                }
                RportMsg::FreeContext { conn_id } => {
                    conns_disp.lock().await.remove(&conn_id);
                }
                _ => {}
            }
        }
        info!("rport dispatcher ended for channel_id={channel_id}");
    });

    loop {
        tokio::select! {
            _ = &mut dispatcher => {
                info!("rport dispatcher ended for channel_id={channel_id}");
                break;
            }
            accept_res = accept_listener(&listener) => {
                match accept_res {
                    Ok(stream) => {
                        let conn_id = rand::random::<u32>();
                        info!("rport accepted connection, conn_id={conn_id}");
                        let (tx, rx) = mpsc::unbounded_channel();
                        conns.lock().await.insert(conn_id, tx);
                        let ls = local_spec.clone();
                        let wr_conn = wr.clone();
                        let conns_clone = conns.clone();
                        tokio::spawn(run_rport_connection(
                            conn_id, stream, ls, channel_id, wr_conn, rx, conns_clone,
                        ));
                    }
                    Err(e) => {
                        warn!("rport accept failed for {remote_spec}: {e}");
                        break;
                    }
                }
            }
        }
    }

    // Shut down all connections and clean up the listener registration.
    {
        let mut guard = conns.lock().await;
        for (_, tx) in guard.drain() {
            let _ = tx.send(RportConnMsg::Shutdown);
        }
    }
    dispatcher.abort();
    cleanup_rport_listener(channel_id).await;
}

async fn run_rport_connection(
    conn_id: u32,
    stream: LocalStream,
    local_spec: String,
    channel_id: u32,
    wr: SharedWriter,
    mut rx: mpsc::UnboundedReceiver<RportConnMsg>,
    conns: Arc<Mutex<HashMap<u32, mpsc::UnboundedSender<RportConnMsg>>>>,
) {
    info!("rport conn_id={conn_id}: sending ForwardActiveSlave");
    // 1. Tell the host to activate this connection by connecting to the local spec.
    let mut payload = vec![0u8; 4 + 8 + local_spec.len() + 1];
    payload[..4].copy_from_slice(&conn_id.to_be_bytes());
    payload[12..12 + local_spec.len()].copy_from_slice(local_spec.as_bytes());
    let active_slave = TaskMessage {
        channel_id,
        command: HdcCommand::ForwardActiveSlave,
        payload,
    };
    if send_shared(&wr, &active_slave).await.is_err() {
        warn!("rport conn_id={conn_id}: failed to send ForwardActiveSlave");
        conns.lock().await.remove(&conn_id);
        return;
    }

    // 2. Wait for the host's ForwardActiveMaster before reading from the accepted socket.
    let activated = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(msg) = rx.recv().await {
            match msg {
                RportConnMsg::ActiveMaster => return true,
                RportConnMsg::Shutdown => return false,
                _ => {}
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    if !activated {
        warn!("rport conn_id={conn_id}: did not receive ForwardActiveMaster");
        conns.lock().await.remove(&conn_id);
        let free_msg = TaskMessage {
            channel_id,
            command: HdcCommand::ForwardFreeContext,
            payload: conn_id.to_be_bytes().to_vec(),
        };
        let _ = send_shared(&wr, &free_msg).await;
        return;
    }
    info!("rport conn_id={conn_id}: ForwardActiveMaster received, bridging");

    let (mut local_rd, mut local_wr) = stream.into_split();

    let wr_send = wr.clone();
    let conn_id_send = conn_id;
    let to_host = tokio::spawn(async move {
        let mut buf = vec![0u8; 4096];
        loop {
            match local_rd.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let mut payload = vec![0u8; 4 + n];
                    payload[..4].copy_from_slice(&conn_id_send.to_be_bytes());
                    payload[4..].copy_from_slice(&buf[..n]);
                    let msg = TaskMessage {
                        channel_id,
                        command: HdcCommand::ForwardData,
                        payload,
                    };
                    if send_shared(&wr_send, &msg).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let to_local = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            match msg {
                RportConnMsg::Data(data) => {
                    if local_wr.write_all(&data).await.is_err() {
                        break;
                    }
                }
                RportConnMsg::Shutdown => break,
                _ => {}
            }
        }
        let _ = local_wr.shutdown().await;
    });

    let _ = tokio::join!(to_host, to_local);
    conns.lock().await.remove(&conn_id);
    let free_msg = TaskMessage {
        channel_id,
        command: HdcCommand::ForwardFreeContext,
        payload: conn_id.to_be_bytes().to_vec(),
    };
    let _ = send_shared(&wr, &free_msg).await;
}

async fn cleanup_rport_listener(channel_id: u32) {
    let state = get_state();
    let mut guard = state.inner.lock().await;
    guard.rport_listeners.remove(&channel_id);
}

// ============================================================================
// Local listener/stream helpers
// ============================================================================

enum LocalListener {
    Tcp(tokio::net::TcpListener),
    #[cfg(unix)]
    Unix(tokio::net::UnixListener),
}

enum LocalStream {
    Tcp(tokio::net::TcpStream),
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
}

enum LocalReadHalf {
    Tcp(tokio::net::tcp::OwnedReadHalf),
    #[cfg(unix)]
    Unix(tokio::net::unix::OwnedReadHalf),
}

enum LocalWriteHalf {
    Tcp(tokio::net::tcp::OwnedWriteHalf),
    #[cfg(unix)]
    Unix(tokio::net::unix::OwnedWriteHalf),
}

impl LocalStream {
    fn into_split(self) -> (LocalReadHalf, LocalWriteHalf) {
        match self {
            LocalStream::Tcp(s) => {
                let (r, w) = s.into_split();
                (LocalReadHalf::Tcp(r), LocalWriteHalf::Tcp(w))
            }
            #[cfg(unix)]
            LocalStream::Unix(s) => {
                let (r, w) = s.into_split();
                (LocalReadHalf::Unix(r), LocalWriteHalf::Unix(w))
            }
        }
    }
}

impl LocalReadHalf {
    async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            LocalReadHalf::Tcp(r) => r.read(buf).await,
            #[cfg(unix)]
            LocalReadHalf::Unix(r) => r.read(buf).await,
        }
    }
}

impl LocalWriteHalf {
    async fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        match self {
            LocalWriteHalf::Tcp(w) => w.write_all(buf).await,
            #[cfg(unix)]
            LocalWriteHalf::Unix(w) => w.write_all(buf).await,
        }
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        match self {
            LocalWriteHalf::Tcp(w) => w.shutdown().await,
            #[cfg(unix)]
            LocalWriteHalf::Unix(w) => w.shutdown().await,
        }
    }
}

async fn start_listener(spec: &str) -> io::Result<LocalListener> {
    let (proto, target) = spec.split_once(':').ok_or_else(|| {
        Error::new(ErrorKind::InvalidInput, format!("Invalid forward spec: {spec}"))
    })?;
    match proto {
        "tcp" => {
            let port: u16 = target.parse().map_err(|e| {
                Error::new(ErrorKind::InvalidInput, format!("Invalid port: {e}"))
            })?;
            let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await?;
            Ok(LocalListener::Tcp(listener))
        }
        #[cfg(unix)]
        "localabstract" => {
            let std_listener = listen_abstract_socket(target)?;
            let listener = tokio::net::UnixListener::from_std(std_listener)?;
            Ok(LocalListener::Unix(listener))
        }
        #[cfg(unix)]
        "localfilesystem" => {
            let listener = tokio::net::UnixListener::bind(target)?;
            Ok(LocalListener::Unix(listener))
        }
        #[cfg(unix)]
        "localreserved" => {
            let path = format!("/dev/unix/socket/{target}");
            let _ = std::fs::remove_file(&path);
            let listener = tokio::net::UnixListener::bind(&path)?;
            Ok(LocalListener::Unix(listener))
        }
        #[cfg(not(unix))]
        "localabstract" | "localfilesystem" | "localreserved" => Err(Error::new(
            ErrorKind::Unsupported,
            "Unix-domain forward specs are not supported on this platform",
        )),
        _ => Err(Error::new(
            ErrorKind::InvalidInput,
            format!("Unsupported rport listener proto: {proto}"),
        )),
    }
}

async fn accept_listener(listener: &LocalListener) -> io::Result<LocalStream> {
    match listener {
        LocalListener::Tcp(l) => {
            let (s, _) = l.accept().await?;
            Ok(LocalStream::Tcp(s))
        }
        #[cfg(unix)]
        LocalListener::Unix(l) => {
            let (s, _) = l.accept().await?;
            Ok(LocalStream::Unix(s))
        }
    }
}

#[cfg(unix)]
fn listen_abstract_socket(name: &str) -> io::Result<std::os::unix::net::UnixListener> {
    use std::os::unix::io::FromRawFd;

    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }

    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = name.as_bytes();
    let max_len = addr.sun_path.len().saturating_sub(1);
    if bytes.len() > max_len {
        unsafe { libc::close(fd) };
        return Err(Error::new(ErrorKind::InvalidInput, "abstract socket name too long"));
    }
    unsafe {
        let path_ptr = addr.sun_path.as_mut_ptr();
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr() as *const libc::c_char,
            path_ptr.add(1) as *mut libc::c_char,
            bytes.len(),
        );
    }

    let addr_len = std::mem::size_of::<libc::sa_family_t>() + 1 + bytes.len();
    let ret = unsafe {
        libc::bind(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            addr_len as libc::socklen_t,
        )
    };
    if ret < 0 {
        let e = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(e);
    }

    let ret = unsafe { libc::listen(fd, 128) };
    if ret < 0 {
        let e = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(e);
    }

    let std_listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(fd) };
    std_listener.set_nonblocking(true)?;
    Ok(std_listener)
}

// ============================================================================
// Shared helpers
// ============================================================================

async fn echo_client(
    wr: &SharedWriter,
    channel_id: u32,
    msg: &str,
    level: MessageLevel,
) -> io::Result<()> {
    let prefix = match level {
        MessageLevel::Fail => "[Fail]",
        MessageLevel::Info => "[Info]",
        MessageLevel::Ok => "",
    };
    let data = format!("{}{}\r\n", prefix, msg);
    let response = TaskMessage {
        channel_id,
        command: HdcCommand::KernelEcho,
        payload: [vec![level as u8], data.into_bytes()].concat(),
    };
    send_shared(&wr, &response).await
}

fn parse_null_terminated(data: &[u8]) -> String {
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    String::from_utf8_lossy(&data[..end]).to_string()
}

async fn check_remote_reachable(spec: &str) -> io::Result<()> {
    let (proto, target) = spec.split_once(':').ok_or_else(|| {
        Error::new(ErrorKind::InvalidInput, format!("Invalid remote spec: {spec}"))
    })?;
    match proto {
        "tcp" => {
            let port: u16 = target.parse().map_err(|e| {
                Error::new(ErrorKind::InvalidInput, format!("Invalid port: {e}"))
            })?;
            let _ = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}")).await?;
            Ok(())
        }
        #[cfg(unix)]
        "localabstract" => {
            let _ = connect_abstract_socket(target).await?;
            Ok(())
        }
        #[cfg(not(unix))]
        "localabstract" => Err(Error::new(
            ErrorKind::Unsupported,
            "localabstract not supported on this platform",
        )),
        _ => Err(Error::new(
            ErrorKind::InvalidInput,
            format!("Unsupported forward proto: {proto}"),
        )),
    }
}

enum RemoteStream {
    Tcp(tokio::net::TcpStream),
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
}

impl RemoteStream {
    fn into_split(self) -> (RemoteReadHalf, RemoteWriteHalf) {
        match self {
            RemoteStream::Tcp(s) => {
                let (r, w) = s.into_split();
                (RemoteReadHalf::Tcp(r), RemoteWriteHalf::Tcp(w))
            }
            #[cfg(unix)]
            RemoteStream::Unix(s) => {
                let (r, w) = s.into_split();
                (RemoteReadHalf::Unix(r), RemoteWriteHalf::Unix(w))
            }
        }
    }
}

enum RemoteReadHalf {
    Tcp(tokio::net::tcp::OwnedReadHalf),
    #[cfg(unix)]
    Unix(tokio::net::unix::OwnedReadHalf),
}

enum RemoteWriteHalf {
    Tcp(tokio::net::tcp::OwnedWriteHalf),
    #[cfg(unix)]
    Unix(tokio::net::unix::OwnedWriteHalf),
}

impl RemoteReadHalf {
    async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            RemoteReadHalf::Tcp(r) => r.read(buf).await,
            #[cfg(unix)]
            RemoteReadHalf::Unix(r) => r.read(buf).await,
        }
    }
}

impl RemoteWriteHalf {
    async fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        match self {
            RemoteWriteHalf::Tcp(w) => w.write_all(buf).await,
            #[cfg(unix)]
            RemoteWriteHalf::Unix(w) => w.write_all(buf).await,
        }
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        match self {
            RemoteWriteHalf::Tcp(w) => w.shutdown().await,
            #[cfg(unix)]
            RemoteWriteHalf::Unix(w) => w.shutdown().await,
        }
    }
}

async fn connect_remote(spec: &str) -> io::Result<RemoteStream> {
    let (proto, target) = spec.split_once(':').ok_or_else(|| {
        Error::new(ErrorKind::InvalidInput, format!("Invalid remote spec: {spec}"))
    })?;
    match proto {
        "tcp" => {
            let port: u16 = target.parse().map_err(|e| {
                Error::new(ErrorKind::InvalidInput, format!("Invalid port: {e}"))
            })?;
            let stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}")).await?;
            Ok(RemoteStream::Tcp(stream))
        }
        #[cfg(unix)]
        "localabstract" => {
            let stream = connect_abstract_socket(target).await?;
            Ok(RemoteStream::Unix(stream))
        }
        #[cfg(not(unix))]
        "localabstract" => Err(Error::new(
            ErrorKind::Unsupported,
            "localabstract not supported on this platform",
        )),
        _ => Err(Error::new(
            ErrorKind::InvalidInput,
            format!("Unsupported forward proto: {proto}"),
        )),
    }
}

#[cfg(unix)]
async fn connect_abstract_socket(name: &str) -> io::Result<tokio::net::UnixStream> {
    use std::os::unix::io::FromRawFd;

    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }

    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = name.as_bytes();
    let max_len = addr.sun_path.len().saturating_sub(1);
    if bytes.len() > max_len {
        return Err(Error::new(ErrorKind::InvalidInput, "abstract socket name too long"));
    }
    unsafe {
        let path_ptr = addr.sun_path.as_mut_ptr();
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr() as *const libc::c_char,
            path_ptr.add(1) as *mut libc::c_char,
            bytes.len(),
        );
    }

    let addr_len = std::mem::size_of::<libc::sa_family_t>() + 1 + bytes.len();
    let ret = unsafe {
        libc::connect(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            addr_len as libc::socklen_t,
        )
    };
    if ret < 0 {
        let e = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(e);
    }

    let std_stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
    std_stream.set_nonblocking(true)?;
    tokio::net::UnixStream::from_std(std_stream)
}
