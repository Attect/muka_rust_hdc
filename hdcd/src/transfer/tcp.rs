//! TCP transport layer for HDC daemon.

use hdc_protocol::config::{HDC_BUF_MAX_SIZE, TaskMessage};
use hdc_protocol::encrypt::PskCipher;
use hdc_protocol::serializer::{HEAD_SIZE, unpack_payload_head, unpack_payload_protect};
use hdc_protocol::config::HdcCommand;
use std::collections::HashMap;
use std::io::{self, Error, ErrorKind};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

static SESSION_CIPHERS: OnceLock<Mutex<HashMap<u32, Arc<tokio::sync::Mutex<PskCipher>>>>> = OnceLock::new();

fn ciphers() -> &'static Mutex<HashMap<u32, Arc<tokio::sync::Mutex<PskCipher>>>> {
    SESSION_CIPHERS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn set_session_cipher(session_id: u32, cipher: PskCipher) {
    let mut map = ciphers().lock().unwrap();
    map.insert(session_id, Arc::new(tokio::sync::Mutex::new(cipher)));
}

pub fn remove_session_cipher(session_id: u32) {
    let mut map = ciphers().lock().unwrap();
    map.remove(&session_id);
}

fn get_session_cipher(session_id: u32) -> Option<Arc<tokio::sync::Mutex<PskCipher>>> {
    let map = ciphers().lock().unwrap();
    map.get(&session_id).cloned()
}

/// Read an exact number of bytes from the TCP stream.
pub async fn read_frame(rd: &mut OwnedReadHalf, expected_size: usize) -> io::Result<Vec<u8>> {
    if expected_size == 0 {
        return Ok(vec![]);
    }
    let mut data = vec![0u8; expected_size];
    let mut index = 0;
    while index < expected_size {
        match rd.read(&mut data[index..]).await {
            Ok(0) => {
                return Err(Error::new(ErrorKind::ConnectionAborted, "peer shutdown"));
            }
            Ok(n) => {
                index += n;
            }
            Err(e) => {
                return Err(Error::new(ErrorKind::Other, format!("read tcp failed: {e}")));
            }
        }
    }
    Ok(data)
}

async fn recv_raw_frame(rd: &mut OwnedReadHalf) -> io::Result<Vec<u8>> {
    let head = read_frame(rd, HEAD_SIZE).await?;
    let payload_head = unpack_payload_head(&head)?;
    let expected_head_size = payload_head.head_size as usize;
    let expected_data_size = payload_head.data_size as usize;
    if expected_head_size + expected_data_size == 0
        || expected_head_size + expected_data_size > HDC_BUF_MAX_SIZE
    {
        return Err(Error::new(ErrorKind::InvalidData, "Packet size incorrect"));
    }
    let protect = read_frame(rd, expected_head_size).await?;
    let payload = read_frame(rd, expected_data_size).await?;
    let mut frame = Vec::with_capacity(HEAD_SIZE + expected_head_size + expected_data_size);
    frame.extend_from_slice(&head);
    frame.extend_from_slice(&protect);
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Unpack a TaskMessage from a TCP stream.  If a PSK cipher has been registered
/// for `session_id`, the incoming bytes are decrypted first.
pub async fn unpack_task_message(rd: &mut OwnedReadHalf, session_id: u32) -> io::Result<TaskMessage> {
    let frame = if let Some(cipher) = get_session_cipher(session_id) {
        let len_bytes = read_frame(rd, 4).await?;
        let len = u32::from_be_bytes([len_bytes[0], len_bytes[1], len_bytes[2], len_bytes[3]]) as usize;
        let ct = read_frame(rd, len).await?;
        let mut cipher = cipher.lock().await;
        cipher.decrypt(&ct)?
    } else {
        recv_raw_frame(rd).await?
    };
    hdc_protocol::serializer::unpack_task_message(&frame)
}

/// Send a complete HDC packet.  If a PSK cipher has been registered for
/// `session_id`, the packet is encrypted before transmission.
pub async fn send_message(wr: &mut OwnedWriteHalf, session_id: u32, msg: &TaskMessage) -> io::Result<()> {
    let data = hdc_protocol::serializer::concat_pack(msg);
    if let Some(cipher) = get_session_cipher(session_id) {
        let mut cipher = cipher.lock().await;
        let ct = cipher.encrypt(&data)?;
        let len_bytes = (ct.len() as u32).to_be_bytes();
        wr.write_all(&len_bytes).await?;
        wr.write_all(&ct).await?;
    } else {
        wr.write_all(&data).await?;
    }
    Ok(())
}
