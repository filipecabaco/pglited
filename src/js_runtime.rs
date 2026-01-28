use anyhow::{Context, Result};
use deno_core::{
    extension, serde_v8, v8, FastString, JsRuntime, ModuleLoadOptions, ModuleLoadReferrer,
    ModuleLoadResponse, ModuleLoader, ModuleSource, ModuleSourceCode, ModuleSpecifier, ModuleType,
    PollEventLoopOptions, ResolutionKind, RuntimeOptions,
};
use deno_error::JsErrorBox;
use flate2::read::GzDecoder;
use regex::Regex;
use rust_embed::Embed;
use serde::de::DeserializeOwned;
use serde_json::json;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tokio::sync::oneshot;

fn debug_enabled() -> bool {
    std::env::var("PGLITE_DEBUG")
        .map(|v| v == "1" || v.to_lowercase() == "true")
        .unwrap_or(false)
}

fn debug_log(message: &str) {
    if debug_enabled() {
        eprintln!("[pglited-js] {}", message);
    }
}

// File descriptor table for managing open files
thread_local! {
    static FD_TABLE: RefCell<FdTable> = RefCell::new(FdTable::new());
}

struct FdTable {
    next_fd: u32,
    files: HashMap<u32, File>,
}

impl FdTable {
    fn new() -> Self {
        Self {
            next_fd: 10, // Start from 10 to avoid confusion with stdin/stdout/stderr
            files: HashMap::new(),
        }
    }

    fn open(&mut self, file: File) -> u32 {
        let fd = self.next_fd;
        self.next_fd += 1;
        self.files.insert(fd, file);
        fd
    }

    fn close(&mut self, fd: u32) -> std::io::Result<()> {
        self.files.remove(&fd).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, format!("Invalid fd: {}", fd))
        })?;
        Ok(())
    }

    fn get(&mut self, fd: u32) -> std::io::Result<&mut File> {
        self.files.get_mut(&fd).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, format!("Invalid fd: {}", fd))
        })
    }
}

use crate::{PgliteConfig, WireProcessor};

#[derive(Embed)]
#[folder = "assets/pglite_npm/dist"]
#[include = "**/*.js"]
#[include = "**/*.wasm"]
#[include = "**/*.data"]
#[include = "**/*.tar.gz"]
#[include = "**/*.tar"]
struct PgliteAssets;

#[derive(Embed)]
#[folder = "src/js"]
#[include = "*.js"]
struct JsShimAssets;

fn extract_value<T: DeserializeOwned>(
    runtime: &mut JsRuntime,
    global: v8::Global<v8::Value>,
) -> Result<T> {
    deno_core::scope!(scope, runtime);
    let local = v8::Local::new(scope, global);
    serde_v8::from_v8(scope, local).context("Failed to deserialize V8 value")
}

pub struct PgliteRuntime {
    sender: mpsc::Sender<JsRequest>,
    pub tcp_port: u16,
    pub data_dir: String,
}

enum JsRequest {
    Exec(Vec<u8>, mpsc::Sender<Result<Vec<u8>>>),
    ExecAsync(Vec<u8>, oneshot::Sender<Result<Vec<u8>>>),
    ExecSql(String, mpsc::Sender<Result<()>>),
    DumpDataDir(mpsc::Sender<Result<Vec<u8>>>),
    Shutdown,
}

impl PgliteRuntime {
    pub fn new(config: PgliteConfig) -> Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let data_dir = config.data_dir.clone();
        let extensions = config.extensions.clone();

        thread::spawn(move || {
            run_js_runtime(receiver, &data_dir, extensions, ready_tx);
        });

        ready_rx
            .recv_timeout(Duration::from_secs(120))
            .map_err(|_| anyhow::anyhow!("Timed out initializing PGlite runtime"))??;

        Ok(Self {
            sender,
            tcp_port: config.tcp_port,
            data_dir: config.data_dir,
        })
    }

    pub async fn process_wire_message_async(&self, data: Vec<u8>) -> Result<Vec<u8>> {
        let (response_tx, response_rx) = oneshot::channel();
        self.sender
            .send(JsRequest::ExecAsync(data, response_tx))
            .map_err(|_| anyhow::anyhow!("Runtime channel closed"))?;
        response_rx
            .await
            .map_err(|_| anyhow::anyhow!("Response channel closed"))?
    }

    pub fn init_postgres(&mut self) -> Result<()> {
        Ok(())
    }

    pub fn execute_sql(&self, sql: &str) -> Result<()> {
        let (response_tx, response_rx) = mpsc::channel();
        self.sender
            .send(JsRequest::ExecSql(sql.to_string(), response_tx))
            .map_err(|_| anyhow::anyhow!("Runtime channel closed"))?;
        response_rx
            .recv_timeout(Duration::from_secs(30))
            .map_err(|_| anyhow::anyhow!("SQL execution timeout"))?
    }

    pub fn dump_data_dir(&self) -> Result<Vec<u8>> {
        let (response_tx, response_rx) = mpsc::channel();
        self.sender
            .send(JsRequest::DumpDataDir(response_tx))
            .map_err(|_| anyhow::anyhow!("Runtime channel closed"))?;
        response_rx
            .recv_timeout(Duration::from_secs(60))
            .map_err(|_| anyhow::anyhow!("Dump datadir timeout"))?
    }
}

impl WireProcessor for PgliteRuntime {
    fn process_wire_message(&self, data: &[u8]) -> Result<Vec<u8>> {
        let (response_tx, response_rx) = mpsc::channel();
        self.sender
            .send(JsRequest::Exec(data.to_vec(), response_tx))
            .map_err(|_| anyhow::anyhow!("Runtime channel closed"))?;
        response_rx
            .recv_timeout(Duration::from_secs(30))
            .map_err(|_| anyhow::anyhow!("Runtime response timeout"))?
    }
}

impl Drop for PgliteRuntime {
    fn drop(&mut self) {
        let _ = self.sender.send(JsRequest::Shutdown);
    }
}

fn module_error(msg: impl Into<String>) -> deno_core::error::ModuleLoaderError {
    JsErrorBox::generic(msg.into())
}

fn normalize_asset_path(path: &str) -> String {
    let mut parts = Vec::new();
    for part in path.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            if !parts.is_empty() {
                parts.pop();
            }
            continue;
        }
        parts.push(part);
    }
    parts.join("/")
}

fn get_embedded_asset(path: &str) -> Option<std::borrow::Cow<'static, [u8]>> {
    let mut normalized = path
        .strip_prefix("pglite:///")
        .or_else(|| path.strip_prefix("pglite://"))
        .or_else(|| path.strip_prefix("file://"))
        .unwrap_or(path);

    normalized = normalized.strip_prefix('/').unwrap_or(normalized);

    // normalize_asset_path handles all . and .. segments properly
    let normalized = normalize_asset_path(normalized);

    let asset = PgliteAssets::get(&normalized).or_else(|| {
        let without_dist = normalized.strip_prefix("dist/").unwrap_or(&normalized);
        PgliteAssets::get(without_dist)
    });

    if asset.is_none() {
        debug_log(&format!("Embedded asset not found: {}", normalized));
    }

    asset.map(|f| f.data)
}

fn get_js_shim_asset(name: &str) -> Option<String> {
    JsShimAssets::get(name).and_then(|f| std::str::from_utf8(&f.data).ok().map(|s| s.to_string()))
}

struct EmbeddedModuleLoader;

fn extract_asset_path_from_specifier(specifier: &ModuleSpecifier) -> String {
    let url_str = specifier.as_str();
    if let Some(path) = url_str.strip_prefix("pglite:///") {
        return path.to_string();
    }
    if let Some(path) = url_str.strip_prefix("pglite://") {
        return path.to_string();
    }

    let path = specifier.path();
    normalize_asset_path(path.strip_prefix('/').unwrap_or(path))
}

impl ModuleLoader for EmbeddedModuleLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: ResolutionKind,
    ) -> Result<ModuleSpecifier, deno_core::error::ModuleLoaderError> {
        // Handle Node.js fs module
        if specifier == "fs" || specifier == "node:fs" {
            return ModuleSpecifier::parse("pglite:///fs").map_err(|e| module_error(e.to_string()));
        }

        // Handle Node.js path module
        if specifier == "path" || specifier == "node:path" {
            return ModuleSpecifier::parse("pglite:///path")
                .map_err(|e| module_error(e.to_string()));
        }

        if specifier.starts_with("pglite:///") || specifier.starts_with("pglite://") {
            return ModuleSpecifier::parse(specifier).map_err(|e| module_error(e.to_string()));
        }

        if specifier.starts_with("./") || specifier.starts_with("../") {
            let referrer_path = if referrer == "(no referrer)" || referrer.is_empty() {
                "pglite:///index.js".to_string()
            } else {
                referrer.to_string()
            };

            let referrer_spec =
                ModuleSpecifier::parse(&referrer_path).map_err(|e| module_error(e.to_string()))?;

            return referrer_spec
                .join(specifier)
                .map_err(|e| module_error(e.to_string()));
        }

        let url = format!("pglite:///{}", specifier);
        ModuleSpecifier::parse(&url).map_err(|e| module_error(e.to_string()))
    }

    fn load(
        &self,
        module_specifier: &ModuleSpecifier,
        _maybe_referrer: Option<&ModuleLoadReferrer>,
        _options: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        let specifier = module_specifier.clone();
        let asset_path = extract_asset_path_from_specifier(&specifier);

        // Serve our fs module shim from external file
        if asset_path == "fs" {
            let code =
                get_js_shim_asset("fs_shim.js").expect("fs_shim.js not found in embedded assets");
            return ModuleLoadResponse::Sync(Ok(ModuleSource::new(
                ModuleType::JavaScript,
                ModuleSourceCode::String(FastString::from(code)),
                &specifier,
                None,
            )));
        }

        // Serve our path module shim from external file
        if asset_path == "path" {
            let code = get_js_shim_asset("path_shim.js")
                .expect("path_shim.js not found in embedded assets");
            return ModuleLoadResponse::Sync(Ok(ModuleSource::new(
                ModuleType::JavaScript,
                ModuleSourceCode::String(FastString::from(code)),
                &specifier,
                None,
            )));
        }

        // Serve the RustBackedFilesystem module
        if asset_path == "shims/rust_backed_filesystem.js" {
            let code = get_js_shim_asset("rust_backed_filesystem.js")
                .expect("rust_backed_filesystem.js not found in embedded assets");
            return ModuleLoadResponse::Sync(Ok(ModuleSource::new(
                ModuleType::JavaScript,
                ModuleSourceCode::String(FastString::from(code)),
                &specifier,
                None,
            )));
        }

        let data = match get_embedded_asset(&asset_path) {
            Some(d) => d,
            None => {
                return ModuleLoadResponse::Sync(Err(module_error(format!(
                    "Embedded asset not found: {}",
                    asset_path
                ))));
            }
        };

        let code = match std::str::from_utf8(&data) {
            Ok(s) => s.to_string(),
            Err(e) => {
                return ModuleLoadResponse::Sync(Err(module_error(format!(
                    "Invalid UTF-8 in module {}: {}",
                    asset_path, e
                ))));
            }
        };

        // Patch index.js and fs/nodefs.js to inject fs/path modules for Emscripten NODEFS compatibility
        // The bundled code uses its own require() that doesn't see our global require
        // We do direct text replacement to ensure the modules are accessed from globalThis
        let code = if asset_path == "index.js" {
            // Use regex to match various forms of require for fs and path
            // This handles minified code where spacing may vary
            let mut patched = code.clone();

            // Match require("fs"), require('fs'), require( "fs" ), etc.
            // Also handles node:fs variants
            let fs_regex = Regex::new(r#"require\s*\(\s*["'](?:node:)?fs["']\s*\)"#).unwrap();
            patched = fs_regex.replace_all(&patched, "globalThis.fs").to_string();

            // Match require("path"), require('path'), etc.
            let path_regex = Regex::new(r#"require\s*\(\s*["'](?:node:)?path["']\s*\)"#).unwrap();
            patched = path_regex
                .replace_all(&patched, "globalThis.path")
                .to_string();

            // Also handle the case where a function is aliased and called with fs/path
            // e.g., n("fs") where n is a renamed require
            // Look for patterns like: =n("fs") or (n("fs") or ,n("fs")
            let aliased_fs_regex =
                Regex::new(r#"([=\(,])(\w+)\s*\(\s*["'](?:node:)?fs["']\s*\)"#).unwrap();
            patched = aliased_fs_regex
                .replace_all(&patched, "${1}globalThis.fs")
                .to_string();

            let aliased_path_regex =
                Regex::new(r#"([=\(,])(\w+)\s*\(\s*["'](?:node:)?path["']\s*\)"#).unwrap();
            patched = aliased_path_regex
                .replace_all(&patched, "${1}globalThis.path")
                .to_string();

            // Also ensure process is available
            let process_injection = r#"
// Ensure process global exists for Emscripten
if (typeof process === 'undefined') {
    globalThis.process = { platform: 'linux', env: {}, binding: function() { return {}; }, cwd: function() { return '/'; } };
}
"#;
            format!("{}\n{}", process_injection, patched)
        } else if asset_path == "fs/nodefs.js" {
            // For nodefs.js, prepend the process stub and make fs available via import
            let injection = r#"
// Ensure process global exists for Emscripten
if (typeof process === 'undefined') {
    globalThis.process = { platform: 'linux', env: {}, binding: function() { return {}; }, cwd: function() { return '/'; } };
}
"#;
            format!("{}\n{}", injection, code)
        } else {
            code
        };

        ModuleLoadResponse::Sync(Ok(ModuleSource::new(
            ModuleType::JavaScript,
            ModuleSourceCode::String(FastString::from(code)),
            &specifier,
            None,
        )))
    }
}

fn decompress_gzip(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut decoder = GzDecoder::new(data);
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed)?;
    Ok(decompressed)
}

#[deno_core::op2]
#[buffer]
fn op_read_file(#[string] path: String) -> std::result::Result<Vec<u8>, std::io::Error> {
    get_embedded_asset(&path)
        .map(|data| {
            let vec = data.to_vec();

            // Auto-decompress .tar.gz files so PGlite can read them directly
            // (PGlite tries to use DecompressionStream which we don't have)
            if path.ends_with(".tar.gz") || path.ends_with(".tgz") {
                if let Ok(decompressed) = decompress_gzip(&vec) {
                    return decompressed;
                }
                // Fall back to returning raw data on decompression error
            }

            vec
        })
        .ok_or_else(|| {
            debug_log(&format!("Embedded asset not found: {}", path));
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Embedded asset not found: {}", path),
            )
        })
}

#[deno_core::op2]
#[buffer]
fn op_exec_wire(#[buffer] payload: &[u8]) -> std::result::Result<Vec<u8>, std::io::Error> {
    Ok(payload.to_vec())
}

// Filesystem ops for Node.js fs module compatibility
#[deno_core::op2]
#[buffer]
fn op_fs_read_file_sync(#[string] path: String) -> std::result::Result<Vec<u8>, std::io::Error> {
    std::fs::read(&path)
}

#[deno_core::op2(fast)]
fn op_fs_write_file_sync(
    #[string] path: String,
    #[buffer] data: &[u8],
) -> std::result::Result<(), std::io::Error> {
    std::fs::write(&path, data)
}

#[deno_core::op2(fast)]
fn op_fs_mkdir_sync(
    #[string] path: String,
    recursive: bool,
) -> std::result::Result<(), std::io::Error> {
    if recursive {
        std::fs::create_dir_all(&path)
    } else {
        std::fs::create_dir(&path)
    }
}

#[deno_core::op2(fast)]
fn op_fs_exists_sync(#[string] path: String) -> bool {
    std::path::Path::new(&path).exists()
}

#[deno_core::op2(fast)]
fn op_fs_unlink_sync(#[string] path: String) -> std::result::Result<(), std::io::Error> {
    std::fs::remove_file(&path)
}

#[deno_core::op2(fast)]
fn op_fs_rmdir_sync(#[string] path: String) -> std::result::Result<(), std::io::Error> {
    std::fs::remove_dir(&path)
}

#[deno_core::op2]
#[serde]
fn op_fs_stat_sync(#[string] path: String) -> std::result::Result<FsStat, std::io::Error> {
    let metadata = std::fs::metadata(&path)?;
    Ok(FsStat::from_metadata(&metadata))
}

#[deno_core::op2]
#[serde]
fn op_fs_lstat_sync(#[string] path: String) -> std::result::Result<FsStat, std::io::Error> {
    let metadata = std::fs::symlink_metadata(&path)?;
    Ok(FsStat::from_metadata(&metadata))
}

#[deno_core::op2]
#[serde]
fn op_fs_readdir_sync(#[string] path: String) -> std::result::Result<Vec<String>, std::io::Error> {
    let entries = std::fs::read_dir(&path)?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str() {
            names.push(name.to_string());
        }
    }
    Ok(names)
}

#[deno_core::op2(fast)]
fn op_fs_rename_sync(
    #[string] old_path: String,
    #[string] new_path: String,
) -> std::result::Result<(), std::io::Error> {
    std::fs::rename(&old_path, &new_path)
}

#[deno_core::op2(fast)]
fn op_fs_truncate_sync(
    #[string] path: String,
    #[bigint] len: u64,
) -> std::result::Result<(), std::io::Error> {
    let file = std::fs::OpenOptions::new().write(true).open(&path)?;
    file.set_len(len)
}

// File descriptor based operations for BaseFilesystem support
#[deno_core::op2(fast)]
fn op_fs_open_sync(
    #[string] path: String,
    #[string] flags: String,
    mode: u32,
) -> std::result::Result<u32, std::io::Error> {
    let mut opts = std::fs::OpenOptions::new();

    // Parse flags string (Node.js style: 'r', 'r+', 'w', 'w+', 'a', 'a+', etc.)
    match flags.as_str() {
        "r" => {
            opts.read(true);
        }
        "r+" | "rs+" => {
            opts.read(true).write(true);
        }
        "w" => {
            opts.write(true).create(true).truncate(true);
        }
        "wx" => {
            opts.write(true).create_new(true);
        }
        "w+" => {
            opts.read(true).write(true).create(true).truncate(true);
        }
        "wx+" => {
            opts.read(true).write(true).create_new(true);
        }
        "a" => {
            opts.write(true).append(true).create(true);
        }
        "ax" => {
            opts.write(true).append(true).create_new(true);
        }
        "a+" => {
            opts.read(true).write(true).append(true).create(true);
        }
        "ax+" => {
            opts.read(true).write(true).append(true).create_new(true);
        }
        _ => {
            // Default to read
            opts.read(true);
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        if mode > 0 {
            opts.mode(mode);
        }
    }

    let file = opts.open(&path)?;
    let fd = FD_TABLE.with(|table| table.borrow_mut().open(file));
    Ok(fd)
}

#[deno_core::op2(fast)]
fn op_fs_close_sync(fd: u32) -> std::result::Result<(), std::io::Error> {
    FD_TABLE.with(|table| table.borrow_mut().close(fd))
}

#[deno_core::op2(fast)]
fn op_fs_read_fd_sync(
    fd: u32,
    #[buffer] buffer: &mut [u8],
    offset: u32,
    length: u32,
    #[bigint] position: i64,
) -> std::result::Result<u32, std::io::Error> {
    FD_TABLE.with(|table| {
        let mut table = table.borrow_mut();
        let file = table.get(fd)?;

        // Seek to position if specified (>= 0)
        if position >= 0 {
            file.seek(SeekFrom::Start(position as u64))?;
        }

        let offset = offset as usize;
        let length = length as usize;
        let end = std::cmp::min(offset + length, buffer.len());

        let bytes_read = file.read(&mut buffer[offset..end])?;
        Ok(bytes_read as u32)
    })
}

#[deno_core::op2(fast)]
fn op_fs_write_fd_sync(
    fd: u32,
    #[buffer] buffer: &[u8],
    offset: u32,
    length: u32,
    #[bigint] position: i64,
) -> std::result::Result<u32, std::io::Error> {
    FD_TABLE.with(|table| {
        let mut table = table.borrow_mut();
        let file = table.get(fd)?;

        // Seek to position if specified (>= 0)
        if position >= 0 {
            file.seek(SeekFrom::Start(position as u64))?;
        }

        let offset = offset as usize;
        let length = length as usize;
        let end = std::cmp::min(offset + length, buffer.len());

        let bytes_written = file.write(&buffer[offset..end])?;
        Ok(bytes_written as u32)
    })
}

#[deno_core::op2]
#[serde]
fn op_fs_fstat_sync(fd: u32) -> std::result::Result<FsStat, std::io::Error> {
    FD_TABLE.with(|table| {
        let mut table = table.borrow_mut();
        let file = table.get(fd)?;
        let metadata = file.metadata()?;
        Ok(FsStat::from_metadata(&metadata))
    })
}

#[deno_core::op2(fast)]
fn op_fs_fsync_sync(fd: u32) -> std::result::Result<(), std::io::Error> {
    FD_TABLE.with(|table| {
        let mut table = table.borrow_mut();
        let file = table.get(fd)?;
        file.sync_all()
    })
}

#[derive(serde::Serialize)]
struct FsStat {
    is_file: bool,
    is_directory: bool,
    is_symlink: bool,
    size: u64,
    mode: u32,
    mtime_ms: f64,
    atime_ms: f64,
    ctime_ms: f64,
}

impl FsStat {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::MetadataExt;
            metadata.mode()
        };
        #[cfg(not(unix))]
        let mode = if metadata.is_dir() { 0o755 } else { 0o644 };

        let mtime_ms = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as f64)
            .unwrap_or(0.0);

        let atime_ms = metadata
            .accessed()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as f64)
            .unwrap_or(0.0);

        let ctime_ms = metadata
            .created()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as f64)
            .unwrap_or(mtime_ms);

        FsStat {
            is_file: metadata.is_file(),
            is_directory: metadata.is_dir(),
            is_symlink: metadata.file_type().is_symlink(),
            size: metadata.len(),
            mode,
            mtime_ms,
            atime_ms,
            ctime_ms,
        }
    }
}

extension!(
    pglite_ext,
    ops = [
        op_read_file,
        op_exec_wire,
        op_fs_read_file_sync,
        op_fs_write_file_sync,
        op_fs_mkdir_sync,
        op_fs_exists_sync,
        op_fs_unlink_sync,
        op_fs_rmdir_sync,
        op_fs_stat_sync,
        op_fs_lstat_sync,
        op_fs_readdir_sync,
        op_fs_rename_sync,
        op_fs_truncate_sync,
        // fd-based ops for BaseFilesystem
        op_fs_open_sync,
        op_fs_close_sync,
        op_fs_read_fd_sync,
        op_fs_write_fd_sync,
        op_fs_fstat_sync,
        op_fs_fsync_sync,
    ],
);

fn build_extensions_loader_js(extensions: &[String]) -> String {
    if extensions.is_empty() {
        return "const __pgliteRequestedExtensions = [];\nconst __pgliteExtensions = undefined;"
            .to_string();
    }

    let mut specs = serde_json::Map::new();
    specs.insert(
        "live".to_string(),
        json!({ "module": "pglite:///live/index.js", "exportName": "live" }),
    );
    specs.insert(
        "vector".to_string(),
        json!({ "module": "pglite:///vector/index.js", "exportName": "vector" }),
    );
    specs.insert(
        "pg_hashids".to_string(),
        json!({ "module": "pglite:///pg_hashids/index.js", "exportName": "pg_hashids" }),
    );
    specs.insert(
        "pg_ivm".to_string(),
        json!({ "module": "pglite:///pg_ivm/index.js", "exportName": "pg_ivm" }),
    );
    specs.insert(
        "pg_uuidv7".to_string(),
        json!({ "module": "pglite:///pg_uuidv7/index.js", "exportName": "pg_uuidv7" }),
    );
    specs.insert(
        "pgtap".to_string(),
        json!({ "module": "pglite:///pgtap/index.js", "exportName": "pgtap" }),
    );

    let specs_json = serde_json::Value::Object(specs).to_string();
    let requested_json = serde_json::to_string(extensions).unwrap_or_else(|_| "[]".to_string());

    format!(
        r#"
        const __pgliteExtensionSpecs = {};
        const __pgliteRequestedExtensions = {};
        let __pgliteExtensions = undefined;
        if (__pgliteRequestedExtensions.length > 0) {{
            __pgliteExtensions = {{}};
            for (const name of __pgliteRequestedExtensions) {{
                const spec = __pgliteExtensionSpecs[name];
                const modulePath = spec?.module ?? `pglite:///contrib/${{name}}.js`;
                const exportName = spec?.exportName ?? name;
                let mod;
                try {{
                    mod = await import(modulePath);
                }} catch (err) {{
                    throw new Error(`Unknown extension: ${{name}}: ${{err}}`);
                }}
                const ext = mod[exportName];
                if (!ext) {{
                    throw new Error(`Extension export missing: ${{name}}`);
                }}
                __pgliteExtensions[name] = ext;
            }}
        }}
        "#,
        specs_json, requested_json
    )
}

fn run_js_runtime(
    receiver: mpsc::Receiver<JsRequest>,
    data_dir: &str,
    extensions: Vec<String>,
    ready_tx: mpsc::Sender<Result<()>>,
) {
    debug_log("Starting JS runtime thread");
    let tokio_rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = ready_tx.send(Err(anyhow::anyhow!(
                "Failed to create tokio runtime: {}",
                e
            )));
            return;
        }
    };

    let result = tokio_rt.block_on(async {
        // Configure V8 heap to avoid GC during startup and query processing
        // Initial: 256MB, Max: 1GB
        let create_params = v8::Isolate::create_params()
            .heap_limits(256 * 1024 * 1024, 1024 * 1024 * 1024);

        let mut runtime = JsRuntime::new(RuntimeOptions {
            extensions: vec![pglite_ext::init()],
            module_loader: Some(Rc::new(EmbeddedModuleLoader)),
            create_params: Some(create_params),
            ..Default::default()
        });

        debug_log("JS runtime created");

        let data_dir_str = normalize_data_dir(data_dir);

        let debug_enabled = debug_enabled();
        let bootstrap_code = format!(
            r#"
            // Debug logging (defined early so polyfills can use it)
            const __pgliteDebugEnabled = {debug_enabled};
            globalThis.__pgliteDebug = __pgliteDebugEnabled;
            const debugLog = (...args) => {{
                if (__pgliteDebugEnabled) {{
                    console.log(...args);
                }}
            }};
            const debugError = (...args) => {{
                if (__pgliteDebugEnabled) {{
                    console.error(...args);
                }}
            }};

            // TextEncoder polyfill (UTF-8 only)
            if (typeof TextEncoder === 'undefined') {{
                globalThis.TextEncoder = class TextEncoder {{
                    constructor() {{ this.encoding = 'utf-8'; }}
                    encode(str) {{
                        const buf = new Uint8Array(str.length * 4);
                        const result = this.encodeInto(str, buf);
                        return buf.subarray(0, result.written);
                    }}
                    encodeInto(str, dest) {{
                        let read = 0;
                        let written = 0;
                        while (read < str.length && written < dest.length) {{
                            let c = str.charCodeAt(read);
                            if (c < 128) {{
                                dest[written++] = c;
                                read++;
                            }} else if (c < 2048) {{
                                if (written + 2 > dest.length) break;
                                dest[written++] = (c >> 6) | 192;
                                dest[written++] = (c & 63) | 128;
                                read++;
                            }} else if (c < 55296 || c >= 57344) {{
                                if (written + 3 > dest.length) break;
                                dest[written++] = (c >> 12) | 224;
                                dest[written++] = ((c >> 6) & 63) | 128;
                                dest[written++] = (c & 63) | 128;
                                read++;
                            }} else {{
                                if (written + 4 > dest.length) break;
                                c = 65536 + (((c & 1023) << 10) | (str.charCodeAt(read + 1) & 1023));
                                dest[written++] = (c >> 18) | 240;
                                dest[written++] = ((c >> 12) & 63) | 128;
                                dest[written++] = ((c >> 6) & 63) | 128;
                                dest[written++] = (c & 63) | 128;
                                read += 2;
                            }}
                        }}
                        return {{ read, written }};
                    }}
                }};
            }}

            // TextDecoder polyfill (UTF-8 only) with proper error handling
            if (typeof TextDecoder === 'undefined') {{
                globalThis.TextDecoder = class TextDecoder {{
                    constructor(encoding = 'utf-8', options = {{}}) {{
                        this.encoding = encoding;
                        this.fatal = options.fatal || false;
                    }}
                    decode(buf, options = {{}}) {{
                        if (!buf) return '';
                        const arr = buf instanceof Uint8Array ? buf : new Uint8Array(buf);
                        let str = '';
                        let i = 0;
                        const REPLACEMENT_CHAR = '\uFFFD';

                        while (i < arr.length) {{
                            const c = arr[i];

                            // ASCII (0x00-0x7F)
                            if (c < 0x80) {{
                                str += String.fromCharCode(c);
                                i++;
                            }}
                            // 2-byte sequence (0xC0-0xDF)
                            else if (c >= 0xC0 && c < 0xE0) {{
                                if (i + 1 >= arr.length || (arr[i + 1] & 0xC0) !== 0x80) {{
                                    str += REPLACEMENT_CHAR;
                                    i++;
                                    continue;
                                }}
                                const cp = ((c & 0x1F) << 6) | (arr[i + 1] & 0x3F);
                                // Check for overlong encoding
                                if (cp < 0x80) {{
                                    str += REPLACEMENT_CHAR;
                                }} else {{
                                    str += String.fromCharCode(cp);
                                }}
                                i += 2;
                            }}
                            // 3-byte sequence (0xE0-0xEF)
                            else if (c >= 0xE0 && c < 0xF0) {{
                                if (i + 2 >= arr.length || (arr[i + 1] & 0xC0) !== 0x80 || (arr[i + 2] & 0xC0) !== 0x80) {{
                                    str += REPLACEMENT_CHAR;
                                    i++;
                                    continue;
                                }}
                                const cp = ((c & 0x0F) << 12) | ((arr[i + 1] & 0x3F) << 6) | (arr[i + 2] & 0x3F);
                                // Check for overlong encoding and surrogate range
                                if (cp < 0x800 || (cp >= 0xD800 && cp <= 0xDFFF)) {{
                                    str += REPLACEMENT_CHAR;
                                }} else {{
                                    str += String.fromCharCode(cp);
                                }}
                                i += 3;
                            }}
                            // 4-byte sequence (0xF0-0xF7)
                            else if (c >= 0xF0 && c < 0xF8) {{
                                if (i + 3 >= arr.length || (arr[i + 1] & 0xC0) !== 0x80 || (arr[i + 2] & 0xC0) !== 0x80 || (arr[i + 3] & 0xC0) !== 0x80) {{
                                    str += REPLACEMENT_CHAR;
                                    i++;
                                    continue;
                                }}
                                const cp = ((c & 0x07) << 18) | ((arr[i + 1] & 0x3F) << 12) | ((arr[i + 2] & 0x3F) << 6) | (arr[i + 3] & 0x3F);
                                // Check for valid code point range (must be <= 0x10FFFF and >= 0x10000 to avoid overlong)
                                if (cp < 0x10000 || cp > 0x10FFFF) {{
                                    str += REPLACEMENT_CHAR;
                                }} else {{
                                    str += String.fromCodePoint(cp);
                                }}
                                i += 4;
                            }}
                            // Invalid start byte
                            else {{
                                str += REPLACEMENT_CHAR;
                                i++;
                            }}
                        }}
                        return str;
                    }}
                }};
            }}

            // Performance API polyfill
            if (typeof performance === 'undefined') {{
                const startTime = Date.now();
                globalThis.performance = {{
                    now: () => Date.now() - startTime,
                    timeOrigin: startTime,
                }};
            }}

            // Crypto polyfill
            if (typeof crypto === 'undefined') {{
                globalThis.crypto = {{
                    getRandomValues: (arr) => {{
                        for (let i = 0; i < arr.length; i++) {{
                            arr[i] = Math.floor(Math.random() * 256);
                        }}
                        return arr;
                    }},
                    randomUUID: () => {{
                        return 'xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx'.replace(/[xy]/g, (c) => {{
                            const r = Math.random() * 16 | 0;
                            const v = c === 'x' ? r : (r & 0x3 | 0x8);
                            return v.toString(16);
                        }});
                    }},
                }};
            }}

            // Timer polyfills
            if (typeof setTimeout === 'undefined') {{
                let timerId = 0;
                const pendingTimers = new Map();

                globalThis.setTimeout = (fn, delay = 0, ...args) => {{
                    const id = ++timerId;
                    if (delay <= 0) {{
                        queueMicrotask(() => {{
                            if (pendingTimers.has(id)) {{
                                pendingTimers.delete(id);
                                fn(...args);
                            }}
                        }});
                    }} else {{
                        const start = Date.now();
                        const check = () => {{
                            if (!pendingTimers.has(id)) return;
                            if (Date.now() - start >= delay) {{
                                pendingTimers.delete(id);
                                fn(...args);
                            }} else {{
                                queueMicrotask(check);
                            }}
                        }};
                        queueMicrotask(check);
                    }}
                    pendingTimers.set(id, true);
                    return id;
                }};

                globalThis.clearTimeout = (id) => pendingTimers.delete(id);

                globalThis.setInterval = (fn, delay = 0, ...args) => {{
                    const id = ++timerId;
                    pendingTimers.set(id, true);
                    const run = () => {{
                        if (!pendingTimers.has(id)) return;
                        fn(...args);
                        setTimeout(run, delay);
                    }};
                    setTimeout(run, delay);
                    return id;
                }};

                globalThis.clearInterval = (id) => pendingTimers.delete(id);
            }}

            // URL polyfill
            if (typeof URL === 'undefined') {{
                globalThis.URL = class URL {{
                    constructor(url, base) {{
                        let fullUrl = url;
                        if (base && !url.includes('://')) {{
                            const baseStr = base instanceof URL ? base.href : String(base);
                            if (url.startsWith('/')) {{
                                const match = baseStr.match(/^([a-z]+:\/\/[^\/]+)/i);
                                fullUrl = match ? match[1] + url : url;
                            }} else {{
                                fullUrl = baseStr.replace(/\/[^\/]*$/, '/') + url;
                            }}
                        }}
                        this.href = fullUrl;
                        const protoMatch = fullUrl.match(/^([a-z]+):\/\//i);
                        this.protocol = protoMatch ? protoMatch[1] + ':' : '';
                        const rest = fullUrl.replace(/^[a-z]+:\/\//i, '');
                        const pathStart = rest.indexOf('/');
                        this.host = pathStart >= 0 ? rest.substring(0, pathStart) : rest;
                        this.hostname = this.host.split(':')[0];
                        this.port = this.host.includes(':') ? this.host.split(':')[1] : '';
                        this.pathname = pathStart >= 0 ? rest.substring(pathStart).split('?')[0].split('#')[0] : '/';
                        this.search = fullUrl.includes('?') ? '?' + fullUrl.split('?')[1].split('#')[0] : '';
                        this.hash = fullUrl.includes('#') ? '#' + fullUrl.split('#')[1] : '';
                        this.origin = this.protocol + '//' + this.host;
                    }}
                    toString() {{ return this.href; }}
                }};
            }}

            // Blob polyfill
            if (typeof Blob === 'undefined') {{
                globalThis.Blob = class Blob {{
                    constructor(parts = [], options = {{}}) {{
                        this._parts = parts;
                        this.type = options.type || '';
                        let size = 0;
                        for (const part of parts) {{
                            if (part instanceof ArrayBuffer) {{
                                size += part.byteLength;
                            }} else if (part instanceof Uint8Array) {{
                                size += part.byteLength;
                            }} else if (part instanceof Blob) {{
                                size += part.size;
                            }} else if (typeof part === 'string') {{
                                size += new TextEncoder().encode(part).length;
                            }}
                        }}
                        this.size = size;
                    }}

                    async arrayBuffer() {{
                        const chunks = [];
                        for (const part of this._parts) {{
                            if (part instanceof ArrayBuffer) {{
                                chunks.push(new Uint8Array(part));
                            }} else if (part instanceof Uint8Array) {{
                                chunks.push(part);
                            }} else if (part instanceof Blob) {{
                                const buf = await part.arrayBuffer();
                                chunks.push(new Uint8Array(buf));
                            }} else if (typeof part === 'string') {{
                                chunks.push(new TextEncoder().encode(part));
                            }}
                        }}
                        const totalLength = chunks.reduce((sum, chunk) => sum + chunk.length, 0);
                        const result = new Uint8Array(totalLength);
                        let offset = 0;
                        for (const chunk of chunks) {{
                            result.set(chunk, offset);
                            offset += chunk.length;
                        }}
                        return result.buffer;
                    }}

                    async text() {{
                        const buf = await this.arrayBuffer();
                        return new TextDecoder().decode(buf);
                    }}

                    slice(start = 0, end = this.size, type = '') {{
                        return new Blob([this._parts], {{ type }});
                    }}

                    stream() {{
                        throw new Error('Blob.stream() not implemented');
                    }}
                }};
            }}

            // Node.js fs module for Emscripten NODEFS compatibility
            const fsModule = (() => {{
                const {{ ops }} = Deno.core;

                function createStats(stat) {{
                    return {{
                        isFile: () => stat.is_file,
                        isDirectory: () => stat.is_directory,
                        isSymbolicLink: () => stat.is_symlink,
                        isBlockDevice: () => false,
                        isCharacterDevice: () => false,
                        isFIFO: () => false,
                        isSocket: () => false,
                        dev: 0,
                        ino: 0,
                        mode: stat.mode,
                        nlink: 1,
                        uid: 0,
                        gid: 0,
                        rdev: 0,
                        size: stat.size,
                        blksize: 4096,
                        blocks: Math.ceil(stat.size / 512),
                        atimeMs: stat.atime_ms,
                        mtimeMs: stat.mtime_ms,
                        ctimeMs: stat.ctime_ms,
                        birthtimeMs: stat.ctime_ms,
                        atime: new Date(stat.atime_ms),
                        mtime: new Date(stat.mtime_ms),
                        ctime: new Date(stat.ctime_ms),
                        birthtime: new Date(stat.ctime_ms),
                    }};
                }}

                return {{
                    existsSync: (path) => ops.op_fs_exists_sync(path),
                    mkdirSync: (path, options) => {{
                        const recursive = options?.recursive ?? false;
                        ops.op_fs_mkdir_sync(path, recursive);
                    }},
                    readFileSync: (path, options) => {{
                        const data = ops.op_fs_read_file_sync(path);
                        if (options?.encoding === 'utf8' || options?.encoding === 'utf-8') {{
                            return new TextDecoder().decode(data);
                        }}
                        return Buffer.from(data);
                    }},
                    writeFileSync: (path, data, options) => {{
                        let bytes;
                        if (typeof data === 'string') {{
                            bytes = new TextEncoder().encode(data);
                        }} else if (data instanceof Uint8Array) {{
                            bytes = data;
                        }} else {{
                            bytes = new Uint8Array(data);
                        }}
                        ops.op_fs_write_file_sync(path, bytes);
                    }},
                    unlinkSync: (path) => ops.op_fs_unlink_sync(path),
                    rmdirSync: (path) => ops.op_fs_rmdir_sync(path),
                    statSync: (path) => createStats(ops.op_fs_stat_sync(path)),
                    lstatSync: (path) => createStats(ops.op_fs_lstat_sync(path)),
                    fstatSync: (fd) => {{
                        // For Emscripten compatibility, return a minimal stat for fd
                        return createStats({{ is_file: true, is_directory: false, is_symlink: false, size: 0, mode: 0o644, mtime_ms: 0, atime_ms: 0, ctime_ms: 0 }});
                    }},
                    readdirSync: (path) => ops.op_fs_readdir_sync(path),
                    renameSync: (oldPath, newPath) => ops.op_fs_rename_sync(oldPath, newPath),
                    truncateSync: (path, len = 0) => ops.op_fs_truncate_sync(path, BigInt(len)),
                    ftruncateSync: (fd, len = 0) => {{}}, // stub for Emscripten
                    chmodSync: (path, mode) => {{}}, // stub
                    fchmodSync: (fd, mode) => {{}}, // stub
                    chownSync: (path, uid, gid) => {{}}, // stub
                    fchownSync: (fd, uid, gid) => {{}}, // stub
                    utimesSync: (path, atime, mtime) => {{}}, // stub
                    openSync: (path, flags, mode) => {{
                        // Emscripten's NODEFS needs this - return a dummy fd
                        // The actual file ops go through the path-based functions
                        return 999;
                    }},
                    closeSync: (fd) => {{}}, // stub
                    readSync: (fd, buffer, offset, length, position) => {{
                        // stub - Emscripten typically uses path-based ops
                        return 0;
                    }},
                    writeSync: (fd, buffer, offset, length, position) => {{
                        // stub - Emscripten typically uses path-based ops
                        return length || buffer.length;
                    }},
                    fsyncSync: (fd) => {{}}, // stub
                    fdatasyncSync: (fd) => {{}}, // stub
                }};
            }})();

            // Node.js path module
            const pathModule = (() => {{
                const sep = '/';
                const delimiter = ':';

                function join(...parts) {{
                    return parts.filter(p => p && p.length > 0).join(sep).replace(/\/+/g, '/');
                }}

                function normalize(path) {{
                    if (!path) return '.';
                    const isAbsolute = path.startsWith('/');
                    const parts = path.split('/').filter(p => p && p !== '.');
                    const result = [];
                    for (const part of parts) {{
                        if (part === '..') {{
                            if (result.length > 0 && result[result.length - 1] !== '..') {{
                                result.pop();
                            }} else if (!isAbsolute) {{
                                result.push('..');
                            }}
                        }} else {{
                            result.push(part);
                        }}
                    }}
                    let normalized = result.join('/');
                    if (isAbsolute) normalized = '/' + normalized;
                    return normalized || (isAbsolute ? '/' : '.');
                }}

                function dirname(path) {{
                    if (!path) return '.';
                    const lastSlash = path.lastIndexOf('/');
                    if (lastSlash === -1) return '.';
                    if (lastSlash === 0) return '/';
                    return path.slice(0, lastSlash);
                }}

                function basename(path, ext) {{
                    if (!path) return '';
                    let base = path;
                    const lastSlash = path.lastIndexOf('/');
                    if (lastSlash !== -1) base = path.slice(lastSlash + 1);
                    if (ext && base.endsWith(ext)) base = base.slice(0, -ext.length);
                    return base;
                }}

                function resolve(...parts) {{
                    let resolved = '';
                    for (let i = parts.length - 1; i >= 0 && !resolved.startsWith('/'); i--) {{
                        const part = parts[i];
                        if (part && part.length > 0) {{
                            resolved = resolved ? part + '/' + resolved : part;
                        }}
                    }}
                    return normalize(resolved.startsWith('/') ? resolved : '/' + resolved);
                }}

                return {{ join, normalize, dirname, basename, resolve, sep, delimiter }};
            }})();

            // Buffer polyfill for Node.js compatibility
            if (typeof Buffer === 'undefined') {{
                globalThis.Buffer = class Buffer extends Uint8Array {{
                    static from(data, encoding) {{
                        if (typeof data === 'string') {{
                            return new Buffer(new TextEncoder().encode(data));
                        }}
                        if (data instanceof ArrayBuffer) {{
                            return new Buffer(new Uint8Array(data));
                        }}
                        return new Buffer(data);
                    }}
                    static alloc(size, fill = 0) {{
                        const buf = new Buffer(size);
                        buf.fill(fill);
                        return buf;
                    }}
                    static allocUnsafe(size) {{
                        return new Buffer(size);
                    }}
                    static isBuffer(obj) {{
                        return obj instanceof Buffer;
                    }}
                    toString(encoding) {{
                        return new TextDecoder().decode(this);
                    }}
                }};
            }}

            // Make fs and path available globally
            globalThis.fs = fsModule;
            globalThis.path = pathModule;

            // Custom filesystem class implementing BaseFilesystem interface for PGlite
            // This provides proper fd-based file operations for file:// storage
            globalThis.RustFilesystem = class RustFilesystem {{
                constructor(dataDir) {{
                    this.dataDir = dataDir;
                    this.debug = globalThis.__pgliteDebug === true;
                    const {{ ops }} = Deno.core;
                    this.ops = ops;
                    if (this.debug) console.log('[RustFilesystem] Constructor called with dataDir:', dataDir);
                    // Ensure the data directory exists
                    if (!ops.op_fs_exists_sync(dataDir)) {{
                        if (this.debug) console.log('[RustFilesystem] Creating data directory:', dataDir);
                        ops.op_fs_mkdir_sync(dataDir, true);
                    }}
                }}

                // Convert WASM relative path to absolute path
                _toAbsPath(path) {{
                    // PGlite's BaseFilesystem receives paths like /tmp/pglite/base/...
                    // We need to map these to the actual data directory
                    const PGDATA_PREFIX = '/tmp/pglite/base';
                    if (path.startsWith(PGDATA_PREFIX)) {{
                        const result = this.dataDir + path.substring(PGDATA_PREFIX.length);
                        if (this.debug) console.log('[RustFilesystem] _toAbsPath:', path, '->', result);
                        return result;
                    }}
                    if (this.debug) console.log('[RustFilesystem] _toAbsPath (no prefix):', path);
                    return path;
                }}

                chmod(path, mode) {{
                    // No-op on most systems for our use case
                }}

                close(fd) {{
                    this.ops.op_fs_close_sync(fd);
                }}

                fstat(fd) {{
                    const stat = this.ops.op_fs_fstat_sync(fd);
                    return this._toFsStats(stat);
                }}

                lstat(path) {{
                    const absPath = this._toAbsPath(path);
                    const stat = this.ops.op_fs_lstat_sync(absPath);
                    return this._toFsStats(stat);
                }}

                mkdir(path, options) {{
                    const absPath = this._toAbsPath(path);
                    const recursive = options?.recursive ?? false;
                    if (this.debug) console.log('[RustFilesystem] mkdir:', absPath, 'recursive:', recursive);
                    this.ops.op_fs_mkdir_sync(absPath, recursive);
                }}

                open(path, flags = 'r', mode = 0o644) {{
                    const absPath = this._toAbsPath(path);
                    if (this.debug) console.log('[RustFilesystem] open:', absPath, 'flags:', flags);
                    return this.ops.op_fs_open_sync(absPath, flags, mode);
                }}

                readdir(path) {{
                    const absPath = this._toAbsPath(path);
                    return this.ops.op_fs_readdir_sync(absPath);
                }}

                read(fd, buffer, offset, length, position) {{
                    return this.ops.op_fs_read_fd_sync(fd, buffer, offset, length, BigInt(position));
                }}

                rename(oldPath, newPath) {{
                    const absOldPath = this._toAbsPath(oldPath);
                    const absNewPath = this._toAbsPath(newPath);
                    this.ops.op_fs_rename_sync(absOldPath, absNewPath);
                }}

                rmdir(path) {{
                    const absPath = this._toAbsPath(path);
                    this.ops.op_fs_rmdir_sync(absPath);
                }}

                truncate(path, len) {{
                    const absPath = this._toAbsPath(path);
                    this.ops.op_fs_truncate_sync(absPath, BigInt(len));
                }}

                unlink(path) {{
                    const absPath = this._toAbsPath(path);
                    this.ops.op_fs_unlink_sync(absPath);
                }}

                utimes(path, atime, mtime) {{
                    // No-op for now - timestamps not critical for PGlite
                }}

                writeFile(path, data, options) {{
                    const absPath = this._toAbsPath(path);
                    let bytes;
                    if (typeof data === 'string') {{
                        bytes = new TextEncoder().encode(data);
                    }} else if (data instanceof Uint8Array) {{
                        bytes = data;
                    }} else {{
                        bytes = new Uint8Array(data);
                    }}
                    this.ops.op_fs_write_file_sync(absPath, bytes);
                }}

                write(fd, buffer, offset, length, position) {{
                    return this.ops.op_fs_write_fd_sync(fd, buffer, offset, length, BigInt(position));
                }}

                _toFsStats(stat) {{
                    return {{
                        dev: 0,
                        ino: 0,
                        mode: stat.mode,
                        nlink: 1,
                        uid: 0,
                        gid: 0,
                        rdev: 0,
                        size: stat.size,
                        blksize: 4096,
                        blocks: Math.ceil(stat.size / 512),
                        atime: stat.atime_ms / 1000,
                        mtime: stat.mtime_ms / 1000,
                        ctime: stat.ctime_ms / 1000,
                    }};
                }}

                // Required interface methods from Filesystem
                async syncToFs(relaxedDurability) {{}}
                async initialSyncFs() {{}}
                async closeFs() {{}}

                async dumpTar(dbname, compression) {{
                    // Use the instance's dumpTar if available
                    throw new Error('dumpTar should be called on PGlite instance');
                }}

                async init(pg, emscriptenOptions) {{
                    if (this.debug) console.log('[RustFilesystem] init called');
                    this.pg = pg;
                    return {{ emscriptenOpts: emscriptenOptions }};
                }}
            }};

            // CommonJS require for Emscripten compatibility
            globalThis.require = (moduleName) => {{
                if (moduleName === 'fs' || moduleName === 'node:fs') return fsModule;
                if (moduleName === 'path' || moduleName === 'node:path') return pathModule;
                throw new Error('Module not found: ' + moduleName);
            }};

            // Also provide module/exports for CommonJS compatibility
            globalThis.module = {{ exports: {{}} }};
            globalThis.exports = globalThis.module.exports;

            globalThis.__pgliteDataDir = "{}";

            globalThis.__pgliteReadFile = (path) => {{
                return Deno.core.ops.op_read_file(String(path));
            }};

            // Console polyfill
            if (typeof console === 'undefined') {{
                globalThis.console = {{
                    log: (...args) => Deno.core.print(args.map(String).join(' ') + '\\n'),
                    error: (...args) => Deno.core.print('ERROR: ' + args.map(String).join(' ') + '\\n'),
                    warn: (...args) => Deno.core.print('WARN: ' + args.map(String).join(' ') + '\\n'),
                }};
            }}

            // WebAssembly streaming polyfills
            const originalCompile = WebAssembly.compile.bind(WebAssembly);
            const originalInstantiate = WebAssembly.instantiate.bind(WebAssembly);

            WebAssembly.compileStreaming = async (response) => {{
                const resp = await response;
                const buffer = await resp.arrayBuffer();
                return originalCompile(buffer);
            }};

            WebAssembly.instantiateStreaming = async (response, imports) => {{
                const resp = await response;
                const buffer = await resp.arrayBuffer();
                return originalInstantiate(buffer, imports);
            }};

            globalThis.fetch = async (url) => {{
                const urlStr = String(url.href || url);
                let buffer;
                try {{
                    buffer = globalThis.__pgliteReadFile(urlStr);
                }} catch (fetchErr) {{
                    throw fetchErr;
                }}

                let arrayBuffer;
                if (buffer instanceof ArrayBuffer) {{
                    arrayBuffer = buffer;
                }} else if (buffer && buffer.buffer instanceof ArrayBuffer) {{
                    const copy = new ArrayBuffer(buffer.byteLength);
                    new Uint8Array(copy).set(new Uint8Array(buffer.buffer, buffer.byteOffset, buffer.byteLength));
                    arrayBuffer = copy;
                }} else if (buffer && typeof buffer.length === 'number') {{
                    const copy = new ArrayBuffer(buffer.length);
                    const view = new Uint8Array(copy);
                    for (let i = 0; i < buffer.length; i++) view[i] = buffer[i];
                    arrayBuffer = copy;
                }} else {{
                    throw new Error('Invalid buffer type from op_read_file: ' + typeof buffer);
                }}

                const ext = urlStr.split('.').pop()?.toLowerCase();
                const contentType = ext === 'wasm' ? 'application/wasm' :
                                    ext === 'js' ? 'application/javascript' :
                                    ext === 'json' ? 'application/json' :
                                    'application/octet-stream';

                // For .tar.gz files, the Rust op_read_file already decompressed them,
                // so tell PGlite not to decompress again by setting content-encoding: gzip
                // (PGlite checks for this header and skips DecompressionStream when present)
                const isGzipped = urlStr.endsWith('.tar.gz') || urlStr.endsWith('.tgz');
                const headers = new Map([
                    ['content-type', contentType],
                    ['content-length', String(arrayBuffer.byteLength)]
                ]);
                if (isGzipped) {{
                    // PGlite checks Content-Encoding: gzip to decide to use blob() directly
                    // Since we already decompressed, we set this header to skip DecompressionStream
                    headers.set('content-encoding', 'gzip');
                }}
                const responseBlob = new Blob([new Uint8Array(arrayBuffer)], {{ type: contentType }});
                return {{
                    ok: true,
                    status: 200,
                    statusText: 'OK',
                    url: urlStr,
                    // PGlite checks for response.body to determine if fetch succeeded
                    // We provide a minimal stub that makes the check pass
                    body: {{
                        getReader: () => {{ throw new Error('ReadableStream not implemented'); }},
                        pipeThrough: () => {{ throw new Error('pipeThrough not implemented'); }},
                    }},
                    headers: {{
                        get: (name) => headers.get(name.toLowerCase()),
                        has: (name) => headers.has(name.toLowerCase()),
                    }},
                    arrayBuffer: async () => arrayBuffer,
                    text: async () => new TextDecoder().decode(new Uint8Array(arrayBuffer)),
                    json: async () => JSON.parse(new TextDecoder().decode(new Uint8Array(arrayBuffer))),
                    blob: async () => responseBlob,
                    clone: function() {{ return this; }},
                }};
            }};

            globalThis.__pgliteIsReady = false;
            globalThis.__pgliteInitError = undefined;
            "#,
            data_dir_str.replace('\\', "\\\\").replace('"', "\\\""),
            debug_enabled = debug_enabled,
        );

        runtime
            .execute_script("<bootstrap>", bootstrap_code)
            .context("Failed to execute bootstrap code")?;

        debug_log("Bootstrap loaded");

        let module_specifier = ModuleSpecifier::parse("pglite:///index.js")
            .map_err(|e| anyhow::anyhow!("Invalid module specifier: {}", e))?;

        let mod_id = runtime
            .load_main_es_module(&module_specifier)
            .await
            .context("Failed to load PGlite module")?;

        let eval_result = runtime.mod_evaluate(mod_id);
        runtime.run_event_loop(Default::default()).await?;
        eval_result.await?;

        debug_log("PGlite module evaluated");

        // Determine if we should use the custom filesystem
        let use_custom_fs = data_dir_str.starts_with("file://");
        let actual_path = if use_custom_fs {
            data_dir_str.strip_prefix("file://").unwrap_or(&data_dir_str)
        } else {
            &data_dir_str
        };

        let extensions_loader = build_extensions_loader_js(&extensions);
        let init_code = if use_custom_fs {
            format!(
                r#"
                (async () => {{
                    try {{
                        const mod = await import("pglite:///index.js");
                        const fsBase = await import("pglite:///fs/base.js");
                        const {{ createRustBackedFilesystem, extractTar }} = await import("pglite:///shims/rust_backed_filesystem.js");
                        {}
                        globalThis.__pgliteExtensionNames = __pgliteRequestedExtensions;

                        const {{ ops }} = Deno.core;
                        const dataDir = "{}";

                        if (!ops.op_fs_exists_sync(dataDir)) {{
                            ops.op_fs_mkdir_sync(dataDir, true);
                        }}

                        const RustBackedFilesystem = createRustBackedFilesystem(fsBase.BaseFilesystem, ops, dataDir);
                        const customFs = new RustBackedFilesystem();

                        const pgVersionPath = dataDir + '/PG_VERSION';
                        const dbExists = ops.op_fs_exists_sync(pgVersionPath);

                        if (!dbExists) {{
                            const seedResp = await fetch('pglite:///pgdata_seed.tar');
                            const tarData = new Uint8Array(await seedResp.arrayBuffer());
                            extractTar(tarData, dataDir, ops);
                        }}

                        const pg = await mod.PGlite.create({{ fs: customFs, extensions: __pgliteExtensions }});
                        globalThis.__pgliteInstance = pg;
                        globalThis.__pgliteIsReady = true;
                    }} catch (err) {{
                        globalThis.__pgliteInitError = String(err);
                        throw err;
                    }}
                }})();
                "#,
                extensions_loader,
                actual_path.replace('\\', "\\\\").replace('"', "\\\"")
            )
        } else {
            format!(
                r#"
                (async () => {{
                    try {{
                        const mod = await import("pglite:///index.js");
                        {}
                        globalThis.__pgliteExtensionNames = __pgliteRequestedExtensions;
                        const options = {{ dataDir: "{}" }};
                        if (__pgliteExtensions) {{
                            options.extensions = __pgliteExtensions;
                        }}
                        const pg = await mod.PGlite.create(options);
                        globalThis.__pgliteInstance = pg;
                        globalThis.__pgliteIsReady = true;
                    }} catch (err) {{
                        globalThis.__pgliteInitError = String(err);
                        throw err;
                    }}
                }})();
                "#,
                extensions_loader,
                data_dir_str.replace('\\', "\\\\").replace('"', "\\\"")
            )
        };

        runtime
            .execute_script("<init>", init_code)
            .context("Failed to start PGlite initialization")?;

        debug_log("Init script executed");

        // Run the event loop to completion - this processes all async work including
        // dynamic imports used by extensions. The run_event_loop() call will return
        // when all promises have settled (either resolved or rejected).
        runtime.run_event_loop(PollEventLoopOptions::default()).await?;

        debug_log("Init event loop finished");

        // Check for initialization errors
        let error_global = runtime
            .execute_script("<check_error>", "globalThis.__pgliteInitError")
            .context("Failed to check error")?;
        let error: Option<String> = extract_value(&mut runtime, error_global)?;
        if let Some(error) = error {
            return Err(anyhow::anyhow!("PGlite init error: {}", error));
        }

        // Verify initialization succeeded
        let ready_global = runtime
            .execute_script("<check_ready>", "globalThis.__pgliteIsReady")
            .context("Failed to check ready state")?;
        let ready: bool = extract_value(&mut runtime, ready_global)?;
        if !ready {
            return Err(anyhow::anyhow!("PGlite initialization did not complete"));
        }

        runtime
            .execute_script(
                "<exec_fn>",
                r#"
                globalThis.__pgliteExecCount = 0;
                globalThis.__pgliteExec = (input) => {
                    const count = ++globalThis.__pgliteExecCount;
                    try {
                        const pg = globalThis.__pgliteInstance;
                        // input is already a Uint8Array from V8
                        const output = pg.execProtocolRawSync(input);
                        // Return Uint8Array directly - no Array.from() conversion needed
                        if (output instanceof Uint8Array) {
                            return output;
                        }
                        // If output is a different typed array view, create a proper Uint8Array
                        return new Uint8Array(output.buffer, output.byteOffset, output.byteLength);
                    } catch (e) {
                        console.error(`[JS] __pgliteExec #${count} error:`, String(e));
                        throw e;
                    }
                };
                "#,
            )
            .context("Failed to define exec function")?;

        let _ = ready_tx.send(Ok(()));

        // Exit the async context to run the message loop synchronously
        // This avoids nested runtime issues
        Ok::<JsRuntime, anyhow::Error>(runtime)
    });

    let mut runtime = match result {
        Ok(rt) => rt,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    // Message loop runs outside async context to avoid nested runtime issues
    loop {
        match receiver.recv() {
            Ok(JsRequest::Exec(payload, response_tx)) => {
                let result = exec_wire_message(&mut runtime, payload);
                let _ = response_tx.send(result);
                runtime.v8_isolate().perform_microtask_checkpoint();
            }
            Ok(JsRequest::ExecAsync(payload, response_tx)) => {
                let result = exec_wire_message(&mut runtime, payload);
                let _ = response_tx.send(result);
                runtime.v8_isolate().perform_microtask_checkpoint();
            }
            Ok(JsRequest::DumpDataDir(response_tx)) => {
                let result = dump_data_dir_sync(&mut runtime, &tokio_rt, &extensions);
                let _ = response_tx.send(result);
            }
            Ok(JsRequest::ExecSql(sql, response_tx)) => {
                let result = exec_sql(&mut runtime, &sql);
                let _ = response_tx.send(result);
                runtime.v8_isolate().perform_microtask_checkpoint();
            }
            Ok(JsRequest::Shutdown) | Err(_) => break,
        }
    }
}

#[inline]
fn exec_wire_message(runtime: &mut JsRuntime, payload: Vec<u8>) -> Result<Vec<u8>> {
    // Use V8's ArrayBuffer directly - zero-copy since we own the Vec
    let payload_len = payload.len();
    deno_core::scope!(scope, runtime);

    // Create ArrayBuffer using the owned Vec directly - no copy!
    let backing_store = v8::ArrayBuffer::new_backing_store_from_vec(payload).make_shared();
    let array_buffer = v8::ArrayBuffer::with_backing_store(scope, &backing_store);
    let uint8_array = v8::Uint8Array::new(scope, array_buffer, 0, payload_len)
        .ok_or_else(|| anyhow::anyhow!("Failed to create Uint8Array"))?;

    // Get the exec function from global
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, "__pgliteExec").unwrap();
    let exec_fn = global
        .get(scope, key.into())
        .ok_or_else(|| anyhow::anyhow!("__pgliteExec not found"))?;
    let exec_fn = v8::Local::<v8::Function>::try_from(exec_fn)
        .map_err(|_| anyhow::anyhow!("__pgliteExec is not a function"))?;

    // Call with the Uint8Array directly
    let undefined = v8::undefined(scope);
    let args = [uint8_array.into()];
    let result = exec_fn
        .call(scope, undefined.into(), &args)
        .ok_or_else(|| anyhow::anyhow!("__pgliteExec call failed (JS exception or undefined)"))?;

    // Extract result - expect Uint8Array or ArrayBuffer
    if result.is_uint8_array() {
        let arr = v8::Local::<v8::Uint8Array>::try_from(result)
            .map_err(|e| anyhow::anyhow!("Failed to convert to Uint8Array: {:?}", e))?;
        let len = arr.byte_length();
        let mut bytes = vec![0u8; len];
        arr.copy_contents(&mut bytes);
        Ok(bytes)
    } else if result.is_array_buffer() {
        let ab = v8::Local::<v8::ArrayBuffer>::try_from(result)
            .map_err(|e| anyhow::anyhow!("Failed to convert to ArrayBuffer: {:?}", e))?;
        let len = ab.byte_length();
        let mut bytes = vec![0u8; len];
        if let Some(data) = ab.data() {
            // SAFETY: We're copying from valid V8 ArrayBuffer memory within the scope
            unsafe {
                std::ptr::copy_nonoverlapping(data.as_ptr() as *const u8, bytes.as_mut_ptr(), len);
            }
        }
        Ok(bytes)
    } else if result.is_array_buffer_view() {
        let view = v8::Local::<v8::ArrayBufferView>::try_from(result)
            .map_err(|e| anyhow::anyhow!("Failed to convert to ArrayBufferView: {:?}", e))?;
        let len = view.byte_length();
        let mut bytes = vec![0u8; len];
        view.copy_contents(&mut bytes);
        Ok(bytes)
    } else {
        // Fallback to serde deserialization for arrays (legacy path)
        serde_v8::from_v8(scope, result).context("Failed to deserialize V8 value")
    }
}

fn exec_sql(runtime: &mut JsRuntime, sql: &str) -> Result<()> {
    // Escape the SQL for JavaScript string
    let escaped_sql = sql
        .replace('\\', "\\\\")
        .replace('`', "\\`")
        .replace('$', "\\$");

    let code = format!(
        r#"
        globalThis.__execSqlError = null;
        globalThis.__execSqlDone = false;
        (async () => {{
            try {{
                const pg = globalThis.__pgliteInstance;
                await pg.exec(`{}`);
                globalThis.__execSqlDone = true;
            }} catch (e) {{
                globalThis.__execSqlError = String(e);
                globalThis.__execSqlDone = true;
            }}
        }})();
        "#,
        escaped_sql
    );

    runtime
        .execute_script("<exec_sql>", code)
        .context("Failed to start SQL execution")?;

    // Run event loop to completion
    let tokio_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("Failed to create tokio runtime for exec_sql")?;

    tokio_rt
        .block_on(async {
            runtime
                .run_event_loop(PollEventLoopOptions::default())
                .await
        })
        .context("Failed to run event loop for exec_sql")?;

    // Check for errors
    let error_global = runtime
        .execute_script("<check_sql_error>", "globalThis.__execSqlError")
        .context("Failed to check SQL error")?;
    let error: Option<String> = extract_value(runtime, error_global)?;
    if let Some(error) = error {
        return Err(anyhow::anyhow!("SQL execution failed: {}", error));
    }

    Ok(())
}

fn dump_data_dir_sync(
    runtime: &mut JsRuntime,
    tokio_rt: &tokio::runtime::Runtime,
    extensions: &[String],
) -> Result<Vec<u8>> {
    if !extensions.is_empty() {
        let install_code = format!(
            r#"
            globalThis.__seedExtensionError = null;
            (async () => {{
                try {{
                    const pg = globalThis.__pgliteInstance;
                    const names = {};
                    for (const name of names) {{
                        if (name === 'live') continue;
                        await pg.exec(`CREATE EXTENSION IF NOT EXISTS "${{name}}"`);
                    }}
                }} catch (e) {{
                    globalThis.__seedExtensionError = String(e);
                }}
            }})();
            "#,
            serde_json::to_string(extensions).unwrap_or_else(|_| "[]".to_string())
        );
        runtime
            .execute_script("<seed_extensions>", install_code)
            .context("Failed to start extension install")?;

        tokio_rt.block_on(async { runtime.run_event_loop(Default::default()).await })?;

        let error_global = runtime
            .execute_script("<seed_extensions_error>", "globalThis.__seedExtensionError")
            .context("Failed to check extension install error")?;
        let error: Option<String> = extract_value(runtime, error_global)?;
        if let Some(e) = error {
            return Err(anyhow::anyhow!("Extension install error: {}", e));
        }
    }

    runtime
        .execute_script(
            "<dump_setup>",
            r#"
            globalThis.__dumpResult = null;
            globalThis.__dumpError = null;
            globalThis.__dumpDone = false;

            (async () => {
                try {
                    const pg = globalThis.__pgliteInstance;
                    const blob = await pg.dumpDataDir('none');
                    const arrayBuffer = await blob.arrayBuffer();
                    // Store as Uint8Array instead of Array for faster extraction
                    globalThis.__dumpResult = new Uint8Array(arrayBuffer);
                } catch (e) {
                    globalThis.__dumpError = String(e);
                } finally {
                    globalThis.__dumpDone = true;
                }
            })();
            "#,
        )
        .context("Failed to start dump")?;

    // Use tokio runtime to properly run the event loop for async operations
    tokio_rt.block_on(async { runtime.run_event_loop(Default::default()).await })?;

    let error_global = runtime
        .execute_script("<dump_error>", "globalThis.__dumpError")
        .context("Failed to check dump error")?;
    let error: Option<String> = extract_value(runtime, error_global)?;

    if let Some(e) = error {
        return Err(anyhow::anyhow!("Dump error: {}", e));
    }

    // Extract result using proper TypedArray handling
    deno_core::scope!(scope, runtime);
    let result_global = {
        let key = v8::String::new(scope, "__dumpResult").unwrap();
        let global = scope.get_current_context().global(scope);
        global.get(scope, key.into())
    };

    if let Some(result) = result_global {
        if result.is_uint8_array() {
            let arr = v8::Local::<v8::Uint8Array>::try_from(result)
                .map_err(|e| anyhow::anyhow!("Failed to convert dump result: {:?}", e))?;
            let len = arr.byte_length();
            let mut bytes = vec![0u8; len];
            arr.copy_contents(&mut bytes);
            return Ok(bytes);
        }
    }

    Err(anyhow::anyhow!("Dump result not available"))
}

fn normalize_data_dir(data_dir: &str) -> String {
    if data_dir.is_empty() {
        "memory://".to_string()
    } else if data_dir.starts_with("memory://") || data_dir.starts_with("file://") {
        data_dir.to_string()
    } else if data_dir.starts_with('/') {
        // Absolute filesystem path - use file:// for persistent storage
        format!("file://{}", data_dir)
    } else {
        // Named in-memory database
        format!("memory://{}", data_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::{build_extensions_loader_js, normalize_asset_path};

    #[test]
    fn build_extensions_loader_js_empty() {
        let code = build_extensions_loader_js(&[]);

        assert!(code.contains("const __pgliteExtensions = undefined"));
    }

    #[test]
    fn build_extensions_loader_js_includes_requested() {
        let code = build_extensions_loader_js(&["pg_trgm".to_string(), "vector".to_string()]);

        assert!(code.contains("\"pg_trgm\""));
        assert!(code.contains("\"vector\""));
        assert!(code.contains("pglite:///vector/index.js"));
        assert!(code.contains("contrib"));
    }

    #[test]
    fn normalize_asset_path_resolves_dot_segments() {
        let path = normalize_asset_path("contrib/../pg_trgm.tar.gz");

        assert_eq!(path, "pg_trgm.tar.gz");
    }
}
