//! App installation handler for HDC daemon.

use hdc_protocol::config::{HdcCommand, TaskMessage, MessageLevel, INSTALL_TMP_DIR};
use std::io::{self, Error, ErrorKind, Write};
use std::sync::{Arc, Mutex};
use tracing::{error, info, warn};

use crate::file::FileTaskMap;
use crate::task::{send_shared, SharedWriter};

pub async fn handle_app_task(
    msg: TaskMessage,
    session_id: u32,
    wr: SharedWriter,
    file_map: &FileTaskMap,
) -> io::Result<()> {
    match msg.command {
        HdcCommand::AppInit => handle_app_init(msg, session_id, wr, file_map).await,
        HdcCommand::AppCheck => handle_app_check(msg, session_id, wr, file_map).await,
        HdcCommand::AppBegin => handle_app_begin(msg, session_id, wr, file_map).await,
        HdcCommand::AppData => handle_app_data(msg, session_id, wr, file_map).await,
        HdcCommand::AppFinish => handle_app_finish(msg, session_id, wr, file_map).await,
        HdcCommand::AppUninstall => handle_app_uninstall(msg, wr).await,
        _ => {
            warn!("unhandled app command: {:?}", msg.command);
            Ok(())
        }
    }
}

fn parse_app_path(payload: &[u8]) -> Option<String> {
    let cmd_str = String::from_utf8_lossy(payload);
    let parts: Vec<&str> = cmd_str.split(' ').collect();
    if parts.len() < 2 {
        return None;
    }
    let mut idx = 1; // skip "install"
    if parts.len() > idx + 1 && parts[idx] == "-cwd" {
        idx += 2;
    }
    parts.get(idx).map(|s| s.to_string())
}

async fn handle_app_init(
    msg: TaskMessage,
    session_id: u32,
    wr: SharedWriter,
    file_map: &FileTaskMap,
) -> io::Result<()> {
    let app_path = match parse_app_path(&msg.payload) {
        Some(p) => p,
        None => {
            echo_client(msg.channel_id, &wr, "Missing app path", MessageLevel::Fail).await?;
            return Ok(());
        }
    };

    let file_name = std::path::Path::new(&app_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("app.hap");

    let tmp_path = format!("{}{}", INSTALL_TMP_DIR, file_name);
    info!("app init: will install to {tmp_path}");

    // Reuse file transfer state but mark it as app install
    let state = crate::file::FileTransferState {
        session_id,
        channel_id: msg.channel_id,
        local_path: app_path,
        remote_path: tmp_path.clone(),
        file: Arc::new(Mutex::new(None)),
        total_size: 0,
        received_size: 0,
        direction: crate::file::TransferDirection::Send,
    };
    file_map.put(session_id, msg.channel_id, state);

    // Send AppCheck
    let response = TaskMessage {
        channel_id: msg.channel_id,
        command: HdcCommand::AppCheck,
        payload: tmp_path.into_bytes(),
    };
    send_shared(&wr, &response).await
}

async fn handle_app_check(
    msg: TaskMessage,
    _session_id: u32,
    wr: SharedWriter,
    _file_map: &FileTaskMap,
) -> io::Result<()> {
    let response = TaskMessage {
        channel_id: msg.channel_id,
        command: HdcCommand::AppBegin,
        payload: vec![],
    };
    send_shared(&wr, &response).await
}

async fn handle_app_begin(
    msg: TaskMessage,
    session_id: u32,
    wr: SharedWriter,
    file_map: &FileTaskMap,
) -> io::Result<()> {
    let mut state = match file_map.get(session_id, msg.channel_id) {
        Some(s) => s,
        None => {
            echo_client(msg.channel_id, &wr, "App install not initialized", MessageLevel::Fail).await?;
            return Ok(());
        }
    };

    match std::fs::File::create(&state.remote_path) {
        Ok(file) => {
            *state.file.lock().unwrap() = Some(file);
            file_map.put(session_id, msg.channel_id, state);
        }
        Err(e) => {
            error!("failed to create temp file: {e}");
            echo_client(msg.channel_id, &wr, &format!("Failed to create temp file: {e}"), MessageLevel::Fail).await?;
        }
    }

    Ok(())
}

async fn handle_app_data(
    msg: TaskMessage,
    session_id: u32,
    wr: SharedWriter,
    file_map: &FileTaskMap,
) -> io::Result<()> {
    let mut state = match file_map.get(session_id, msg.channel_id) {
        Some(s) => s,
        None => {
            echo_client(msg.channel_id, &wr, "App install not initialized", MessageLevel::Fail).await?;
            return Ok(());
        }
    };

    let mut write_failed = None;
    if let Ok(mut file_guard) = state.file.lock() {
        if let Some(ref mut file) = *file_guard {
            if let Err(e) = file.write_all(&msg.payload) {
                error!("failed to write app data: {e}");
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

async fn handle_app_finish(
    msg: TaskMessage,
    session_id: u32,
    wr: SharedWriter,
    file_map: &FileTaskMap,
) -> io::Result<()> {
    let state = match file_map.get(session_id, msg.channel_id) {
        Some(s) => s,
        None => {
            echo_client(msg.channel_id, &wr, "App install not initialized", MessageLevel::Fail).await?;
            return Ok(());
        }
    };

    file_map.remove(session_id, msg.channel_id);
    info!("app install: file received at {}", state.remote_path);

    // Call install command
    #[cfg(target_os = "linux")]
    {
        let result = std::process::Command::new("bm")
            .args(["install", "-p", &state.remote_path])
            .output();

        match result {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !stdout.is_empty() {
                    echo_client(msg.channel_id, &wr, &stdout, MessageLevel::Info).await?;
                }
                if !stderr.is_empty() {
                    echo_client(msg.channel_id, &wr, &stderr, MessageLevel::Fail).await?;
                }
                if output.status.success() {
                    echo_client(msg.channel_id, &wr, "AppMod finish", MessageLevel::Ok).await?;
                } else {
                    echo_client(msg.channel_id, &wr, "Install failed", MessageLevel::Fail).await?;
                }
            }
            Err(e) => {
                echo_client(msg.channel_id, &wr, &format!("Install command failed: {e}"), MessageLevel::Fail).await?;
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        echo_client(msg.channel_id, &wr, "App install not supported on this platform", MessageLevel::Fail).await?;
    }

    let close = TaskMessage {
        channel_id: msg.channel_id,
        command: HdcCommand::KernelChannelClose,
        payload: vec![0],
    };
    send_shared(&wr, &close).await
}

async fn handle_app_uninstall(
    msg: TaskMessage,
    wr: SharedWriter,
) -> io::Result<()> {
    let package = String::from_utf8_lossy(&msg.payload);
    let package = package.trim();

    #[cfg(target_os = "linux")]
    {
        let result = std::process::Command::new("bm")
            .args(["uninstall", "-n", package])
            .output();

        match result {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !stdout.is_empty() {
                    echo_client(msg.channel_id, &wr, &stdout, MessageLevel::Info).await?;
                }
                if !stderr.is_empty() {
                    echo_client(msg.channel_id, &wr, &stderr, MessageLevel::Fail).await?;
                }
                if output.status.success() {
                    echo_client(msg.channel_id, &wr, "AppMod finish", MessageLevel::Ok).await?;
                } else {
                    echo_client(msg.channel_id, &wr, "Uninstall failed", MessageLevel::Fail).await?;
                }
            }
            Err(e) => {
                echo_client(msg.channel_id, &wr, &format!("Uninstall command failed: {e}"), MessageLevel::Fail).await?;
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        echo_client(msg.channel_id, &wr, "App uninstall not supported on this platform", MessageLevel::Fail).await?;
    }

    let close = TaskMessage {
        channel_id: msg.channel_id,
        command: HdcCommand::KernelChannelClose,
        payload: vec![0],
    };
    send_shared(&wr, &close).await
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
