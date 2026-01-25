# Runtime Migration Progress

Branch: `explore-runtime-alternatives`

## Overview

This document tracks the exploration of replacing the original Wasmtime runtime with a JavaScript-based approach using the PGlite npm package and V8 via deno_core.

## Goal

Simplify the architecture by:
1. Using the official `@electric-sql/pglite` npm package instead of building custom WASM artifacts
2. Leveraging V8 (via deno_core) as the JavaScript runtime with full WebAssembly support
3. Reducing build complexity and asset management overhead

## Final Solution: V8 via deno_core

After exploring QuickJS (which lacks WebAssembly support), we successfully implemented the runtime using **deno_core 0.382.0** which provides:
- Full V8 JavaScript engine
- Native WebAssembly support
- ES module loading

## Progress

### Completed

- [x] Migrated `build.rs` to fetch `@electric-sql/pglite@0.3.15` from npm registry
- [x] Extracted npm package dist folder to `assets/pglite_npm/dist/`
- [x] Implemented `js_runtime.rs` with V8 runtime using `deno_core` crate
- [x] Created ES module loader/resolver for PGlite imports
- [x] Added polyfills for missing Web APIs:
  - `TextEncoder` / `TextDecoder` (UTF-8 with encodeInto support)
  - `URL` class
  - `console` object
  - `crypto.getRandomValues`
  - `performance.now()`
  - `setTimeout` / `setInterval` (via queueMicrotask)
  - `fetch` (delegates to Rust file reader)
  - `WebAssembly.compileStreaming` / `instantiateStreaming` (fallback to non-streaming)
- [x] Simplified CLI arguments (just `<data_dir> <tcp_port>`)
- [x] Maintained wire protocol handling from original implementation
- [x] All integration tests passing

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                     pglited binary                          │
├─────────────────────────────────────────────────────────────┤
│  TCP Server (tokio)                                         │
│    ↓                                                        │
│  Wire Protocol Handler                                      │
│    ↓                                                        │
│  PgliteRuntime (channel-based message passing)              │
│    ↓                                                        │
│  deno_core JsRuntime (V8)                                   │
│    ↓                                                        │
│  PGlite npm package (JavaScript)                            │
│    ↓                                                        │
│  pglite.wasm (PostgreSQL WebAssembly)                       │
└─────────────────────────────────────────────────────────────┘
```

## Files Changed

| File | Status | Description |
|------|--------|-------------|
| `Cargo.toml` | Modified | Uses deno_core 0.382, deno_error 0.7.3 |
| `build.rs` | Modified | Downloads npm tarball instead of building WASM |
| `src/lib.rs` | Modified | WireProcessor trait, async executor |
| `src/main.rs` | Modified | Minor adjustments for new runtime |
| `src/js_runtime.rs` | Added | V8/deno_core runtime implementation |
| `src/assets.rs` | Deleted | Wasmtime-specific asset handling |
| `src/bin/build_artifacts.rs` | Deleted | No longer needed |

## Dependencies

### Current (V8/deno_core approach)
```toml
[dependencies]
anyhow = "1.0"
once_cell = "1.19"
deno_core = "0.382"
deno_error = "0.7.3"
serde = "1.0"
serde_json = "1.0"
tokio = { version = "1.40", features = ["full", "rt-multi-thread", "macros"] }

[build-dependencies]
ureq = "2.9"
tar = "0.4"
flate2 = "1.0"
```

## Testing

All integration tests pass:
- `test_binary_starts_and_binds_port` ✅
- `test_multiple_instances_different_ports` ✅
- `test_persistent_storage_mode` ✅
- `test_ready_signal_format` ✅

## Journey: QuickJS → V8

### QuickJS Attempt (Failed)

Initially attempted to use QuickJS via `rquickjs` crate because:
- Lightweight (~600KB)
- Fast compilation
- Simple API

**Blocker**: QuickJS does not support WebAssembly execution. PGlite requires WebAssembly to run PostgreSQL.

### V8 Solution (Success)

Switched to V8 via `deno_core` which provides:
- Full WebAssembly support
- Browser-compatible JavaScript environment
- ES module loading
- Well-maintained by the Deno team

**Trade-offs**:
- Larger binary size (V8 is ~50MB)
- Longer compile times
- More complex integration

## Key Implementation Details

### Module Loading

Custom `ModuleLoader` trait implementation handles:
- Resolving relative imports (`./`, `../`)
- Loading files from the npm package's dist folder
- Converting file paths to module specifiers

### Web API Polyfills

Since deno_core is a minimal V8 wrapper, we needed to polyfill:
- **TextEncoder/TextDecoder**: Full UTF-8 support including `encodeInto()`
- **fetch**: Redirects to Rust file reader op for loading WASM/data files
- **WebAssembly streaming**: Falls back to non-streaming since our Response objects aren't real
- **Timers**: Using `queueMicrotask` for setTimeout/setInterval

### Thread Architecture

```
Main Thread                    JS Runtime Thread
    │                               │
    ├──spawn_blocking──────────────►│
    │                               │ run_js_runtime()
    │                               │   ├── Initialize V8
    │                               │   ├── Load polyfills
    │                               │   ├── Load PGlite module
    │                               │   ├── Create PGlite instance
    │◄──ready_tx.send(Ok(()))───────┤   │
    │                               │   └── while let recv() { exec_wire_message }
    │──JsRequest::Exec─────────────►│
    │◄──response_tx.send()──────────│
    │                               │
```

## Startup Performance

### pgdata_seed No Longer Required

The original Wasmtime implementation required a `pgdata_seed.tar.zst` file to achieve fast startup times:
- **With pgdata_seed**: ~2-9 seconds
- **Without pgdata_seed**: ~10-15 seconds (runs full initdb)

The new V8/deno_core approach using the PGlite npm package has **built-in optimized initialization**:
- **Current startup time**: ~1 second
- No external seed files required
- PGlite npm package bundles an optimized WASM module and initialization

### Optional: loadDataDir for Even Faster Startup

For use cases requiring sub-second initialization, PGlite supports `loadDataDir`:

```javascript
const pg = await PGlite.create({
  dataDir: "memory://",
  loadDataDir: preloadedDatabaseBlob,  // Tarball from dumpDataDir()
});
```

This can be used to:
1. Pre-initialize a database with schema/data
2. Skip first-run initialization entirely
3. Achieve consistent startup times

To generate a loadDataDir tarball:
```javascript
const tarball = await pg.dumpDataDir();
```

### Performance Comparison

| Approach | Startup Time | Complexity |
|----------|-------------|------------|
| Old Wasmtime (no seed) | ~10-15s | High |
| Old Wasmtime (with pgdata_seed) | ~2-9s | High |
| New V8/deno_core | ~1s | Low |
| New V8 + loadDataDir | <1s | Medium |

## Validation Against ex_pglite

### Test Results

All 24 ex_pglite integration tests pass with the new V8/deno_core implementation:

```
Running ExUnit with seed: 820154, max_cases: 3
........................
Finished in 17.9 seconds (17.9s async, 0.00s sync)
24 tests, 0 failures
```

### Benchmark Results

| Intensity | Target ops/s | Actual ops | Errors | Avg Latency | Memory | Status |
|-----------|--------------|------------|--------|-------------|--------|--------|
| Low (10/s) | 10 | 94 | 0% | 2.86ms | 575 MB | ✅ Works |
| Medium (100/s) | 100 | 33 | 24% | 1815ms | 567 MB | ⚠️ Stalls |

**Key Metrics (Low Intensity - 30s benchmark):**
- Schema setup: 85ms (52ms DDL + 20ms seed)
- Latency: Min 1.08ms, Max 15.37ms, P50 2.24ms, P95 7.61ms
- Sustained throughput: ~3.13 ops/sec
- Memory per instance: ~567-575 MB

### Known Issue: High-Rate Query Stalling

At higher query rates (medium/high intensity), the benchmark stalls after ~30-33 operations with Postgrex timeout errors:

```
Postgrex.Protocol disconnected: client timed out because it queued and
checked out the connection for longer than 15000ms
```

**Observations:**
- Sequential queries from psql work reliably (tested 30+ queries)
- Postgrex extended query protocol works for individual connections
- Issue manifests at higher throughput (~100 ops/sec target)
- Exactly 33 operations complete before stall in medium intensity tests

### Performance Regression vs Previous Implementation

The previous Wasmtime implementation achieved higher sustained throughput. This regression needs investigation:

| Metric | Previous (Wasmtime) | Current (V8/deno_core) |
|--------|---------------------|------------------------|
| Startup time | 2-9s (with seed) | ~1s |
| Low intensity | Stable | ✅ Stable |
| Medium intensity | Stable | ⚠️ Stalls after ~33 ops |
| High intensity | Stable | ❌ Not tested |

### Areas for Investigation

1. **Wire Protocol Handling**
   - The `execProtocolRawSync` may have blocking behavior at high rates
   - Need to investigate if PGlite's sync execution causes V8 event loop issues
   - Consider async wire protocol handling

2. **V8 Event Loop**
   - Current implementation uses synchronous `execute_script` for query execution
   - May need to pump the event loop between queries
   - Consider using `run_event_loop` periodically

3. **Channel Back-pressure**
   - The mpsc channel between TCP handler and JS runtime may cause blocking
   - Consider bounded channels with back-pressure handling

4. **Postgrex Compatibility**
   - Extended query protocol (Parse/Bind/Execute) may have subtle differences
   - Consider adding protocol-level debugging

## Potential Future Improvements

1. **Add deno_web extension**: Would provide native TextEncoder, URL, etc.
2. **Add deno_console extension**: Proper console implementation
3. **Pre-compiled WASM module**: Use `wasmModule` option for even faster startup
4. **Reduce polyfills**: Use more deno extensions for native implementations
5. **Investigate wire protocol performance**: Address the high-rate query stalling issue
6. **Add async query execution**: Consider making wire protocol handling async-aware
