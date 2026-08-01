//! File transfer handler for HDC daemon.

use hdc_protocol::config::{HdcCommand, TaskMessage, MessageLevel, MAX_SIZE_IOBUF};
use hdc_protocol::serializer::{TransferConfig, HdcSerialize, HdcDeserialize};
use std::collections::HashMap;
use std::io::{self, Error, ErrorKind, Read, Write};
use std::sync::{Arc, Mutex};
use tracing::{debug, error, info, warn};

use crate::task::{send_shared, SharedWriter};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    Send, // Host → Daemon (daemon receives file)
    Recv, // Daemon → Host (daemon sends file)
}

#[derive(Debug)]
pub struct FileTransferState {
    pub session_id: u32,
    pub channel_id: u32,
    pub local_path: String,
    pub remote_path: String,
    pub file: Arc<Mutex<Option<std::fs::File>>>,
    pub total_size: u64,
    pub received_size: u64,
    pub direction: TransferDirection,
}

#[derive(Default)]
struct FileTaskMapInner {
    tasks: HashMap<(u32, u32), FileTransferState>,
}

#[derive(Clone)]
pub struct FileTaskMap {
    inner: Arc<std::sync::Mutex<FileTaskMapInner>>,
}

impl FileTaskMap {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(std::sync::Mutex::new(FileTaskMapInner::default())),
        }
    }

    pub fn put(&self, session_id: u32, channel_id: u32, state: FileTransferState) {
        let mut guard = self.inner.lock().unwrap();
        guard.tasks.insert((session_id, channel_id), state);
    }

    pub fn get(&self, session_id: u32, channel_id: u32) -> Option<FileTransferState> {
        let guard = self.inner.lock().unwrap();
        guard.tasks.get(&(session_id, channel_id)).cloned()
    }

    pub fn remove(&self, session_id: u32, channel_id: u32) {
        let mut guard = self.inner.lock().unwrap();
        guard.tasks.remove(&(session_id, channel_id));
    }
}

impl Clone for FileTransferState {
    fn clone(&self) -> Self {
        Self {
            session_id: self.session_id,
            channel_id: self.channel_id,
            local_path: self.local_path.clone(),
            remote_path: self.remote_path.clone(),
            file: self.file.clone(),
            total_size: self.total_size,
            received_size: self.received_size,
            direction: self.direction,
        }
    }
}

pub async fn handle_file_task(
    msg: TaskMessage,
    session_id: u32,
    wr: SharedWriter,
    file_map: &FileTaskMap,
) -> io::Result<()> {
    match msg.command {
        HdcCommand::FileInit => {
            handle_file_init(msg, session_id, wr, file_map).await?;
        }
        HdcCommand::FileCheck => {
            handle_file_check(msg, session_id, wr, file_map).await?;
        }
        HdcCommand::FileBegin => {
            handle_file_begin(msg, session_id, wr, file_map).await?;
        }
        HdcCommand::FileData => {
            handle_file_data(msg, session_id, wr, file_map).await?;
        }
        HdcCommand::FileFinish => {
            handle_file_finish(msg, session_id, wr, file_map).await?;
        }
        HdcCommand::FileRecvInit => {
            handle_file_recv_init(msg, session_id, wr, file_map).await?;
        }
        _ => {
            warn!("unhandled file command: {:?}", msg.command);
        }
    }
    Ok(())
}

/// Parse file paths from command string.
/// Format: "file send|recv [-cwd <cwd>] <path1> <path2>"
fn parse_file_paths(payload: &[u8]) -> Option<(String, String)> {
    let cmd_str = String::from_utf8_lossy(payload);
    let parts: Vec<&str> = cmd_str.split(' ').collect();
    if parts.len() < 4 {
        return None;
    }

    let mut idx = 2; // skip "file" and "send|recv"
    if parts.len() > idx && parts[idx] == "-cwd" {
        idx += 2; // skip "-cwd" and cwd value
    }

    if parts.len() >= idx + 2 {
        Some((parts[idx].to_string(), parts[idx + 1].to_string()))
    } else {
        None
    }
}

async fn handle_file_init(
    msg: TaskMessage,
    session_id: u32,
    wr: SharedWriter,
    file_map: &FileTaskMap,
) -> io::Result<()> {
    let Some((local_path, remote_path)) = parse_file_paths(&msg.payload) else {
        echo_client(msg.channel_id, &wr, "Invalid file init parameters", MessageLevel::Fail).await?;
        return Ok(());
    };

    info!("file init: local={local_path}, remote={remote_path}");

    let state = FileTransferState {
        session_id,
        channel_id: msg.channel_id,
        local_path,
        remote_path: remote_path.clone(),
        file: Arc::new(Mutex::new(None)),
        total_size: 0,
        received_size: 0,
        direction: TransferDirection::Send,
    };
    file_map.put(session_id, msg.channel_id, state);

    // Send FileCheck response with transfer config
    let config = TransferConfig {
        path: remote_path,
        ..Default::default()
    };
    let response = TaskMessage {
        channel_id: msg.channel_id,
        command: HdcCommand::FileCheck,
        payload: config.serialize(),
    };
    send_shared(&wr, &response).await?;

    Ok(())
}

async fn handle_file_check(
    msg: TaskMessage,
    session_id: u32,
    wr: SharedWriter,
    file_map: &FileTaskMap,
) -> io::Result<()> {
    let config = match TransferConfig::deserialize(&msg.payload) {
        Ok(c) => c,
        Err(e) => {
            error!("file check deserialize failed: {e}");
            echo_client(msg.channel_id, &wr, "Invalid transfer config", MessageLevel::Fail).await?;
            return Ok(());
        }
    };

    info!("file check: path={}, size={}", config.path, config.file_size);

    if let Some(mut state) = file_map.get(session_id, msg.channel_id) {
        state.total_size = config.file_size;
        state.remote_path = config.path.clone();
        file_map.put(session_id, msg.channel_id, state);
    }

    // Send FileBegin
    let response = TaskMessage {
        channel_id: msg.channel_id,
        command: HdcCommand::FileBegin,
        payload: vec![],
    };
    send_shared(&wr, &response).await?;

    Ok(())
}

async fn handle_file_begin(
    msg: TaskMessage,
    session_id: u32,
    wr: SharedWriter,
    file_map: &FileTaskMap,
) -> io::Result<()> {
    let mut state = match file_map.get(session_id, msg.channel_id) {
        Some(s) => s,
        None => {
            echo_client(msg.channel_id, &wr, "File transfer not initialized", MessageLevel::Fail).await?;
            return Ok(());
        }
    };

    match state.direction {
        TransferDirection::Send => {
            // Daemon receives file: create/truncate target file
            let remote_path = state.remote_path.clone();
            match std::fs::File::create(&remote_path) {
                Ok(file) => {
                    *state.file.lock().unwrap() = Some(file);
                    file_map.put(session_id, msg.channel_id, state);
                    info!("file begin: created {}", remote_path);
                }
                Err(e) => {
                    error!("failed to create file {}: {e}", remote_path);
                    echo_client(msg.channel_id, &wr, &format!("Failed to create file: {e}"), MessageLevel::Fail).await?;
                }
            }
        }
        TransferDirection::Recv => {
            // Daemon sends file: open and read, send FileData
            handle_daemon_file_send(msg.channel_id, session_id, wr, file_map, &state.remote_path).await?;
        }
    }

    Ok(())
}

async fn handle_daemon_file_send(
    channel_id: u32,
    session_id: u32,
    wr: SharedWriter,
    file_map: &FileTaskMap,
    path: &str,
) -> io::Result<()> {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            error!("failed to open file {path}: {e}");
            echo_client(channel_id, &wr, &format!("Failed to open file: {e}"), MessageLevel::Fail).await?;
            let close = TaskMessage {
                channel_id,
                command: HdcCommand::KernelChannelClose,
                payload: vec![0],
            };
            return send_shared(&wr, &close).await;
        }
    };

    let mut buf = vec![0u8; MAX_SIZE_IOBUF];
    let mut sent = 0u64;
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let file_data = TaskMessage {
                    channel_id,
                    command: HdcCommand::FileData,
                    payload: buf[..n].to_vec(),
                };
                if let Err(e) = send_shared(&wr, &file_data).await {
                    error!("failed to send file data: {e}");
                    return Ok(());
                }
                sent += n as u64;
            }
            Err(e) => {
                error!("failed to read file: {e}");
                echo_client(channel_id, &wr, &format!("Read failed: {e}"), MessageLevel::Fail).await?;
                return Ok(());
            }
        }
    }

    info!("file send complete: {path}, {sent} bytes");

    // Send FileFinish
    let file_finish = TaskMessage {
        channel_id,
        command: HdcCommand::FileFinish,
        payload: vec![],
    };
    send_shared(&wr, &file_finish).await?;

    // Host side will output FileTransfer finish message; do not echo from daemon

    // Close channel
    let close = TaskMessage {
        channel_id,
        command: HdcCommand::KernelChannelClose,
        payload: vec![0],
    };
    send_shared(&wr, &close).await?;

    file_map.remove(session_id, channel_id);
    Ok(())
}

async fn handle_file_data(
    msg: TaskMessage,
    session_id: u32,
    wr: SharedWriter,
    file_map: &FileTaskMap,
) -> io::Result<()> {
    let mut state = match file_map.get(session_id, msg.channel_id) {
        Some(s) => s,
        None => {
            echo_client(msg.channel_id, &wr, "File transfer not initialized", MessageLevel::Fail).await?;
            return Ok(());
        }
    };

    if state.direction != TransferDirection::Send {
        // FileData should not be received in Recv mode
        return Ok(());
    }

    let mut write_failed = None;
    if let Ok(mut file_guard) = state.file.lock() {
        if let Some(ref mut file) = *file_guard {
            if let Err(e) = file.write_all(&msg.payload) {
                error!("failed to write file: {e}");
                write_failed = Some(format!("Write failed: {e}"));
            } else {
                state.received_size += msg.payload.len() as u64;
            }
        }
    }

    if let Some(err) = write_failed {
        echo_client(msg.channel_id, &wr, &err, MessageLevel::Fail).await?;
        return Ok(());
    }

    file_map.put(session_id, msg.channel_id, state);
    Ok(())
}

async fn handle_file_finish(
    msg: TaskMessage,
    session_id: u32,
    wr: SharedWriter,
    file_map: &FileTaskMap,
) -> io::Result<()> {
    let state = match file_map.get(session_id, msg.channel_id) {
        Some(s) => s,
        None => {
            echo_client(msg.channel_id, &wr, "File transfer not initialized", MessageLevel::Fail).await?;
            return Ok(());
        }
    };

    info!(
        "file finish: received {}/{} bytes for {}",
        state.received_size, state.total_size, state.remote_path
    );

    file_map.remove(session_id, msg.channel_id);

    // Host side will output FileTransfer finish message; do not echo from daemon

    // Close channel
    let response = TaskMessage {
        channel_id: msg.channel_id,
        command: HdcCommand::KernelChannelClose,
        payload: vec![0],
    };
    send_shared(&wr, &response).await?;

    Ok(())
}

async fn handle_file_recv_init(
    msg: TaskMessage,
    session_id: u32,
    wr: SharedWriter,
    file_map: &FileTaskMap,
) -> io::Result<()> {
    let Some((remote_path, _local_path)) = parse_file_paths(&msg.payload) else {
        echo_client(msg.channel_id, &wr, "Invalid file recv parameters", MessageLevel::Fail).await?;
        return Ok(());
    };

    info!("file recv init: remote={remote_path}");

    // Check if file exists and get size
    let metadata = match std::fs::metadata(&remote_path) {
        Ok(m) => m,
        Err(e) => {
            echo_client(msg.channel_id, &wr, &format!("File not found: {e}"), MessageLevel::Fail).await?;
            let close = TaskMessage {
                channel_id: msg.channel_id,
                command: HdcCommand::KernelChannelClose,
                payload: vec![0],
            };
            return send_shared(&wr, &close).await;
        }
    };

    let file_size = metadata.len();

    // Send FileCheck with file info
    let config = TransferConfig {
        path: remote_path.clone(),
        file_size,
        ..Default::default()
    };
    let response = TaskMessage {
        channel_id: msg.channel_id,
        command: HdcCommand::FileCheck,
        payload: config.serialize(),
    };
    send_shared(&wr, &response).await?;

    // Store state for this recv operation
    let state = FileTransferState {
        session_id,
        channel_id: msg.channel_id,
        local_path: remote_path.clone(),
        remote_path: remote_path.clone(),
        file: Arc::new(Mutex::new(None)),
        total_size: file_size,
        received_size: 0,
        direction: TransferDirection::Recv,
    };
    file_map.put(session_id, msg.channel_id, state);

    Ok(())
}

async fn echo_client(
    channel_id: u32,
    wr: &SharedWriter,
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
