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

### Performance Fixes Applied (January 2025)

The following performance issues were identified and fixed:

#### 1. TextDecoder Polyfill Bug (Critical)
**Symptom**: System would stall after ~240 wire protocol operations with `RangeError: Invalid code point 1572864`

**Root Cause**: The TextDecoder polyfill didn't validate UTF-8 byte sequences, allowing invalid code points (> 0x10FFFF) to be passed to `String.fromCodePoint()`.

**Fix**: Rewrote TextDecoder polyfill with proper UTF-8 validation:
- Validates continuation bytes in multi-byte sequences
- Checks for overlong encodings
- Validates code point ranges (must be ≤ 0x10FFFF)
- Replaces invalid sequences with Unicode replacement character (U+FFFD)

#### 2. V8 ArrayBuffer Zero-Copy Buffer Passing
**Previous**: Building JavaScript strings like `new Uint8Array([1,2,3,...]).buffer` for payloads

**Fix**: Use V8's native ArrayBuffer/Uint8Array directly:
```rust
let backing_store = v8::ArrayBuffer::new_backing_store_from_vec(payload.to_vec()).make_shared();
let array_buffer = v8::ArrayBuffer::with_backing_store(scope, &backing_store);
let uint8_array = v8::Uint8Array::new(scope, array_buffer, 0, payload.len());
```

#### 3. Event Loop / Microtask Checkpoint
**Fix**: Call `runtime.v8_isolate().perform_microtask_checkpoint()` after each wire message execution to process pending microtasks.

#### 4. Result Extraction Without Array.from()
**Previous**: JavaScript used `Array.from(result)` to convert Uint8Array to an array for serialization

**Fix**: Return Uint8Array directly and extract bytes using V8's `copy_contents()`:
```rust
if result.is_uint8_array() {
    let arr = v8::Local::<v8::Uint8Array>::try_from(result)?;
    arr.copy_contents(&mut bytes);
}
```

### Current Benchmark Results (Post-Fix)

#### Before vs After Comparison

| Metric | **BEFORE (Broken)** | **AFTER (Fixed)** | **Improvement** |
|--------|---------------------|-------------------|-----------------|
| **Low Intensity (10/s)** |
| Total ops (30s) | 94 | 600 (60s) | ✅ Works |
| Error rate | 0% | 0% | Same |
| P50 Latency | 2.24ms | **1.31ms** | **42% faster** |
| P95 Latency | 7.61ms | **3.87ms** | **49% faster** |
| **Medium Intensity (100/s)** |
| Total ops (30s) | 33 (STALLED) | **30,025** (60s) | **909x more** |
| Error rate | 24% | **0%** | **Fixed** |
| P50 Latency | 1815ms (timeout) | **0.73ms** | **2486x faster** |
| Throughput | ~1 ops/s | **500 ops/s** | **500x faster** |
| **High Intensity (500/s)** |
| Total ops (60s) | ❌ Not working | **60,206** | **Now works** |
| Error rate | N/A | **0%** | **Now works** |
| P50 Latency | N/A | **1.44ms** | **Now works** |
| Throughput | 0 | **1,003 ops/s** | **Now works** |

#### Comprehensive Benchmark Suite

| Test | Duration | Ops/sec | Total Ops | Errors | P50 | P95 | P99 | Memory |
|------|----------|---------|-----------|--------|-----|-----|-----|--------|
| Low (10/s) | 60s | 10 | 600 | 0% | 1.31ms | 3.87ms | 5.76ms | 558MB |
| Medium (100/s) | 60s | **500** | 30,025 | 0% | 0.73ms | 1.87ms | 2.27ms | 817MB |
| High (500/s) | 60s | **1,003** | 60,206 | 0% | 1.44ms | 2.35ms | 3.10ms | 1.0GB |
| Complex Schema | 30s | 500 | 15,010 | 0% | 0.94ms | 3.69ms | 7.96ms | 817MB |
| Large (10k rows) | 30s | 919 | 27,584 | 0% | 1.12ms | 2.43ms | 2.68ms | 869MB |
| Extended (2min) | 120s | 500 | **60,055** | **0%** | 0.72ms | 1.90ms | 2.28ms | 1.0GB |

#### Key Achievements
- **Throughput**: Sustained **1,000+ ops/sec** (target was 500)
- **Reliability**: **0% error rate** across all standard tests
- **Latency**: Sub-millisecond P50 (**0.72-1.44ms**)
- **Stability**: 60,000+ ops over 2 minutes with zero errors

### Comparison with Native PGlite

Native PGlite (direct JavaScript API) is approximately **10x faster** for individual operations:

| Approach | Single Op Latency | Use Case |
|----------|-------------------|----------|
| **Native PGlite** (JS) | 0.06-0.15ms | Browser/Node.js apps |
| **pglited** (TCP) | 0.7-1.5ms | Any language via PostgreSQL protocol |

#### Why the Overhead?

pglited adds several layers that native PGlite doesn't have:
1. **TCP socket communication** - Network stack overhead
2. **PostgreSQL wire protocol** - Encode/decode messages
3. **Postgrex client** - Elixir driver serialization
4. **Rust ↔ V8 bridge** - Message passing between threads
5. **Process boundary** - Elixir Port to pglited binary

#### Trade-off Analysis

| | Native PGlite | pglited |
|--|---------------|---------|
| **Speed** | ⚡ 0.06ms | 🔵 0.7ms |
| **Language support** | JavaScript only | Any (Elixir, Python, Go, etc.) |
| **Protocol** | Proprietary JS API | Standard PostgreSQL wire protocol |
| **Client libraries** | Custom SDK | Postgrex, psycopg2, pg gem, etc. |
| **Use case** | Browser/embedded JS | Embedded DB for any tech stack |

**Conclusion**: The ~10x overhead is the expected cost of providing a real PostgreSQL-compatible interface. For the ex_pglite use case (Elixir applications), 0.7ms latency with 1000+ ops/sec throughput is excellent for an embedded database.

## Potential Future Improvements

1. **Add deno_web extension**: Would provide native TextEncoder, URL, etc. (would replace custom polyfills)
2. **Add deno_console extension**: Proper console implementation
3. **Pre-compiled WASM module**: Use `wasmModule` option for even faster startup
4. **Reduce polyfills**: Use more deno extensions for native implementations
5. **Further performance optimization**: Profile and optimize hot paths in wire protocol handling
