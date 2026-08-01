//! Flashd (update / flash / erase / format) stub handler for the HDC daemon.
//!
//! This is a stub implementation for testing the host-side flashd protocol.
//! It does not perform real flashing; it acknowledges the transfer and reports
//! success so the command flow can be verified end-to-end.

use hdc_protocol::config::{HdcCommand, MessageLevel, TaskMessage};
use std::io;
use tracing::{debug, info, warn};

use crate::task::{send_shared, SharedWriter};

pub async fn handle_flashd_task(
    msg: TaskMessage,
    _session_id: u32,
    wr: SharedWriter,
) -> io::Result<()> {
    let channel_id = msg.channel_id;
    match msg.command {
        HdcCommand::FlashdCheck => {
            info!("flashd check received, sending FlashdBegin");
            let response = TaskMessage {
                channel_id,
                command: HdcCommand::FlashdBegin,
                payload: vec![],
            };
            send_shared(&wr, &response).await?;
        }
        HdcCommand::FlashdData => {
            debug!("flashd data received: {} bytes", msg.payload.len());
            // Stub: ignore the actual firmware data.
        }
        HdcCommand::FlashdFinish => {
            let from_host = msg.payload.first() == Some(&1);
            info!("flashd finish received from_host={}", from_host);
            if from_host {
                // Host has finished sending data; report stub success.
                let mut payload = vec![0u8, MessageLevel::Ok as u8];
                payload.extend_from_slice(b"flashd stub completed");
                let response = TaskMessage {
                    channel_id,
                    command: HdcCommand::FlashdFinish,
                    payload,
                };
                send_shared(&wr, &response).await?;
            }
        }
        HdcCommand::FlashdErase | HdcCommand::FlashdFormat => {
            info!("flashd erase/format received");
            let mut payload = vec![0u8, MessageLevel::Ok as u8];
            payload.extend_from_slice(b"flashd stub completed");
            let response = TaskMessage {
                channel_id,
                command: HdcCommand::FlashdFinish,
                payload,
            };
            send_shared(&wr, &response).await?;
        }
        HdcCommand::FlashdProgress => {
            // Host does not send progress, but handle defensively.
            debug!("flashd progress received: {:?}", msg.payload);
        }
        _ => {
            warn!("unhandled flashd command: {:?}", msg.command);
        }
    }
    Ok(())
}
