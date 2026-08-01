//! Daemon task dispatcher.

use hdc_protocol::config::{HdcCommand, TaskMessage, MessageLevel};
use crate::file::FileTaskMap;
use std::io::{self, Error, ErrorKind};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::sync::Arc;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

#[derive(Clone)]
pub struct SharedWriter {
    pub wr: Arc<Mutex<OwnedWriteHalf>>,
    pub session_id: u32,
}

pub async fn send_shared(wr: &SharedWriter, msg: &TaskMessage) -> io::Result<()> {
    let mut guard = wr.wr.lock().await;
    crate::transfer::tcp::send_message(&mut *guard, wr.session_id, msg).await
}

/// Dispatch incoming task messages to appropriate handlers.
pub async fn dispatch_task(
    msg: TaskMessage,
    session_id: u32,
    wr: SharedWriter,
    file_map: &FileTaskMap,
) -> io::Result<()> {
    info!(
        "dispatch task: session_id={session_id}, channel_id={}, command={:?}",
        msg.channel_id, msg.command
    );

    match msg.command {
        HdcCommand::KernelHandshake => {
            // Simple handshake response
            let response = TaskMessage {
                channel_id: msg.channel_id,
                command: HdcCommand::KernelHandshake,
                payload: b"OK".to_vec(),
            };
            let mut guard = wr.wr.lock().await;
            crate::transfer::tcp::send_message(&mut *guard, session_id, &response).await?;
        }
        HdcCommand::UnityExecute | HdcCommand::UnityExecuteEx => {
            handle_shell(msg, session_id, wr).await?;
        }
        HdcCommand::ShellInit => {
            handle_interactive_shell(msg, session_id, wr).await?;
        }
        HdcCommand::ShellData => {
            // Interactive shell data - just echo for now
            debug!("shell data received: {} bytes", msg.payload.len());
        }
        HdcCommand::UnityReboot => {
            handle_reboot(msg, wr).await?;
        }
        HdcCommand::UnityRemount => {
            handle_remount(msg, wr).await?;
        }
        HdcCommand::UnityRunmode => {
            handle_runmode(msg, wr).await?;
        }
        HdcCommand::UnityHilog => {
            handle_hilog(msg, wr).await?;
        }
        HdcCommand::FileInit
        | HdcCommand::FileCheck
        | HdcCommand::FileBegin
        | HdcCommand::FileData
        | HdcCommand::FileFinish
        | HdcCommand::FileRecvInit => {
            crate::file::handle_file_task(msg, session_id, wr, file_map).await?;
        }
        HdcCommand::AppInit
        | HdcCommand::AppCheck
        | HdcCommand::AppBegin
        | HdcCommand::AppData
        | HdcCommand::AppFinish
        | HdcCommand::AppUninstall => {
            crate::app::handle_app_task(msg, session_id, wr, file_map).await?;
        }
        HdcCommand::FlashdCheck
        | HdcCommand::FlashdBegin
        | HdcCommand::FlashdData
        | HdcCommand::FlashdFinish
        | HdcCommand::FlashdErase
        | HdcCommand::FlashdFormat
        | HdcCommand::FlashdProgress => {
            crate::flashd::handle_flashd_task(msg, session_id, wr).await?;
        }
        HdcCommand::ForwardInit
        | HdcCommand::ForwardCheck
        | HdcCommand::ForwardCheckResult
        | HdcCommand::ForwardActiveSlave
        | HdcCommand::ForwardActiveMaster
        | HdcCommand::ForwardData
        | HdcCommand::ForwardFreeContext
        | HdcCommand::ForwardList
        | HdcCommand::ForwardRemove
        | HdcCommand::ForwardSuccess
        | HdcCommand::ForwardRportInit
        | HdcCommand::ForwardRportList
        | HdcCommand::ForwardRportRemove => {
            crate::forward::handle_forward_task(msg, session_id, wr).await?;
        }
        HdcCommand::HeartbeatMsg => {
            // Echo heartbeat back to host
            let response = TaskMessage {
                channel_id: msg.channel_id,
                command: HdcCommand::HeartbeatMsg,
                payload: msg.payload,
            };
            send_shared(&wr, &response).await?;
        }
        HdcCommand::KernelChannelClose => {
            // If this channel belongs to a forward/rport task, clean it up locally
            // and keep the session alive. Otherwise echo the close and abort.
            if crate::forward::handle_channel_close(msg.channel_id).await {
                let response = TaskMessage {
                    channel_id: msg.channel_id,
                    command: HdcCommand::KernelChannelClose,
                    payload: vec![0],
                };
                send_shared(&wr, &response).await?;
            } else {
                let response = TaskMessage {
                    channel_id: msg.channel_id,
                    command: HdcCommand::KernelChannelClose,
                    payload: vec![0],
                };
                send_shared(&wr, &response).await?;
                return Err(Error::new(ErrorKind::ConnectionAborted, "channel closed"));
            }
        }
        _ => {
            warn!("unhandled command: {:?}", msg.command);
            echo_client(msg.channel_id, &wr, "Command not implemented", MessageLevel::Fail).await?;
        }
    }
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

fn trim_quotation_for_cmd(cmd_input: String) -> String {
    let mut cmd = cmd_input.trim().to_string();
    if cmd.starts_with('"') && cmd.ends_with('"') {
        cmd = cmd.strip_prefix('"').unwrap_or(&cmd).to_string();
        cmd = cmd.strip_suffix('"').unwrap_or(&cmd).to_string();
    }
    cmd
}

async fn handle_shell(
    msg: TaskMessage,
    _session_id: u32,
    wr: SharedWriter,
) -> io::Result<()> {
    let cmd = String::from_utf8(msg.payload.clone())
        .map_err(|e| Error::new(ErrorKind::InvalidData, format!("invalid utf8: {e}")))?;
    let cmd = trim_quotation_for_cmd(cmd);
    
    debug!("executing shell command: {cmd}");

    #[cfg(target_os = "windows")]
    let output = std::process::Command::new("cmd.exe")
        .args(["/c", &cmd])
        .output();
    
    #[cfg(not(target_os = "windows"))]
    let output = {
        let mut shell_cmd = std::process::Command::new("sh");
        shell_cmd.args(["-c", &cmd]);
        unsafe {
            shell_cmd.pre_exec(|| {
                libc::setsid();
                let pid = libc::getpid();
                libc::setpgid(pid, pid);
                // Set SELinux label: switch from u:r:hdcd:s0 or u:r:updater:s0 to u:r:sh:s0
                // This matches official hdcd behavior (async_cmd.cpp SetSelinuxLabel)
                let path = b"/proc/self/attr/current\0";
                let fd = libc::open(path.as_ptr() as *const libc::c_char, libc::O_RDONLY);
                if fd >= 0 {
                    let mut buf = [0u8; 256];
                    let n = libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len() - 1);
                    libc::close(fd);
                    if n > 0 {
                        let current = String::from_utf8_lossy(&buf[..n as usize]);
                        let current = current.trim_end_matches('\n');
                        if current == "u:r:hdcd:s0" || current == "u:r:updater:s0" {
                            let new_label = b"u:r:sh:s0\0";
                            let fd = libc::open(path.as_ptr() as *const libc::c_char, libc::O_WRONLY);
                            if fd >= 0 {
                                libc::write(fd, new_label.as_ptr() as *const libc::c_void, new_label.len() - 1);
                                libc::close(fd);
                            }
                        }
                    }
                }
                Ok(())
            });
        }
        shell_cmd.output()
    };

    match output {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            
            if !stdout.is_empty() {
                let response = TaskMessage {
                    channel_id: msg.channel_id,
                    command: HdcCommand::KernelEchoRaw,
                    payload: stdout.as_bytes().to_vec(),
                };
                send_shared(&wr, &response).await?;
            }
            if !stderr.is_empty() {
                let response = TaskMessage {
                    channel_id: msg.channel_id,
                    command: HdcCommand::KernelEchoRaw,
                    payload: stderr.as_bytes().to_vec(),
                };
                send_shared(&wr, &response).await?;
            }
            // Close channel
            let response = TaskMessage {
                channel_id: msg.channel_id,
                command: HdcCommand::KernelChannelClose,
                payload: vec![0],
            };
            send_shared(&wr, &response).await?;
        }
        Err(e) => {
            echo_client(msg.channel_id, &wr, &format!("Execute failed: {e}"), MessageLevel::Fail).await?;
            let response = TaskMessage {
                channel_id: msg.channel_id,
                command: HdcCommand::KernelChannelClose,
                payload: vec![0],
            };
            send_shared(&wr, &response).await?;
        }
    }
    Ok(())
}

async fn handle_interactive_shell(
    msg: TaskMessage,
    _session_id: u32,
    wr: SharedWriter,
) -> io::Result<()> {
    // Simplified: just start a shell and send initial prompt
    let response = TaskMessage {
        channel_id: msg.channel_id,
        command: HdcCommand::KernelEchoRaw,
        payload: b"$ ".to_vec(),
    };
    send_shared(&wr, &response).await
}

async fn handle_reboot(
    msg: TaskMessage,
    wr: SharedWriter,
) -> io::Result<()> {
    let payload_str = String::from_utf8_lossy(&msg.payload);
    let boot_mode = payload_str.trim();
    
    #[cfg(target_os = "linux")]
    {
        let result = if boot_mode.is_empty() {
            std::process::Command::new("reboot").output()
        } else {
            // For specific modes, this would need more complex handling
            std::process::Command::new("reboot").output()
        };
        
        if let Err(e) = result {
            echo_client(msg.channel_id, &wr, &format!("Reboot failed: {e}"), MessageLevel::Fail).await?;
        }
    }
    
    #[cfg(not(target_os = "linux"))]
    {
        echo_client(msg.channel_id, &wr, "Reboot not supported on this platform", MessageLevel::Fail).await?;
    }
    
    let response = TaskMessage {
        channel_id: msg.channel_id,
        command: HdcCommand::KernelChannelClose,
        payload: vec![0],
    };
    send_shared(&wr, &response).await
}

async fn handle_remount(
    msg: TaskMessage,
    wr: SharedWriter,
) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        match std::process::Command::new("mount").args(["-o", "remount,rw", "/"]).output() {
            Ok(output) => {
                if output.status.success() {
                    echo_client(msg.channel_id, &wr, "Mount finish", MessageLevel::Ok).await?;
                } else {
                    echo_client(msg.channel_id, &wr, "Mount failed", MessageLevel::Fail).await?;
                }
            }
            Err(_) => {
                echo_client(msg.channel_id, &wr, "Mount failed", MessageLevel::Fail).await?;
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        echo_client(msg.channel_id, &wr, "Remount not supported on this platform", MessageLevel::Fail).await?;
    }
    let response = TaskMessage {
        channel_id: msg.channel_id,
        command: HdcCommand::KernelChannelClose,
        payload: vec![0],
    };
    send_shared(&wr, &response).await
}

async fn handle_runmode(
    msg: TaskMessage,
    wr: SharedWriter,
) -> io::Result<()> {
    let mode = String::from_utf8_lossy(&msg.payload);
    echo_client(msg.channel_id, &wr, &format!("Runmode: {mode}"), MessageLevel::Info).await?;
    let response = TaskMessage {
        channel_id: msg.channel_id,
        command: HdcCommand::KernelChannelClose,
        payload: vec![0],
    };
    send_shared(&wr, &response).await
}

async fn handle_hilog(
    msg: TaskMessage,
    wr: SharedWriter,
) -> io::Result<()> {
    // On real HarmonyOS, this would read from hilog. For now, just echo.
    let payload = if msg.payload.len() == 1 && msg.payload[0] == 104 {
        // 'h' for help
        "hilog help output..."
    } else {
        "hilog output..."
    };
    let response = TaskMessage {
        channel_id: msg.channel_id,
        command: HdcCommand::KernelEchoRaw,
        payload: payload.as_bytes().to_vec(),
    };
    send_shared(&wr, &response).await?;
    let response = TaskMessage {
        channel_id: msg.channel_id,
        command: HdcCommand::KernelChannelClose,
        payload: vec![0],
    };
    send_shared(&wr, &response).await
}
