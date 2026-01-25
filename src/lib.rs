use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Notify, Semaphore};

mod js_runtime;

pub use js_runtime::PgliteRuntime;

static WASM_SEMAPHORE: Lazy<Semaphore> = Lazy::new(|| Semaphore::const_new(1));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryPriority {
    High,
    Normal,
}

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

fn detect_priority(data: &[u8]) -> QueryPriority {
    if data.len() > 5 && data[0] == b'Q' {
        let query_bytes = &data[5..];
        let query_end = query_bytes
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(query_bytes.len());

        if let Ok(sql) = std::str::from_utf8(&query_bytes[..query_end]) {
            if is_high_priority_command(sql) {
                return QueryPriority::High;
            }
        }
    }

    if data.len() > 5 && data[0] == b'P' {
        if let Some(name_end) = data[5..].iter().position(|&b| b == 0) {
            let query_start = 5 + name_end + 1;
            if query_start < data.len() {
                let query_bytes = &data[query_start..];
                let query_end = query_bytes
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(query_bytes.len());

                if let Ok(sql) = std::str::from_utf8(&query_bytes[..query_end]) {
                    if is_high_priority_command(sql) {
                        return QueryPriority::High;
                    }
                }
            }
        }
    }

    QueryPriority::Normal
}

fn is_high_priority_command(sql: &str) -> bool {
    let trimmed = sql.trim();
    let first_word = trimmed
        .split(|c: char| c.is_whitespace() || c == ';')
        .next()
        .unwrap_or("");

    let upper = first_word.to_uppercase();

    matches!(
        upper.as_str(),
        "COMMIT" | "ROLLBACK" | "END" | "ABORT" | "SAVEPOINT" | "RELEASE"
    )
}

pub struct AsyncPgliteExecutor {
    high_priority_tx: mpsc::Sender<QueryRequest>,
    normal_priority_tx: mpsc::Sender<QueryRequest>,
    work_available: Arc<Notify>,
}

struct QueryRequest {
    query: Vec<u8>,
    response_tx: oneshot::Sender<Vec<u8>>,
}

impl AsyncPgliteExecutor {
    pub fn new(runtime: Arc<dyn WireProcessor>) -> Self {
        let (high_priority_tx, high_priority_rx) = mpsc::channel::<QueryRequest>(100);
        let (normal_priority_tx, normal_priority_rx) = mpsc::channel::<QueryRequest>(1000);
        let work_available = Arc::new(Notify::new());
        let work_available_clone = Arc::clone(&work_available);

        tokio::spawn(async move {
            let mut high_rx = high_priority_rx;
            let mut normal_rx = normal_priority_rx;
            let notify = work_available_clone;

            loop {
                tokio::select! {
                    biased;
                    result = high_rx.recv() => {
                        if let Some(request) = result {
                            let _permit = WASM_SEMAPHORE.acquire().await;
                            let runtime_clone = Arc::clone(&runtime);
                            let query = request.query;

                            let response = tokio::task::spawn_blocking(move || {
                                runtime_clone.process_wire_message(&query)
                            }).await.unwrap_or_else(|_| Err(anyhow::anyhow!("Task panicked")));

                            match response {
                                Ok(response) => {
                                    let _ = request.response_tx.send(response);
                                }
                                Err(_) => {
                                    let _ = request.response_tx.send(Vec::new());
                                }
                            }
                        }
                    }
                    result = normal_rx.recv() => {
                        match result {
                            Some(request) => {
                                let _permit = WASM_SEMAPHORE.acquire().await;
                                let runtime_clone = Arc::clone(&runtime);
                                let query = request.query;

                                let response = tokio::task::spawn_blocking(move || {
                                    runtime_clone.process_wire_message(&query)
                                }).await.unwrap_or_else(|_| Err(anyhow::anyhow!("Task panicked")));

                                match response {
                                    Ok(response) => {
                                        let _ = request.response_tx.send(response);
                                    }
                                    Err(_) => {
                                        let _ = request.response_tx.send(Vec::new());
                                    }
                                }
                            }
                            None => break,
                        }
                    }
                    _ = notify.notified() => {}
                }
            }
        });

        Self {
            high_priority_tx,
            normal_priority_tx,
            work_available,
        }
    }

    pub async fn execute_query(&self, query: Vec<u8>) -> Result<Vec<u8>> {
        let (response_tx, response_rx) = oneshot::channel();
        let priority = detect_priority(&query);
        let request = QueryRequest { query, response_tx };
        let send_result = match priority {
            QueryPriority::High => self.high_priority_tx.send(request).await,
            QueryPriority::Normal => self.normal_priority_tx.send(request).await,
        };

        if send_result.is_ok() {
            self.work_available.notify_one();
            response_rx
                .await
                .map_err(|_| anyhow::anyhow!("Query execution failed"))
        } else {
            Err(anyhow::anyhow!("Executor channel closed"))
        }
    }
}

pub fn bind_tcp_socket(port: u16) -> Result<TcpListener> {
    TcpListener::bind(("127.0.0.1", port)).context(format!("Failed to bind to port {}", port))
}

const PGLITE_SERVER_VERSION: &str = "17.5";

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

fn message_starts_transaction(data: &[u8]) -> bool {
    if data.is_empty() {
        return false;
    }
    matches!(
        data[0],
        b'P' | b'Q' | b'B' | b'E' | b'D' | b'C' | b'H' | b'F'
    )
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

        let needs_init = !has_sent_server_version || message_starts_transaction(&buf[..n]);

        if needs_init {
            let _permit = WASM_SEMAPHORE.acquire().await;
        }

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
