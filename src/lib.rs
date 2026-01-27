use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use tokio::sync::Semaphore;

mod js_runtime;

pub use js_runtime::PgliteRuntime;

static WASM_SEMAPHORE: Lazy<Semaphore> = Lazy::new(|| Semaphore::const_new(1));

pub struct PgliteConfig {
    pub data_dir: String,
    pub tcp_port: u16,
}

pub trait WireProcessor: Send + Sync {
    fn process_wire_message(&self, data: &[u8]) -> Result<Vec<u8>>;
}

struct WireMessage<'a> {
    msg_type: u8,
    payload: &'a [u8],
}

struct WireMessageIter<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> WireMessageIter<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, offset: 0 }
    }
}

impl<'a> Iterator for WireMessageIter<'a> {
    type Item = WireMessage<'a>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.offset + 5 > self.data.len() {
            return None;
        }
        let msg_type = self.data[self.offset];
        let msg_len = u32::from_be_bytes([
            self.data[self.offset + 1],
            self.data[self.offset + 2],
            self.data[self.offset + 3],
            self.data[self.offset + 4],
        ]) as usize;

        if msg_len < 4 || self.offset + 1 + msg_len > self.data.len() {
            return None;
        }

        let payload_start = self.offset + 5;
        let payload_end = self.offset + 1 + msg_len;
        let payload = &self.data[payload_start..payload_end];
        self.offset = payload_end;

        Some(WireMessage { msg_type, payload })
    }
}

pub struct AsyncPgliteExecutor {
    runtime: Arc<PgliteRuntime>,
}

impl AsyncPgliteExecutor {
    pub fn new(runtime: Arc<PgliteRuntime>) -> Self {
        Self { runtime }
    }

    pub async fn execute_query(&self, query: Vec<u8>) -> Result<Vec<u8>> {
        self.runtime.process_wire_message_async(query).await
    }
}

const PGLITE_SERVER_VERSION: &str = "17.5";

#[inline]
fn has_server_version(response: &[u8]) -> bool {
    WireMessageIter::new(response)
        .any(|msg| msg.msg_type == b'S' && msg.payload.starts_with(b"server_version\0"))
}

fn create_server_version_message() -> Vec<u8> {
    let name = b"server_version\0";
    let value = format!("{}\0", PGLITE_SERVER_VERSION);
    let value_bytes = value.as_bytes();
    let payload_len = name.len() + value_bytes.len();
    let msg_len = (4 + payload_len) as u32;

    let mut msg = Vec::with_capacity(1 + 4 + payload_len);
    msg.push(b'S');
    msg.extend_from_slice(&msg_len.to_be_bytes());
    msg.extend_from_slice(name);
    msg.extend_from_slice(value_bytes);
    msg
}

#[inline]
fn find_ready_for_query(response: &[u8]) -> Option<usize> {
    let mut offset = 0;
    for msg in WireMessageIter::new(response) {
        if msg.msg_type == b'Z' {
            return Some(offset);
        }
        offset += 1 + 4 + msg.payload.len();
    }
    None
}

fn ensure_server_version(response: Vec<u8>, has_sent_server_version: &mut bool) -> Vec<u8> {
    if response.is_empty() || *has_sent_server_version {
        return response;
    }

    if has_server_version(&response) {
        *has_sent_server_version = true;
        return response;
    }

    if let Some(rfq_pos) = find_ready_for_query(&response) {
        let server_version_msg = create_server_version_message();
        let mut new_response = Vec::with_capacity(response.len() + server_version_msg.len());
        new_response.extend_from_slice(&response[..rfq_pos]);
        new_response.extend_from_slice(&server_version_msg);
        new_response.extend_from_slice(&response[rfq_pos..]);
        *has_sent_server_version = true;
        new_response
    } else {
        response
    }
}

pub fn handle_connection(mut stream: TcpStream, runtime: Arc<dyn WireProcessor>) -> Result<()> {
    use std::time::Duration;

    stream.set_nodelay(true)?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut has_sent_server_version = false;

    let rt = tokio::runtime::Handle::try_current().unwrap_or_else(|_| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime")
            .handle()
            .clone()
    });

    loop {
        stream.set_read_timeout(Some(Duration::from_millis(100)))?;

        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let _permit = rt
                    .block_on(WASM_SEMAPHORE.acquire())
                    .expect("Semaphore closed");

                match runtime.process_wire_message(&buf[..n]) {
                    Ok(response) if !response.is_empty() => {
                        let response =
                            ensure_server_version(response, &mut has_sent_server_version);
                        stream.write_all(&response)?;
                        stream.flush()?;
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(e).context("Failed to read from client"),
        }
    }

    Ok(())
}

pub async fn handle_connection_async(
    stream: tokio::net::TcpStream,
    executor: Arc<AsyncPgliteExecutor>,
) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut buf = vec![0u8; 64 * 1024];
    let mut has_sent_server_version = false;

    let mut reader = stream;
    reader.set_nodelay(true)?;

    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => return Err(e).context("Failed to read from client"),
        };

        // NOTE: Semaphore acquisition removed - executor handles serialization internally
        // This prevents double semaphore acquisition deadlock

        match executor.execute_query(buf[..n].to_vec()).await {
            Ok(response) if !response.is_empty() => {
                let response = ensure_server_version(response, &mut has_sent_server_version);
                reader.write_all(&response).await?;
                reader.flush().await?;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_wire_message(msg_type: u8, payload: &[u8]) -> Vec<u8> {
        let msg_len = (4 + payload.len()) as u32;
        let mut msg = Vec::with_capacity(1 + 4 + payload.len());
        msg.push(msg_type);
        msg.extend_from_slice(&msg_len.to_be_bytes());
        msg.extend_from_slice(payload);
        msg
    }

    #[test]
    fn wire_message_iter_parses_single_message() {
        let msg = create_wire_message(b'Q', b"SELECT 1\0");

        let messages: Vec<_> = WireMessageIter::new(&msg).collect();

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].msg_type, b'Q');
        assert_eq!(messages[0].payload, b"SELECT 1\0");
    }

    #[test]
    fn wire_message_iter_parses_multiple_messages() {
        let mut data = create_wire_message(b'Q', b"SELECT 1\0");
        data.extend(create_wire_message(b'Z', b"I"));

        let messages: Vec<_> = WireMessageIter::new(&data).collect();

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].msg_type, b'Q');
        assert_eq!(messages[1].msg_type, b'Z');
        assert_eq!(messages[1].payload, b"I");
    }

    #[test]
    fn wire_message_iter_handles_empty_data() {
        let messages: Vec<_> = WireMessageIter::new(&[]).collect();

        assert!(messages.is_empty());
    }

    #[test]
    fn wire_message_iter_handles_truncated_header() {
        let messages: Vec<_> = WireMessageIter::new(&[b'Q', 0, 0]).collect();

        assert!(messages.is_empty());
    }

    #[test]
    fn wire_message_iter_handles_invalid_length() {
        let mut msg = vec![b'Q'];
        msg.extend_from_slice(&3u32.to_be_bytes());

        let messages: Vec<_> = WireMessageIter::new(&msg).collect();

        assert!(messages.is_empty());
    }

    #[test]
    fn wire_message_iter_handles_truncated_payload() {
        let mut msg = vec![b'Q'];
        msg.extend_from_slice(&100u32.to_be_bytes());
        msg.extend_from_slice(b"short");

        let messages: Vec<_> = WireMessageIter::new(&msg).collect();

        assert!(messages.is_empty());
    }

    #[test]
    fn has_server_version_returns_true_when_present() {
        let msg = create_wire_message(b'S', b"server_version\x0017.5\0");

        assert!(has_server_version(&msg));
    }

    #[test]
    fn has_server_version_returns_false_when_absent() {
        let msg = create_wire_message(b'S', b"other_param\0value\0");

        assert!(!has_server_version(&msg));
    }

    #[test]
    fn has_server_version_returns_false_for_non_s_message() {
        let msg = create_wire_message(b'Q', b"server_version\0value\0");

        assert!(!has_server_version(&msg));
    }

    #[test]
    fn create_server_version_message_has_correct_format() {
        let msg = create_server_version_message();

        assert_eq!(msg[0], b'S');

        let len = u32::from_be_bytes([msg[1], msg[2], msg[3], msg[4]]) as usize;
        assert_eq!(msg.len(), 1 + 4 + len - 4);

        let payload = &msg[5..];
        assert!(payload.starts_with(b"server_version\0"));
        assert!(payload.ends_with(b"\0"));
    }

    #[test]
    fn find_ready_for_query_returns_position() {
        let mut data = create_wire_message(b'S', b"param\0value\0");
        let rfq_pos = data.len();
        data.extend(create_wire_message(b'Z', b"I"));

        let pos = find_ready_for_query(&data);

        assert_eq!(pos, Some(rfq_pos));
    }

    #[test]
    fn find_ready_for_query_returns_none_when_absent() {
        let data = create_wire_message(b'S', b"param\0value\0");

        assert!(find_ready_for_query(&data).is_none());
    }

    #[test]
    fn ensure_server_version_returns_unchanged_when_empty() {
        let mut flag = false;
        let result = ensure_server_version(vec![], &mut flag);

        assert!(result.is_empty());
        assert!(!flag);
    }

    #[test]
    fn ensure_server_version_returns_unchanged_when_already_sent() {
        let data = create_wire_message(b'Z', b"I");
        let mut flag = true;

        let result = ensure_server_version(data.clone(), &mut flag);

        assert_eq!(result, data);
    }

    #[test]
    fn ensure_server_version_marks_flag_when_version_present() {
        let data = create_wire_message(b'S', b"server_version\x0017.5\0");
        let mut flag = false;

        let result = ensure_server_version(data.clone(), &mut flag);

        assert_eq!(result, data);
        assert!(flag);
    }

    #[test]
    fn ensure_server_version_injects_before_ready_for_query() {
        let mut data = create_wire_message(b'S', b"other\0value\0");
        data.extend(create_wire_message(b'Z', b"I"));
        let mut flag = false;

        let result = ensure_server_version(data.clone(), &mut flag);

        assert!(result.len() > data.len());
        assert!(flag);
        assert!(has_server_version(&result));
    }

    #[test]
    fn ensure_server_version_returns_unchanged_without_ready_for_query() {
        let data = create_wire_message(b'S', b"other\0value\0");
        let mut flag = false;

        let result = ensure_server_version(data.clone(), &mut flag);

        assert_eq!(result, data);
        assert!(!flag);
    }

    #[test]
    fn pglite_config_stores_values() {
        let config = PgliteConfig {
            data_dir: "memory://test".to_string(),
            tcp_port: 5432,
        };

        assert_eq!(config.data_dir, "memory://test");
        assert_eq!(config.tcp_port, 5432);
    }
}
