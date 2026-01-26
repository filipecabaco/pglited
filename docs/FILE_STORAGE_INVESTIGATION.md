# File Storage Persistence Investigation

## Goal

Add an integration test proving that when using file storage (`file://` paths), data persists across reconnects. Enable users to attach compatible data directories for persistent storage.

## Current Status: Work In Progress

The file storage feature is not yet working. Emscripten's NODEFS cannot access our Node.js `fs` module shim.

**Error**: `Cannot read properties of undefined (reading 'lstatSync')`

## What Has Been Implemented

### 1. Integration Tests (`tests/integration_test.rs`)

- `test_file_storage_data_persists_on_reconnect` - Tests data persistence across restarts
- `test_persistent_storage_mode` - Tests file:// storage initialization
- `test_postgres_client_connectivity` - Tests tokio-postgres connectivity

### 2. Path Normalization (`src/js_runtime.rs`)

Modified `normalize_data_dir` to convert absolute paths to `file://` URLs:

```rust
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
```

### 3. Node.js `fs` Module Shim

Implemented Rust ops for filesystem operations:

| Function | Rust Op | Description |
|----------|---------|-------------|
| `existsSync` | `op_fs_exists_sync` | Check if path exists |
| `mkdirSync` | `op_fs_mkdir_sync` | Create directory |
| `readFileSync` | `op_fs_read_file_sync` | Read file contents |
| `writeFileSync` | `op_fs_write_file_sync` | Write file contents |
| `unlinkSync` | `op_fs_unlink_sync` | Delete file |
| `rmdirSync` | `op_fs_rmdir_sync` | Remove directory |
| `statSync` | `op_fs_stat_sync` | Get file stats |
| `lstatSync` | `op_fs_lstat_sync` | Get file stats (no follow symlinks) |
| `readdirSync` | `op_fs_readdir_sync` | List directory contents |
| `renameSync` | `op_fs_rename_sync` | Rename/move file |
| `truncateSync` | `op_fs_truncate_sync` | Truncate file |

### 4. Node.js `path` Module Shim

Implemented path utilities: `join`, `resolve`, `normalize`, `dirname`, `basename`, `extname`, `isAbsolute`, `relative`, `parse`, `format`

### 5. Bundle Patching Attempts

Tried multiple approaches to inject fs/path into the Emscripten bundle:

1. **ES Module Loader** - Return shims for `import fs from 'fs'`
2. **Global require()** - Define `globalThis.require` function
3. **Text Replacement** - Replace `require("fs")` with `globalThis.fs`
4. **Regex Patching** - Match minified patterns like `n("fs")`

## Root Cause Analysis

### Error Trace

```
at pglite:///index.js:8:14651          ← Column 14651 = heavily minified
at Object.tryFSOperation
at Object.getMode
at Object.mount
at emscriptenOpts.preRun (pglite:///fs/nodefs.js)
```

### The Problem

PGlite's bundled `index.js` contains Emscripten's NODEFS implementation. The bundle was created with webpack/rollup which transforms `require("fs")` in complex ways:

```javascript
// Original (before bundling)
var fs = require("fs");

// After bundling (minified) - could be any of these:
var e = n("fs");           // renamed require
var e = (0, r.require)("fs");  // indirect call
typeof require !== "undefined" && require("fs")  // conditional
```

Our regex patterns catch standard `require("fs")` but not all minified variations.

### How Emscripten's NODEFS Works

```javascript
// Emscripten captures fs in module scope at bundle time
var fs = require("fs");
var path = require("path");

// NODEFS uses the captured reference
var NODEFS = {
  getMode: function(path) {
    var stat = fs.lstatSync(path);  // Uses captured fs - FAILS if fs is undefined
    // ...
  },
  mount: function(mount) {
    return NODEFS.createNode(null, '/', NODEFS.getMode(mount.opts.root), 0);
  }
};
```

## Potential Solutions

### Option 1: Deeper Bundle Patching

Instead of replacing `require("fs")`, look for the actual variable assignment pattern and replace the whole thing:

```javascript
// Find: var e=n("fs")
// Replace with: var e=globalThis.fs
```

This requires understanding the specific bundle format PGlite uses.

### Option 2: Custom PGlite Build

Fork PGlite and modify how it handles file:// storage:
- Expose fs as a configuration option
- Use dynamic import instead of bundled require

### Option 3: Alternative Storage Backend

Instead of using NODEFS (which requires Node.js fs), implement a custom Emscripten filesystem:

```javascript
// Custom FS backend that uses our Rust ops
var RUSTFS = {
  mount: function(mount) { /* ... */ },
  // Implement all required FS operations
};
```

### Option 4: IndexedDB-like Approach

Implement a storage layer similar to how PGlite handles IndexedDB in browsers, but backed by our Rust filesystem ops.

### Option 5: Deno Compatibility

Use Deno's fs module (if available in deno_core) instead of shimming Node.js fs.

## Files Modified

| File | Changes |
|------|---------|
| `src/js_runtime.rs` | fs/path ops, module loader, bootstrap code, bundle patching |
| `tests/integration_test.rs` | New persistence tests |
| `Cargo.toml` | Added `regex`, `tokio-postgres` dependencies |

## Dependencies Added

```toml
regex = "1"           # For bundle patching patterns
tokio-postgres = "0.7"  # For integration tests (dev-dependency)
```

## References

- [PGlite Storage Documentation](https://pglite.dev/docs/filesystems)
- [Emscripten NODEFS](https://emscripten.org/docs/api_reference/Filesystem-API.html#nodefs)
- [Emscripten Custom Filesystems](https://emscripten.org/docs/porting/files/file_systems_overview.html)
