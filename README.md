# pglited

![pglited demo](pglited-demo.gif)

A self-contained PostgreSQL runtime using V8 and PGlite WebAssembly. Supports both **memory mode** (in-memory database) and **file mode** (persistent storage).

## Features

- **Single Binary Distribution**: All assets embedded in the binary - no external files required
- **V8 JavaScript Runtime**: Uses Deno Core for high-performance JavaScript/WebAssembly execution
- **Two Storage Modes**: Memory mode for ephemeral databases, file mode for persistent storage
- **Protocol Compatible**: Full PostgreSQL wire protocol support
- **Auto-downloading Assets**: PGlite npm package downloaded and embedded at build time

## Quick Start

Build the release binary with embedded assets:

```bash
make build-release
```

Run with embedded assets (no paths needed):

```bash
# In-memory database on port 5432
./target/release/pglited memory:// 5432

# Persistent storage
./target/release/pglited /tmp/mydb 5432
```

## Connecting

```bash
# Using psql
PGPASSWORD=password psql "host=127.0.0.1 port=5432 user=postgres dbname=template1 sslmode=disable"

# Connection string for any PostgreSQL client
postgresql://postgres:password@127.0.0.1:5432/template1
```

## Examples

### Memory Mode (In-Memory Database)

```bash
# Start pglited in memory mode
./target/release/pglited memory:// 5432 --daemon

# Connect and interact
PGPASSWORD=password psql "host=127.0.0.1 port=5432 user=postgres dbname=template1 sslmode=disable"

template1=# create table users(id serial, name text);
CREATE TABLE

template1=# insert into users(name) values('Alice'), ('Bob');
INSERT 0 2

template1=# select * from users;
 id | name
----+-------
  1 | Alice
  2 | Bob
(2 rows)
```

### File Mode (Persistent Storage)

```bash
# Start pglited with file persistence
./target/release/pglited /tmp/mydb 5432 --daemon

# Data persists across restarts
```

## Command Line Options

```bash
Usage: pglited <data_dir> <tcp_port> [--multiplexer <mode>] [--daemon]
       pglited --dump-datadir <output_path>

Commands:
  --dump-datadir <path>    Dump initialized PostgreSQL data directory to a tar file

Arguments:
  data_dir         Directory for PostgreSQL data (use memory:// for in-memory)
  tcp_port         TCP port for PostgreSQL connections

Options:
  --multiplexer <mode>     Enable connection multiplexer (mode: queue)
  --daemon                 Start in background threaded (blocking) mode

Examples:
  # In-memory database
  ./target/release/pglited memory:// 5432

  # Persistent database
  ./target/release/pglited /tmp/mydb 5432

  # In-memory database (daemon mode)
  ./target/release/pglited memory:// 5432 --daemon

  # Persistent database (daemon mode)
  ./target/release/pglited /tmp/mydb 5432 --daemon
```

## Build Targets

```bash
make help              # Show all available targets
make build-release     # Build optimized release binary
make test              # Run all tests
make clean             # Clean build artifacts
```

## Configuration

### PGlite Version

The PGlite version is configured in `build.rs`:

```rust
const PGLITE_VERSION: &str = "0.3.15";
const PGLITE_NPM_TARBALL: &str =
    "https://registry.npmjs.org/@electric-sql/pglite/-/pglite-0.3.15.tgz";
```

To update PGlite:
1. Edit the version in `build.rs`
2. Delete `assets/pglite_npm/` to force re-download
3. Rebuild: `cargo build --release`

### PostgreSQL Server Version

The reported PostgreSQL server version is configured in `src/lib.rs`:

```rust
const PGLITE_SERVER_VERSION: &str = "17.5";
```

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                            pglited                              │
│                                                                 │
│  ┌──────────────┐    ┌──────────────┐    ┌──────────────────┐  │
│  │  TCP Server  │───▶│ Wire Proto   │───▶│   V8 Runtime     │  │
│  │ (127.0.0.1)  │◀───│   Handler    │◀───│  (PGlite WASM)   │  │
│  └──────────────┘    └──────────────┘    └──────────────────┘  │
│         ▲                                         │             │
│         │                                         ▼             │
│   PostgreSQL                               ┌──────────────┐    │
│    Clients                                 │   PGDATA     │    │
│                                            │ (memory/disk)│    │
│                                            └──────────────┘    │
└─────────────────────────────────────────────────────────────────┘
```

## How It Works

### V8 JavaScript Runtime

The binary uses [Deno Core](https://github.com/denoland/deno_core) to execute JavaScript and WebAssembly:

- **Embedded Assets**: PGlite npm package files embedded via rust-embed
- **Custom Module Loader**: Resolves `pglite:///` URLs to embedded assets
- **Polyfills**: TextEncoder/Decoder, fetch, Blob, URL, crypto, timers
- **Zero-copy Buffers**: Direct V8 ArrayBuffer handling for wire protocol messages
- **Dedicated Thread**: JavaScript runtime runs in isolated thread with message passing

### Memory vs Persistent Mode

**Memory mode** (`memory://`):
- Creates an isolated in-memory database
- Data lost when process exits
- Perfect for testing and ephemeral workloads

**Persistent mode** (any filesystem path):
- Uses the specified directory for PGDATA
- Data survives process restarts

### Wire Protocol

The server implements PostgreSQL wire protocol handling:
- Parses incoming wire messages (Query, Parse, Bind, Execute, etc.)
- Injects `server_version` parameter on first response
- Handles ReadyForQuery state tracking

## Environment Variables

- `PGLITE_DEBUG=1` - Enable verbose debug output

## Testing

```bash
cargo test
```

Tests cover:
- **Wire Protocol Parsing**: Message iteration, truncated data, invalid lengths
- **Server Version Injection**: Detection, creation, and injection logic
- **TCP Socket Binding**: Port allocation and error handling
- **Integration Tests**: Binary startup, ready signal, multiple instances, persistent storage

### Test Summary

- 20 unit tests in `src/lib.rs`
- 4 integration tests in `tests/integration_test.rs`

## Asset Structure

Assets are automatically downloaded and embedded at build time:

```
assets/
└── pglite_npm/
    └── dist/
        ├── index.js           # PGlite entry point
        ├── postgres.wasm      # PostgreSQL WebAssembly module
        ├── postgres.data      # PostgreSQL data files
        └── pgdata_seed.tar    # Pre-initialized database (generated)
```

The build process:
1. Downloads `@electric-sql/pglite` npm tarball
2. Extracts `dist/` contents to `assets/pglite_npm/dist/`
3. Generates `pgdata_seed.tar` using the built binary (second build)
4. Embeds all assets into the final binary via rust-embed

## Dependencies

Key runtime dependencies:
- `deno_core` - V8 JavaScript runtime
- `tokio` - Async runtime for TCP handling
- `rust-embed` - Compile-time asset embedding
- `anyhow` - Error handling

## Performance

Key findings:
- **Use pool_size: 1** - Higher pool sizes cause performance degradation due to single-threaded PGlite
- Typical performance: ~500 QPS reads, ~275 QPS writes, ~100 QPS transactions

## License

MIT
