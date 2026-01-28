# Micro-Supabase Demo

A single script that boots **pglited**, **Supabase Auth**, and **PostgREST** together — giving you a minimal Supabase-like stack with zero Docker, zero external databases, and a single in-memory Postgres instance.

## Quick Start

```bash
# Build pglited (from the repo root)
make build-release

# Run the demo
./demo/micro-supabase/run.sh
```

The script will:

1. Download Auth and PostgREST binaries (cached in `demo/micro-supabase/bin/`).
2. Start pglited in-memory on port **54321** with required extensions and search_path configuration.
3. Seed the database with schemas, types, `api.products` table, and PostgREST roles.
4. Start Supabase Auth on port **9999** (runs 65 migrations automatically).
5. Start PostgREST on port **3000**.
6. Verify both services and print sample curl commands.

## Prerequisites

| Tool | Purpose | Install |
|------|---------|---------|
| `deno` | Runs the seed script | [deno.land](https://deno.land) |
| `curl` | Downloads binaries & verifies endpoints | Usually pre-installed |
| `tar` / `xz` | Extracts release archives | `brew install xz` (macOS) / `apt install xz-utils` (Linux) |
| `go` | Builds Auth on macOS | `brew install go` (macOS only) |
| `git` | Clones Auth repo on macOS | Usually pre-installed |

## Architecture

```
                ┌──────────────┐
                │   Browser /  │
                │    curl      │
                └──┬───────┬───┘
                   │       │
          :9999    │       │   :3000
        ┌──────────▼──┐ ┌──▼──────────┐
        │ Supabase    │ │  PostgREST  │
        │ Auth        │ │             │
        └──────┬──────┘ └──────┬──────┘
               │               │
               │  :54321       │
            ┌──▼───────────────▼──┐
            │     pglited         │
            │  (in-memory PG)     │
            └─────────────────────┘
```

## Ports

| Service | Port |
|---------|------|
| pglited (Postgres wire protocol) | 54321 |
| Supabase Auth | 9999 |
| PostgREST | 3000 |

## Sample Requests

### Sign up a user

```bash
curl -s -X POST http://localhost:9999/signup \
  -H 'Content-Type: application/json' \
  -d '{"email":"user@example.com","password":"testpassword123"}'
```

### Get a JWT token

```bash
curl -s -X POST http://localhost:9999/token?grant_type=password \
  -H 'Content-Type: application/json' \
  -d '{"email":"user@example.com","password":"testpassword123"}'
```

### List products (anonymous — read-only via `web_anon` role)

```bash
curl -s http://localhost:3000/products
```

### List products (authenticated — replace `<TOKEN>` with the JWT)

```bash
curl -s http://localhost:3000/products \
  -H 'Authorization: Bearer <TOKEN>'
```

### Auth health check

```bash
curl -s http://localhost:9999/health
```

## Configuration Files

| File | Description |
|------|-------------|
| `auth.env` | Environment variables for Supabase Auth (JWT secret, DB URL, email settings) |
| `postgrest.conf` | PostgREST configuration (DB URI, JWT secret, schemas, roles) |
| `seed.ts` | Database setup: extensions, `api.products` table, PostgREST roles & grants |

The JWT secret is shared between Auth and PostgREST so that tokens issued by Auth are accepted by PostgREST for role switching (`web_anon` -> `authenticated`).

## pglited Configuration

pglited is started with these flags:

```bash
pglited memory:// 54321 --daemon \
  --extensions uuid_ossp,pgcrypto \
  --init-sql "SET search_path TO auth, public, api"
```

### Extensions

- **uuid_ossp** - Required by Supabase Auth for UUID generation (`uuid_generate_v4()`)
- **pgcrypto** - Required by Supabase Auth for password hashing (`crypt()`, `gen_salt()`)

### Init SQL

The `--init-sql` flag sets `search_path` to include the `auth` schema. This is required because Supabase Auth queries tables with unqualified names (e.g., `SELECT * FROM users` instead of `SELECT * FROM auth.users`). Without this, Auth's runtime queries would fail with "relation does not exist" errors.

### Seed Script

The `seed.ts` script pre-creates:
- The `auth` schema and required enum types (for Auth migrations)
- The `api` schema with a `products` table
- PostgREST roles (`web_anon`, `authenticated`, `authenticator`) with appropriate grants

## Platform Support

| Platform | Auth | PostgREST |
|----------|------|-----------|
| Linux x86_64 | Binary download | Binary download |
| Linux ARM64 | Binary download | Binary download |
| macOS ARM64 | Built from source (requires Go) | Binary download |
| macOS x86_64 | Built from source (requires Go) | ARM64 binary via Rosetta 2 |

On macOS, Auth is built from source because pre-built binaries are Linux-only. The script automatically clones the Auth repository and builds it using Go.

## Cleanup

Press **Ctrl-C** to stop all services. The EXIT trap kills Auth, PostgREST, and pglited automatically. Since pglited runs in-memory, no data persists after shutdown.

To remove downloaded binaries:

```bash
rm -rf demo/micro-supabase/bin/
```
