//! Interactive shell bridge using portable-pty.

use hdc_protocol::config::{HdcCommand, TaskMessage};
use hdc_protocol::serializer::concat_pack;
use portable_pty::{CommandBuilder, NativePtySystem, PtySize};
use std::io::{Read, Write};
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::OwnedReadHalf;
use tracing::{error, info, warn};

use crate::task::SharedWriter;

pub async fn run_shell_bridge(mut rd: OwnedReadHalf, wr: SharedWriter, channel_id: u32, session_id: u32) {
    info!("starting interactive shell bridge for channel {channel_id}");

    let pty_system = match portable_pty::native_pty_system().openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(e) => {
            error!("PTY open failed: {e}");
            let _ = send_echo(&wr, channel_id, "Shell initialize failed").await;
            return;
        }
    };

    #[cfg(target_os = "windows")]
    let cmd = CommandBuilder::new("cmd.exe");
    #[cfg(not(target_os = "windows"))]
    let cmd = CommandBuilder::new("sh");

    let _child = match pty_system.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(e) => {
            error!("Spawn shell failed: {e}");
            let _ = send_echo(&wr, channel_id, "Shell initialize failed").await;
            return;
        }
    };

    let pty_writer = pty_system.master.take_writer().expect("take_writer");
    let mut pty_reader = match pty_system.master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => {
            error!("PTY reader clone failed: {e}");
            return;
        }
    };

    // Channel: TCP -> PTY writer thread
    let (tcp_to_pty_tx, tcp_to_pty_rx) = std::sync::mpsc::channel::<Vec<u8>>();

    // Channel: PTY reader thread -> TCP
    let (pty_to_tcp_tx, mut pty_to_tcp_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

    // Spawn PTY reader thread
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match pty_reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if pty_to_tcp_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Spawn PTY writer thread
    std::thread::spawn(move || {
        let mut writer = pty_writer;
        loop {
            match tcp_to_pty_rx.recv() {
                Ok(data) => {
                    if writer.write_all(&data).is_err() {
                        break;
                    }
                    if writer.flush().is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Spawn TCP reader task
    let (tcp_msg_tx, mut tcp_msg_rx) = tokio::sync::mpsc::channel::<TaskMessage>(64);
    tokio::spawn(async move {
        loop {
            match crate::transfer::tcp::unpack_task_message(&mut rd, session_id).await {
                Ok(msg) => {
                    if tcp_msg_tx.send(msg).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Main bridge loop
    loop {
        tokio::select! {
            Some(msg) = tcp_msg_rx.recv() => {
                match msg.command {
                    HdcCommand::ShellData => {
                        let _ = tcp_to_pty_tx.send(msg.payload);
                    }
                    HdcCommand::HeartbeatMsg => {
                        // Echo heartbeat back to host during interactive shell
                        let response = TaskMessage {
                            channel_id,
                            command: HdcCommand::HeartbeatMsg,
                            payload: msg.payload,
                        };
                        let mut guard = wr.wr.lock().await;
                        let _ = crate::transfer::tcp::send_message(&mut *guard, session_id, &response).await;
                    }
                    HdcCommand::KernelChannelClose => {
                        break;
                    }
                    _ => {}
                }
            }
            Some(data) = pty_to_tcp_rx.recv() => {
                let shell_msg = TaskMessage {
                    channel_id,
                    command: HdcCommand::ShellData,
                    payload: data,
                };
                {
                    let mut guard = wr.wr.lock().await;
                    if crate::transfer::tcp::send_message(&mut *guard, session_id, &shell_msg).await.is_err() {
                        drop(guard);
                        break;
                    }
                }
            }
        }
    }

    info!("interactive shell bridge ended for channel {channel_id}");
}

async fn send_echo(wr: &SharedWriter, channel_id: u32, msg: &str) -> std::io::Result<()> {
    let response = TaskMessage {
        channel_id,
        command: HdcCommand::KernelEcho,
        payload: [vec![2u8], format!("{}\r\n", msg).into_bytes()].concat(),
    };
    let mut guard = wr.wr.lock().await;
    crate::transfer::tcp::send_message(&mut *guard, wr.session_id, &response).await
}
