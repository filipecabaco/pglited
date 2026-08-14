use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

mod js_runtime;

pub use js_runtime::PgliteRuntime;

/// Set once the process should stop accepting work.
pub static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Woken alongside [`SHUTDOWN`] so async accept loops don't have to poll.
pub static SHUTDOWN_NOTIFY: Lazy<tokio::sync::Notify> = Lazy::new(tokio::sync::Notify::new);

pub fn request_shutdown(wake_port: u16) {
    if SHUTDOWN.swap(true, Ordering::SeqCst) {
        return;
    }
    SHUTDOWN_NOTIFY.notify_waiters();
    // A blocking accept() only returns once a connection arrives.
    if wake_port != 0 {
        let _ = TcpStream::connect(("127.0.0.1", wake_port));
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(default)
}

/// Largest single frontend message we will buffer, matching PostgreSQL's own
/// 1 GiB ceiling. Override with `PGLITED_MAX_MESSAGE_BYTES`.
pub static MAX_MESSAGE_BYTES: Lazy<usize> =
    Lazy::new(|| env_usize("PGLITED_MAX_MESSAGE_BYTES", 1024 * 1024 * 1024).max(1024));

/// Concurrent client connections allowed. Queries are serialized onto one JS
/// thread anyway, so this bounds threads, sockets and buffers rather than
/// throughput.
pub static MAX_CONNECTIONS: Lazy<usize> =
    Lazy::new(|| env_usize("PGLITED_MAX_CONNECTIONS", 100).max(1));

/// How long a blocking connection waits for the JS thread.
/// `PGLITED_QUERY_TIMEOUT_SECS=0` waits indefinitely.
pub(crate) fn query_timeout() -> Option<Duration> {
    match env_usize("PGLITED_QUERY_TIMEOUT_SECS", 300) {
        0 => None,
        secs => Some(Duration::from_secs(secs as u64)),
    }
}

const READ_CHUNK: usize = 64 * 1024;

/// Per-connection buffers above this size are released once drained.
const INBOX_SHRINK_THRESHOLD: usize = 1024 * 1024;

pub struct PgliteConfig {
    pub data_dir: String,
    pub tcp_port: u16,
    pub extensions: Vec<String>,
}

/// Extension names end up in a module specifier (`pglite:///contrib/<name>.js`)
/// and in generated JS, so restrict them to an unambiguous character set.
pub fn validate_extension_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        anyhow::bail!("Invalid extension name: {:?}", name);
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        anyhow::bail!(
            "Invalid extension name {:?}: only letters, digits, '_' and '-' are allowed",
            name
        );
    }
    Ok(())
}

pub trait WireProcessor: Send + Sync {
    fn process_wire_message(&self, data: &[u8]) -> Result<Vec<u8>>;
}

// ---------------------------------------------------------------------------
// Connection admission
// ---------------------------------------------------------------------------

pub struct ConnectionLimiter {
    active: AtomicUsize,
    max: usize,
}

pub struct ConnectionPermit(Arc<ConnectionLimiter>);

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Release);
    }
}

impl ConnectionLimiter {
    pub fn new(max: usize) -> Arc<Self> {
        Arc::new(Self {
            active: AtomicUsize::new(0),
            max: max.max(1),
        })
    }

    pub fn max(&self) -> usize {
        self.max
    }

    pub fn try_acquire(self: &Arc<Self>) -> Option<ConnectionPermit> {
        let mut current = self.active.load(Ordering::Acquire);
        loop {
            if current >= self.max {
                return None;
            }
            match self.active.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(ConnectionPermit(Arc::clone(self))),
                Err(observed) => current = observed,
            }
        }
    }
}

/// ErrorResponse for a client that arrives with no free connection slot.
pub fn too_many_clients_response() -> Vec<u8> {
    let mut payload = Vec::with_capacity(96);
    for (code, value) in [
        (b'S', "FATAL"),
        (b'V', "FATAL"),
        (b'C', "53300"),
        (b'M', "sorry, too many clients already"),
    ] {
        payload.push(code);
        payload.extend_from_slice(value.as_bytes());
        payload.push(0);
    }
    payload.push(0);

    let mut msg = Vec::with_capacity(5 + payload.len());
    msg.push(b'E');
    msg.extend_from_slice(&((4 + payload.len()) as u32).to_be_bytes());
    msg.extend_from_slice(&payload);
    msg
}

// ---------------------------------------------------------------------------
// Frontend message framing
// ---------------------------------------------------------------------------

const SSL_REQUEST_CODE: u32 = 80877103;
const GSSENC_REQUEST_CODE: u32 = 80877104;
const CANCEL_REQUEST_CODE: u32 = 80877102;

#[inline]
fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FrontendPhase {
    /// Before the StartupMessage: length-prefixed, no type byte.
    Startup,
    /// After the StartupMessage: type byte followed by a length prefix.
    Steady,
}

/// Splits the raw TCP byte stream back into whole frontend messages.
///
/// PGlite parses what it is handed as a complete protocol batch, so a message
/// spanning more than one read must not be forwarded piecemeal.
#[derive(Debug)]
struct FrontendFramer {
    phase: FrontendPhase,
    max_message_bytes: usize,
}

impl FrontendFramer {
    fn new(max_message_bytes: usize) -> Self {
        Self {
            phase: FrontendPhase::Startup,
            max_message_bytes,
        }
    }

    /// Length of the longest prefix of `buf` made up of complete messages;
    /// 0 when more bytes are needed. Errors on a malformed or oversized
    /// length header.
    fn complete_prefix_len(&mut self, buf: &[u8]) -> Result<usize> {
        let mut offset = 0;

        loop {
            let remaining = buf.len() - offset;
            match self.phase {
                FrontendPhase::Startup => {
                    // Length plus request code: an SSL/GSSENC/Cancel request
                    // is not followed by a phase change.
                    if remaining < 8 {
                        break;
                    }
                    let len = be32(&buf[offset..]) as usize;
                    if len < 8 || len > self.max_message_bytes {
                        anyhow::bail!("Invalid startup message length: {}", len);
                    }
                    if remaining < len {
                        break;
                    }
                    let code = be32(&buf[offset + 4..]);
                    if !matches!(
                        code,
                        SSL_REQUEST_CODE | GSSENC_REQUEST_CODE | CANCEL_REQUEST_CODE
                    ) {
                        self.phase = FrontendPhase::Steady;
                    }
                    offset += len;
                }
                FrontendPhase::Steady => {
                    if remaining < 5 {
                        break;
                    }
                    let len = be32(&buf[offset + 1..]) as usize;
                    if len < 4 || len > self.max_message_bytes {
                        anyhow::bail!("Invalid message length: {}", len);
                    }
                    if remaining < 1 + len {
                        break;
                    }
                    offset += 1 + len;
                }
            }
        }

        Ok(offset)
    }
}

#[inline]
fn consume(buf: &mut Vec<u8>, n: usize) {
    if n == buf.len() {
        buf.clear();
        if buf.capacity() > INBOX_SHRINK_THRESHOLD {
            buf.shrink_to(READ_CHUNK);
        }
    } else {
        buf.drain(..n);
    }
}

// ---------------------------------------------------------------------------
// Backend message inspection
// ---------------------------------------------------------------------------

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
        let msg_len = be32(&self.data[self.offset + 1..]) as usize;

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

// ---------------------------------------------------------------------------
// Connection handling
// ---------------------------------------------------------------------------

pub fn handle_connection(mut stream: TcpStream, runtime: Arc<dyn WireProcessor>) -> Result<()> {
    stream.set_nodelay(true)?;
    // Bounded so an idle connection notices process shutdown.
    stream.set_read_timeout(Some(Duration::from_millis(250)))?;

    let mut chunk = vec![0u8; READ_CHUNK];
    let mut inbox: Vec<u8> = Vec::with_capacity(READ_CHUNK);
    let mut framer = FrontendFramer::new(*MAX_MESSAGE_BYTES);
    let mut has_sent_server_version = false;

    while !SHUTDOWN.load(Ordering::Relaxed) {
        let n = match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(ref e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                continue
            }
            Err(e) => return Err(e).context("Failed to read from client"),
        };

        inbox.extend_from_slice(&chunk[..n]);
        let framed = framer.complete_prefix_len(&inbox)?;
        if framed == 0 {
            continue;
        }

        let result = runtime.process_wire_message(&inbox[..framed]);
        consume(&mut inbox, framed);

        match result {
            Ok(response) if !response.is_empty() => {
                let response = ensure_server_version(response, &mut has_sent_server_version);
                stream.write_all(&response)?;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    Ok(())
}

pub async fn handle_connection_async(
    mut stream: tokio::net::TcpStream,
    executor: Arc<AsyncPgliteExecutor>,
) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    stream.set_nodelay(true)?;

    let mut chunk = vec![0u8; READ_CHUNK];
    let mut inbox: Vec<u8> = Vec::with_capacity(READ_CHUNK);
    let mut framer = FrontendFramer::new(*MAX_MESSAGE_BYTES);
    let mut has_sent_server_version = false;

    loop {
        let n = match stream.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => return Err(e).context("Failed to read from client"),
        };

        inbox.extend_from_slice(&chunk[..n]);
        let framed = framer.complete_prefix_len(&inbox)?;
        if framed == 0 {
            continue;
        }

        let result = executor.execute_query(inbox[..framed].to_vec()).await;
        consume(&mut inbox, framed);

        match result {
            Ok(response) if !response.is_empty() => {
                let response = ensure_server_version(response, &mut has_sent_server_version);
                stream.write_all(&response).await?;
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

    fn startup_message(code: u32, extra: &[u8]) -> Vec<u8> {
        let len = (8 + extra.len()) as u32;
        let mut msg = Vec::with_capacity(len as usize);
        msg.extend_from_slice(&len.to_be_bytes());
        msg.extend_from_slice(&code.to_be_bytes());
        msg.extend_from_slice(extra);
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
            extensions: vec!["pg_trgm".to_string()],
        };

        assert_eq!(config.data_dir, "memory://test");
        assert_eq!(config.tcp_port, 5432);
        assert_eq!(config.extensions, vec!["pg_trgm".to_string()]);
    }

    #[test]
    fn framer_waits_for_a_whole_startup_message() {
        let mut framer = FrontendFramer::new(*MAX_MESSAGE_BYTES);
        let startup = startup_message(196608, b"user\0postgres\0\0");

        assert_eq!(framer.complete_prefix_len(&startup[..4]).unwrap(), 0);
        assert_eq!(
            framer
                .complete_prefix_len(&startup[..startup.len() - 1])
                .unwrap(),
            0
        );
        assert_eq!(framer.complete_prefix_len(&startup).unwrap(), startup.len());
    }

    #[test]
    fn framer_keeps_startup_phase_for_ssl_request() {
        let mut framer = FrontendFramer::new(*MAX_MESSAGE_BYTES);
        let ssl = startup_message(SSL_REQUEST_CODE, b"");

        assert_eq!(framer.complete_prefix_len(&ssl).unwrap(), ssl.len());
        assert_eq!(framer.phase, FrontendPhase::Startup);

        let startup = startup_message(196608, b"user\0postgres\0\0");
        assert_eq!(framer.complete_prefix_len(&startup).unwrap(), startup.len());
        assert_eq!(framer.phase, FrontendPhase::Steady);
    }

    #[test]
    fn framer_holds_back_a_split_query() {
        let mut framer = FrontendFramer::new(*MAX_MESSAGE_BYTES);
        let startup = startup_message(196608, b"user\0postgres\0\0");
        assert_eq!(framer.complete_prefix_len(&startup).unwrap(), startup.len());

        let query = create_wire_message(b'Q', b"SELECT 1\0");
        let split = query.len() - 3;

        assert_eq!(framer.complete_prefix_len(&query[..split]).unwrap(), 0);
        assert_eq!(framer.complete_prefix_len(&query).unwrap(), query.len());
    }

    #[test]
    fn framer_returns_all_complete_messages_and_holds_the_tail() {
        let mut framer = FrontendFramer::new(*MAX_MESSAGE_BYTES);
        framer.phase = FrontendPhase::Steady;

        let mut data = create_wire_message(b'P', b"\0SELECT 1\0\0\0");
        let first = data.len();
        data.extend(create_wire_message(b'B', b"\0\0\0\0\0\0\0\0"));
        let two = data.len();
        // A third message that only partially arrived.
        data.extend_from_slice(&[b'E', 0, 0, 0]);

        assert!(first < two);
        assert_eq!(framer.complete_prefix_len(&data).unwrap(), two);
    }

    #[test]
    fn framer_rejects_oversized_messages() {
        let mut framer = FrontendFramer::new(1024);
        framer.phase = FrontendPhase::Steady;

        let mut data = vec![b'Q'];
        data.extend_from_slice(&u32::MAX.to_be_bytes());

        assert!(framer.complete_prefix_len(&data).is_err());
    }

    #[test]
    fn framer_rejects_undersized_length_headers() {
        let mut framer = FrontendFramer::new(*MAX_MESSAGE_BYTES);
        framer.phase = FrontendPhase::Steady;

        let mut data = vec![b'Q'];
        data.extend_from_slice(&3u32.to_be_bytes());
        data.extend_from_slice(b"junk");

        assert!(framer.complete_prefix_len(&data).is_err());
    }

    #[test]
    fn consume_clears_and_shrinks_large_buffers() {
        let mut buf = vec![0u8; 4 * 1024 * 1024];
        let len = buf.len();

        consume(&mut buf, len);

        assert!(buf.is_empty());
        assert!(buf.capacity() <= INBOX_SHRINK_THRESHOLD);
    }

    #[test]
    fn consume_retains_the_unframed_tail() {
        let mut buf = vec![1, 2, 3, 4, 5];

        consume(&mut buf, 2);

        assert_eq!(buf, vec![3, 4, 5]);
    }

    #[test]
    fn connection_limiter_caps_concurrent_permits() {
        let limiter = ConnectionLimiter::new(2);

        let a = limiter.try_acquire().expect("first permit");
        let _b = limiter.try_acquire().expect("second permit");
        assert!(limiter.try_acquire().is_none());

        drop(a);
        assert!(limiter.try_acquire().is_some());
    }

    #[test]
    fn too_many_clients_response_is_a_well_formed_error() {
        let msg = too_many_clients_response();

        assert_eq!(msg[0], b'E');
        let len = u32::from_be_bytes([msg[1], msg[2], msg[3], msg[4]]) as usize;
        assert_eq!(msg.len(), 1 + len);
        assert_eq!(*msg.last().unwrap(), 0);
        assert!(msg.windows(5).any(|w| w == b"53300"));
    }

    #[test]
    fn validate_extension_name_accepts_plain_names() {
        assert!(validate_extension_name("pg_trgm").is_ok());
        assert!(validate_extension_name("uuid-ossp").is_ok());
    }

    #[test]
    fn validate_extension_name_rejects_traversal_and_injection() {
        assert!(validate_extension_name("../../etc/passwd").is_err());
        assert!(validate_extension_name("a\"; globalThis.x = 1; //").is_err());
        assert!(validate_extension_name("").is_err());
    }
}
