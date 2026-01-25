use anyhow::{Context, Result};
use deno_core::{
    extension, serde_v8, v8, FastString, JsRuntime, ModuleLoadOptions, ModuleLoadReferrer,
    ModuleLoadResponse, ModuleLoader, ModuleSource, ModuleSourceCode, ModuleSpecifier, ModuleType,
    ResolutionKind, RuntimeOptions,
};
use deno_error::JsErrorBox;
use rust_embed::Embed;
use serde::de::DeserializeOwned;
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::{PgliteConfig, WireProcessor};

#[derive(Embed)]
#[folder = "assets/pglite_npm/dist"]
#[include = "*.js"]
#[include = "*.wasm"]
#[include = "*.data"]
#[include = "*.tar.gz"]
#[include = "*.tar"]
struct PgliteAssets;

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
    DumpDataDir(mpsc::Sender<Result<Vec<u8>>>),
    Shutdown,
}

impl PgliteRuntime {
    pub fn new(config: PgliteConfig) -> Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let data_dir = config.data_dir.clone();

        thread::spawn(move || {
            run_js_runtime(receiver, &data_dir, ready_tx);
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

    pub fn init_postgres(&mut self) -> Result<()> {
        Ok(())
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

fn get_embedded_asset(path: &str) -> Option<std::borrow::Cow<'static, [u8]>> {
    let mut normalized = path
        .strip_prefix("pglite:///")
        .or_else(|| path.strip_prefix("pglite://"))
        .or_else(|| path.strip_prefix("file://"))
        .unwrap_or(path);

    normalized = normalized.strip_prefix('/').unwrap_or(normalized);
    normalized = normalized.strip_prefix("./").unwrap_or(normalized);

    while normalized.contains("./") {
        normalized = normalized.strip_prefix("./").unwrap_or(normalized);
    }

    PgliteAssets::get(normalized)
        .or_else(|| {
            let without_dist = normalized.strip_prefix("dist/").unwrap_or(normalized);
            PgliteAssets::get(without_dist)
        })
        .map(|f| f.data)
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
    path.strip_prefix('/').unwrap_or(path).to_string()
}

impl ModuleLoader for EmbeddedModuleLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: ResolutionKind,
    ) -> Result<ModuleSpecifier, deno_core::error::ModuleLoaderError> {
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

        ModuleLoadResponse::Sync(Ok(ModuleSource::new(
            ModuleType::JavaScript,
            ModuleSourceCode::String(FastString::from(code)),
            &specifier,
            None,
        )))
    }
}

#[deno_core::op2]
#[buffer]
fn op_read_file(#[string] path: String) -> std::result::Result<Vec<u8>, std::io::Error> {
    get_embedded_asset(&path)
        .map(|data| data.to_vec())
        .ok_or_else(|| {
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

extension!(pglite_ext, ops = [op_read_file, op_exec_wire],);

fn run_js_runtime(
    receiver: mpsc::Receiver<JsRequest>,
    data_dir: &str,
    ready_tx: mpsc::Sender<Result<()>>,
) {
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
        let mut runtime = JsRuntime::new(RuntimeOptions {
            extensions: vec![pglite_ext::init()],
            module_loader: Some(Rc::new(EmbeddedModuleLoader)),
            ..Default::default()
        });

        let data_dir_str = normalize_data_dir(data_dir);

        let bootstrap_code = format!(
            r#"
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

            // TextDecoder polyfill (UTF-8 only)
            if (typeof TextDecoder === 'undefined') {{
                globalThis.TextDecoder = class TextDecoder {{
                    constructor(encoding = 'utf-8') {{ this.encoding = encoding; }}
                    decode(buf) {{
                        const arr = buf instanceof Uint8Array ? buf : new Uint8Array(buf);
                        let str = '';
                        let i = 0;
                        while (i < arr.length) {{
                            let c = arr[i++];
                            if (c < 128) {{
                                str += String.fromCharCode(c);
                            }} else if (c < 224) {{
                                str += String.fromCharCode(((c & 31) << 6) | (arr[i++] & 63));
                            }} else if (c < 240) {{
                                str += String.fromCharCode(((c & 15) << 12) | ((arr[i++] & 63) << 6) | (arr[i++] & 63));
                            }} else {{
                                const cp = ((c & 7) << 18) | ((arr[i++] & 63) << 12) | ((arr[i++] & 63) << 6) | (arr[i++] & 63);
                                str += String.fromCodePoint(cp);
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
                const buffer = globalThis.__pgliteReadFile(urlStr);

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

                const headers = new Map([['content-type', contentType], ['content-length', String(arrayBuffer.byteLength)]]);
                return {{
                    ok: true,
                    status: 200,
                    statusText: 'OK',
                    url: urlStr,
                    headers: {{
                        get: (name) => headers.get(name.toLowerCase()),
                        has: (name) => headers.has(name.toLowerCase()),
                    }},
                    arrayBuffer: async () => arrayBuffer,
                    text: async () => new TextDecoder().decode(new Uint8Array(arrayBuffer)),
                    json: async () => JSON.parse(new TextDecoder().decode(new Uint8Array(arrayBuffer))),
                    blob: async () => ({{ arrayBuffer: async () => arrayBuffer, type: contentType }}),
                    clone: function() {{ return this; }},
                }};
            }};

            globalThis.__pgliteIsReady = false;
            globalThis.__pgliteInitError = undefined;
            "#,
            data_dir_str.replace('\\', "\\\\").replace('"', "\\\""),
        );

        runtime
            .execute_script("<bootstrap>", bootstrap_code)
            .context("Failed to execute bootstrap code")?;

        let module_specifier = ModuleSpecifier::parse("pglite:///index.js")
            .map_err(|e| anyhow::anyhow!("Invalid module specifier: {}", e))?;

        let mod_id = runtime
            .load_main_es_module(&module_specifier)
            .await
            .context("Failed to load PGlite module")?;

        let eval_result = runtime.mod_evaluate(mod_id);
        runtime.run_event_loop(Default::default()).await?;
        eval_result.await?;

        let init_code = format!(
            r#"
            (async () => {{
                try {{
                    const mod = await import("pglite:///index.js");
                    const options = {{ dataDir: "{}" }};

                    // pgdata_seed loading disabled - PGlite's built-in init is faster
                    // To re-enable, uncomment the following:
                    // try {{
                    //     const seedResponse = await fetch("pglite:///pgdata_seed.tar");
                    //     if (seedResponse.ok) {{
                    //         const seedBuffer = await seedResponse.arrayBuffer();
                    //         options.loadDataDir = new Blob([seedBuffer], {{ type: 'application/x-tar' }});
                    //     }}
                    // }} catch (e) {{
                    //     // pgdata_seed not available, will run initdb
                    // }}

                    const pg = await mod.PGlite.create(options);
                    globalThis.__pgliteInstance = pg;
                    globalThis.__pgliteIsReady = true;
                }} catch (err) {{
                    globalThis.__pgliteInitError = String(err);
                    throw err;
                }}
            }})();
            "#,
            data_dir_str.replace('\\', "\\\\").replace('"', "\\\"")
        );

        runtime
            .execute_script("<init>", init_code)
            .context("Failed to start PGlite initialization")?;

        runtime.run_event_loop(Default::default()).await?;

        let ready_global = runtime
            .execute_script("<check_ready>", "globalThis.__pgliteIsReady")
            .context("Failed to check ready state")?;
        let ready: bool = extract_value(&mut runtime, ready_global)?;

        if !ready {
            let error_global = runtime
                .execute_script(
                    "<check_error>",
                    "globalThis.__pgliteInitError || 'Unknown error'",
                )
                .context("Failed to check error")?;
            let error: String = extract_value(&mut runtime, error_global)?;
            return Err(anyhow::anyhow!("PGlite init error: {}", error));
        }

        runtime
            .execute_script(
                "<exec_fn>",
                r#"
                globalThis.__pgliteExec = (buffer) => {
                    const pg = globalThis.__pgliteInstance;
                    const input = new Uint8Array(buffer);
                    const output = pg.execProtocolRawSync(input);
                    return Array.from(new Uint8Array(output.buffer.slice(output.byteOffset, output.byteOffset + output.byteLength)));
                };
                "#,
            )
            .context("Failed to define exec function")?;

        let _ = ready_tx.send(Ok(()));

        loop {
            match receiver.recv() {
                Ok(JsRequest::Exec(payload, response_tx)) => {
                    let result = exec_wire_message(&mut runtime, &payload);
                    let _ = response_tx.send(result);
                }
                Ok(JsRequest::DumpDataDir(response_tx)) => {
                    let result = dump_data_dir_sync(&mut runtime);
                    let _ = response_tx.send(result);
                }
                Ok(JsRequest::Shutdown) | Err(_) => break,
            }
        }

        Ok::<(), anyhow::Error>(())
    });

    if let Err(e) = result {
        let _ = ready_tx.send(Err(e));
    }
}

fn exec_wire_message(runtime: &mut JsRuntime, payload: &[u8]) -> Result<Vec<u8>> {
    let payload_array = format!(
        "new Uint8Array([{}]).buffer",
        payload
            .iter()
            .map(|b| b.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );

    let exec_code = format!("globalThis.__pgliteExec({})", payload_array);

    let result_global = runtime
        .execute_script("<exec>", exec_code)
        .context("Failed to execute wire message")?;

    let bytes: Vec<u8> = extract_value(runtime, result_global)?;
    Ok(bytes)
}

fn dump_data_dir_sync(runtime: &mut JsRuntime) -> Result<Vec<u8>> {
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
                    globalThis.__dumpResult = Array.from(new Uint8Array(arrayBuffer));
                } catch (e) {
                    globalThis.__dumpError = String(e);
                } finally {
                    globalThis.__dumpDone = true;
                }
            })();
            "#,
        )
        .context("Failed to start dump")?;

    loop {
        let done_global = runtime
            .execute_script("<dump_check>", "globalThis.__dumpDone")
            .context("Failed to check dump status")?;
        let done: bool = extract_value(runtime, done_global)?;

        if done {
            break;
        }

        runtime.execute_script("<pump>", "0").ok();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let error_global = runtime
        .execute_script("<dump_error>", "globalThis.__dumpError")
        .context("Failed to check dump error")?;
    let error: Option<String> = extract_value(runtime, error_global)?;

    if let Some(e) = error {
        return Err(anyhow::anyhow!("Dump error: {}", e));
    }

    let result_global = runtime
        .execute_script("<dump_result>", "globalThis.__dumpResult")
        .context("Failed to get dump result")?;

    let bytes: Vec<u8> = extract_value(runtime, result_global)?;
    Ok(bytes)
}

fn normalize_data_dir(data_dir: &str) -> String {
    if data_dir.is_empty() {
        "memory://".to_string()
    } else if data_dir.starts_with("memory://") || data_dir.starts_with("file://") {
        data_dir.to_string()
    } else {
        format!("memory://{}", data_dir)
    }
}
