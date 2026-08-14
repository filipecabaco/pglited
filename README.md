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
Usage: pglited <data_dir> <tcp_port> [--multiplexer <mode>] [--daemon] [--extensions <list>] [--init-sql <sql>]
       pglited --dump-datadir <output_path> [--extensions <list>]

Commands:
  --dump-datadir <path>    Dump initialized PostgreSQL data directory to a tar file

Arguments:
  data_dir         Directory for PostgreSQL data (use memory:// for in-memory)
  tcp_port         TCP port for PostgreSQL connections

Options:
  --multiplexer <mode>     Enable connection multiplexer (mode: queue)
  --daemon                 Start in background threaded (blocking) mode
  --extensions <list>      Comma-separated PGlite extensions
  --init-sql <sql>         SQL to run after PostgreSQL starts (before accepting connections)

Examples:
  # In-memory database
  ./target/release/pglited memory:// 5432

  # Persistent database
  ./target/release/pglited /tmp/mydb 5432

  # In-memory database (daemon mode)
  ./target/release/pglited memory:// 5432 --daemon

  # Persistent database (daemon mode)
  ./target/release/pglited /tmp/mydb 5432 --daemon

  # Load extensions
  ./target/release/pglited memory:// 5432 --extensions pg_trgm,vector

  # Run initialization SQL (e.g., set search_path for schema compatibility)
  ./target/release/pglited memory:// 5432 --init-sql "SET search_path TO myschema, public"

  # Create a schema on startup
  ./target/release/pglited memory:// 5432 --init-sql "CREATE SCHEMA IF NOT EXISTS api"

  # Multiple statements (semicolon-separated)
  ./target/release/pglited memory:// 5432 --init-sql "CREATE SCHEMA api; SET search_path TO api, public"

  # Regenerate the embedded pgdata seed (see "pgdata Seed" below)
  ./target/release/pglited --dump-datadir pgdata_seed.tar
```

## Build Targets

```bash
make help              # Show all available targets
make build-release     # Build optimized release binary
make test              # Run all tests
make clean             # Clean build artifacts
```

Or with [mise](https://mise.jdx.dev):

```bash
mise run build           # Debug build
mise run release         # Optimized release build
mise run test            # Run all tests
mise run lint            # cargo fmt --check + clippy
mise run seed            # Regenerate the pgdata seed
mise run upgrade-pglite  # Re-download PGlite assets, reseed, rebuild
```

## Configuration

### PGlite Version

pglited tracks PGlite `0.5.4`. The version and the separately published
extension packages are configured in `build.rs`:

```rust
const PGLITE_VERSION: &str = "0.5.4";

const EXTENSION_PACKAGES: &[(&str, &str, &str)] = &[
    ("vector", "pglite-pgvector", "0.0.6"),
    ("pgtap", "pglite-pgtap", "0.0.6"),
    // ...
];
```

Since PGlite 0.4, `vector`, `pgtap`, `pg_ivm`, `pg_hashids` and `pg_uuidv7` are
no longer part of the main tarball. `build.rs` fetches them from their own npm
packages and lays them out under `dist/<name>/` so they resolve exactly as
before. Add another extension by appending to `EXTENSION_PACKAGES`.

To update PGlite:

```bash
# edit PGLITE_VERSION in build.rs, then
mise run upgrade-pglite
```

That re-downloads the assets, regenerates the pgdata seed and rebuilds.

### pgdata Seed

`assets/pglite_npm/dist/pgdata_seed.tar` is a pre-initialised PostgreSQL data
directory embedded in the binary. File-backed databases unpack it instead of
running `initdb`, which is the difference between a sub-second and a
multi-second first start. If it is missing, pglited falls back to `initdb`.

Regenerate it after changing the PGlite version:

```bash
mise run seed
cargo build
```

The seed is deliberately extension-free. `CREATE EXTENSION` writes catalog rows
and `shared_preload_libraries` entries pointing at shared objects that only
exist when that extension is passed at runtime, so a seed with extensions baked
in refuses to start (`could not access file "pg_stat_statements"`). Load
extensions per-run with `--extensions` instead.

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

## Extensions

PGlite supports several extensions that can be loaded via `--extensions` or `PGLITED_EXTENSIONS`:

| `--extensions` name | `CREATE EXTENSION` name | Description |
|---------------------|-------------------------|-------------|
| `pg_trgm` | `pg_trgm` | Trigram matching for similarity search |
| `vector` | `vector` | Vector similarity search (pgvector) |
| `uuid_ossp` | `"uuid-ossp"` | UUID generation functions |
| `pgcrypto` | `pgcrypto` | Cryptographic functions |
| `live` | - | Live queries (PGlite-specific) |
| `pg_hashids` | `pg_hashids` | Generate short unique IDs |
| `pg_ivm` | `pg_ivm` | Incremental view maintenance |
| `pg_uuidv7` | `pg_uuidv7` | UUIDv7 generation |
| `pgtap` | `pgtap` | Unit testing framework |

The full contrib set shipped by PGlite is available; run
`ls assets/pglite_npm/dist/contrib` after a build to see it. The two columns
differ where the bundle filename and the SQL extension name disagree, as with
`uuid_ossp` / `uuid-ossp`.

Loading a bundle at startup makes it available; you still create it per
database:

```bash
./target/release/pglited memory:// 5432 --extensions pg_trgm,vector
```

```sql
CREATE EXTENSION IF NOT EXISTS pg_trgm;
```

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `PGLITE_DEBUG` | `0` | `1` enables verbose debug output |
| `PGLITE_DEBUG_LEVEL` | `0` | PGlite debug level 0-5; at 5 the PostgreSQL server log is echoed to stderr |
| `PGLITED_EXTENSIONS` | - | Default extensions to load, e.g. `pg_trgm,vector` |
| `PGLITED_INIT_SQL` | - | SQL to run after PostgreSQL starts |
| `PGLITED_MAX_CONNECTIONS` | `100` | Concurrent client connections; excess clients get `FATAL 53300` |
| `PGLITED_MAX_MESSAGE_BYTES` | `1 GiB` | Largest single protocol message accepted, matching PostgreSQL |
| `PGLITED_QUERY_TIMEOUT_SECS` | `300` | Per-query wait in threaded (`--daemon`) mode; `0` waits indefinitely |
| `PGLITED_INIT_TIMEOUT_SECS` | `120` | Startup budget |
| `PGLITED_INIT_SQL_TIMEOUT_SECS` | `60` | `--init-sql` budget |
| `PGLITED_DUMP_TIMEOUT_SECS` | `300` | `--dump-datadir` budget |
| `PGLITED_FS_SANDBOX` | `1` | `0` disables the data-directory filesystem sandbox |
| `PGLITED_V8_HEAP_MB` | `256` | Initial V8 heap |
| `PGLITED_V8_MAX_HEAP_MB` | `1024` | Maximum V8 heap |

## Security Notes

pglited listens on `127.0.0.1` only and inherits PGlite's authentication
behaviour, which accepts any password for the `postgres` user. Treat it as a
local development database, not an internet-facing server.

Within that scope:

- **Filesystem sandbox** - in file mode the `op_fs_*` bridge between JS/WASM and
  the host filesystem is confined to the data directory. Paths that resolve
  outside it are refused with `EPERM`, in both the Rust ops and the VFS shim.
  This is a lexical check: it stops path traversal, not a symlink planted by
  someone who can already write into the data directory.
- **Protocol framing** - the wire layer reassembles whole frontend messages
  before handing them to PGlite and rejects malformed or oversized length
  headers, so a peer cannot make the server buffer without bound.
- **Connection limit** - `PGLITED_MAX_CONNECTIONS` bounds threads, sockets and
  per-connection buffers.
- **No shell-style interpolation** - `--init-sql`, `--extensions` and the data
  directory are passed into the JS runtime as values rather than spliced into
  script source. Extension names are restricted to `[A-Za-z0-9_-]`.
- **Tar extraction** - the embedded pgdata seed is extracted with entry names
  checked against the destination directory.

## Web Workers

PGlite's `@electric-sql/pglite/worker` module is intentionally not used.

It exists to let several browser tabs share one PGlite instance: `PGliteWorker`
wraps a `Worker`, and uses `BroadcastChannel` plus `navigator.locks` to elect a
leader tab that owns the database. pglited already occupies that role - one
PGlite instance serving many clients, here over TCP rather than `postMessage`.
Routing through `PGliteWorker` would add a message-passing hop and give up
`execProtocolRawSync`, the synchronous entry point the query path is built on,
in exchange for an async-only API.

Queries are serialized onto a single JS thread. That is a property of PGlite
itself: one WASM PostgreSQL instance, one data directory, one writer.

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
- 10 integration tests in `tests/integration_test.rs`

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
