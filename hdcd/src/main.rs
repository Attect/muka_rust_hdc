//! HDC Daemon (hdcd)

mod app;
mod file;
mod flashd;
mod forward;
mod shell;
mod task;
mod transfer;

use hdc_protocol::config::{DAEMON_PORT, AuthType, HANDSHAKE_MESSAGE, ENV_ENCRYPT_CHANNEL, AUTH_TYPE_SSL_TLS_PSK, FEATURE_ENCRYPT_TCP, FEATURE_HEARTBEAT, HDC_HOST_DAEMON_BUF_SEPARATOR};
use hdc_protocol::encrypt::PskCipher;
use hdc_protocol::serializer::{HdcDeserialize, HdcSerialize, SessionHandShake};
use rsa::pkcs8::DecodePublicKey;
use base64::Engine;
use std::collections::HashMap;
use std::io::{self, Error, ErrorKind};
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

fn parse_tlv(tlv: &str) -> HashMap<String, String> {
    const TAG_LEN: usize = 16;
    const VAL_LEN: usize = 16;
    const MIN_LEN: usize = TAG_LEN + VAL_LEN;
    let mut map = HashMap::new();
    let mut remaining = tlv;
    while remaining.len() >= MIN_LEN {
        let tag = remaining[..TAG_LEN].trim_end().to_string();
        let val_len_str = remaining[TAG_LEN..MIN_LEN].trim_end();
        let val_len: usize = match val_len_str.parse() {
            Ok(n) => n,
            Err(_) => break,
        };
        if remaining.len() < MIN_LEN + val_len {
            break;
        }
        let val = remaining[MIN_LEN..MIN_LEN + val_len].to_string();
        map.insert(tag, val);
        remaining = &remaining[MIN_LEN + val_len..];
    }
    map
}

fn host_supports_encrypt(buf: &str) -> bool {
    parse_tlv(buf)
        .get("supportfeatures")
        .map(|v| v.split(',').any(|f| f == FEATURE_ENCRYPT_TCP))
        .unwrap_or(false)
}

fn extract_host_pubkey(buf: &str) -> Option<String> {
    parse_tlv(buf).get("pubkey").cloned()
}

fn encrypt_psk_with_pubkey(pubkey_pem: &str, psk: &[u8; 32]) -> io::Result<Vec<u8>> {
    // pubkey_pem is "hostname<0x0C>pubkey_pem"
    let pubkey_pem = pubkey_pem
        .split(HDC_HOST_DAEMON_BUF_SEPARATOR)
        .nth(1)
        .unwrap_or(pubkey_pem);
    let public_key = rsa::RsaPublicKey::from_public_key_pem(pubkey_pem)
        .map_err(|e| Error::new(ErrorKind::InvalidData, format!("Invalid host public key: {e}")))?;
    let mut rng = rand::rngs::OsRng;
    let padding = rsa::Oaep::new_with_mgf_hash::<sha2::Sha256, sha2::Sha256>();
    public_key
        .encrypt(&mut rng, padding, psk)
        .map_err(|e| Error::new(ErrorKind::Other, format!("RSA encrypt PSK failed: {e}")))
}

fn build_daemon_auth_buf() -> String {
    let hostname = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "hdcd-stub".to_string());
    let mut features = vec![FEATURE_HEARTBEAT];
    if std::env::var(ENV_ENCRYPT_CHANNEL).ok() == Some("1".to_string()) {
        features.push(FEATURE_ENCRYPT_TCP);
    }
    let mut buf = String::new();
    buf.push_str(&format!("{:<16}{:<16}{}", "devname", hostname.len(), hostname));
    buf.push_str(&format!("{:<16}{:<16}{}", "supportfeatures", features.join(",").len(), features.join(",")));
    buf
}

#[tokio::main]
async fn main() -> io::Result<()> {
    // Setup tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    if args.len() == 2 && args[1] == "-v" {
        println!("{}", hdc_protocol::config::get_version());
        return Ok(());
    }

    info!("hdcd starting...");

    // Get port from environment or use default
    let port = get_daemon_port();
    let addr = format!("0.0.0.0:{}", port);
    
    let listener = TcpListener::bind(&addr).await?;
    let actual_port = listener.local_addr()?.port();
    info!("daemon binds on {addr}, actual port: {actual_port}");

    // TODO: Set port to system property if on real HarmonyOS
    // For now, just log it
    info!("HDC daemon listening on port {}", actual_port);

    let file_map = file::FileTaskMap::new();

    loop {
        let (stream, peer_addr) = listener.accept().await?;
        info!("accepted connection from {peer_addr}");
        let file_map_clone = file_map.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream, file_map_clone).await {
                error!("client handler error: {e}");
            }
        });
    }
}

async fn handle_client(stream: TcpStream, file_map: crate::file::FileTaskMap) -> io::Result<()> {
    let (mut rd, wr) = stream.into_split();
    let wr = Arc::new(Mutex::new(wr));

    // Read first message (should be handshake)
    let first_msg = transfer::tcp::unpack_task_message(&mut rd, 0).await?;
    info!("first message: channel_id={}, command={:?}, payload_len={}", first_msg.channel_id, first_msg.command, first_msg.payload.len());

    // Parse the host handshake and reply with a matching serialized handshake.
    let (session_id, connect_key, host_hs_buf) = match SessionHandShake::deserialize(&first_msg.payload) {
        Ok(hs) => {
            info!("host handshake: auth_type={}, session_id={}", hs.auth_type, hs.session_id);
            (hs.session_id, hs.connect_key, hs.buf)
        }
        Err(e) => {
            warn!("failed to deserialize host handshake: {e}, using random session id");
            (rand::random::<u32>(), String::new(), String::new())
        }
    };
    info!("new session: {session_id}");
    let shared_wr = crate::task::SharedWriter { wr: wr.clone(), session_id };

    let encrypt_enabled = std::env::var(ENV_ENCRYPT_CHANNEL).ok() == Some("1".to_string())
        && host_supports_encrypt(&host_hs_buf);

    let (response_hs, psk_option) = if encrypt_enabled {
        if let Some(pubkey_info) = extract_host_pubkey(&host_hs_buf) {
            let psk = PskCipher::generate_psk();
            let encrypted_psk = encrypt_psk_with_pubkey(&pubkey_info, &psk)?;
            let psk_response = SessionHandShake {
                banner: HANDSHAKE_MESSAGE.to_string(),
                auth_type: AUTH_TYPE_SSL_TLS_PSK,
                session_id,
                connect_key: connect_key.clone(),
                buf: base64::engine::general_purpose::STANDARD.encode(&encrypted_psk),
                version: format!("{}{}", hdc_protocol::config::get_version(), "47d583e40754ffe6"),
            };
            (psk_response, Some(psk))
        } else {
            warn!("host requested encryption but no pubkey in handshake");
            (SessionHandShake {
                banner: HANDSHAKE_MESSAGE.to_string(),
                auth_type: AuthType::Ok as u8,
                session_id,
                connect_key: connect_key.clone(),
                buf: build_daemon_auth_buf(),
                version: format!("{}{}", hdc_protocol::config::get_version(), "47d583e40754ffe6"),
            }, None)
        }
    } else {
        (SessionHandShake {
            banner: HANDSHAKE_MESSAGE.to_string(),
            auth_type: AuthType::Ok as u8,
            session_id,
            connect_key: connect_key.clone(),
            buf: build_daemon_auth_buf(),
            version: format!("{}{}", hdc_protocol::config::get_version(), "47d583e40754ffe6"),
        }, None)
    };
    let response = hdc_protocol::config::TaskMessage {
        channel_id: first_msg.channel_id,
        command: hdc_protocol::config::HdcCommand::KernelHandshake,
        payload: response_hs.serialize(),
    };
    {
        let mut guard = shared_wr.wr.lock().await;
        transfer::tcp::send_message(&mut *guard, session_id, &response).await?;
    }
    // Enable the session cipher only after the PSK response has been sent in plaintext.
    if let Some(psk) = psk_option {
        transfer::tcp::set_session_cipher(session_id, PskCipher::new(&psk, false));
        info!("daemon encrypted TCP channel enabled for session {session_id}");
    }

    // If encryption is enabled, wait for the host's PSK-ACK before entering the main loop.
    if encrypt_enabled {
        match tokio::time::timeout(std::time::Duration::from_secs(10), transfer::tcp::unpack_task_message(&mut rd, session_id)).await {
            Ok(Ok(msg)) => {
                if msg.command != hdc_protocol::config::HdcCommand::KernelHandshake {
                    warn!("expected PSK-ACK handshake, got {:?}", msg.command);
                    transfer::tcp::remove_session_cipher(session_id);
                    return Err(Error::new(ErrorKind::InvalidData, "PSK-ACK expected"));
                }
                info!("host PSK-ACK received, session {session_id} ready");
            }
            Ok(Err(e)) => {
                transfer::tcp::remove_session_cipher(session_id);
                return Err(e);
            }
            Err(_) => {
                transfer::tcp::remove_session_cipher(session_id);
                return Err(Error::new(ErrorKind::TimedOut, "PSK-ACK timeout"));
            }
        }
    }

    // Main message loop
    loop {
        match transfer::tcp::unpack_task_message(&mut rd, session_id).await {
            Ok(msg) => {
                if msg.command == hdc_protocol::config::HdcCommand::ShellInit {
                    // Hand off to interactive shell bridge
                    let wr_clone = shared_wr.clone();
                    tokio::spawn(async move {
                        shell::run_shell_bridge(rd, wr_clone, msg.channel_id, session_id).await;
                    });
                    return Ok(());
                }
                if let Err(e) = task::dispatch_task(msg, session_id, shared_wr.clone(), &file_map).await {
                    if e.kind() == io::ErrorKind::ConnectionAborted {
                        info!("connection closed by task");
                        break;
                    }
                    warn!("task dispatch error: {e}");
                }
            }
            Err(e) => {
                warn!("unpack task failed: {e}");
                break;
            }
        }
    }

    transfer::tcp::remove_session_cipher(session_id);
    info!("client disconnected, session {session_id}");
    Ok(())
}

fn get_daemon_port() -> u16 {
    // On real HarmonyOS, read from system property persist.hdc.port
    // For testing, check environment variable
    std::env::var("HDC_DAEMON_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DAEMON_PORT)
}
