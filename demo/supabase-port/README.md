# Supabase CLI Port Demo

Zero-ETL migration from pglited to Supabase CLI using pg_dump/psql.

## Run

```bash
cd demo/supabase-port && ./run.sh
```

Automatically cleans up on startup and Ctrl+C.

## How It Works

1. **Pglited** starts in file mode → creates persistent PostgreSQL data
2. **Seed** → populates database with sample e-commerce data
3. **pg_dump** → exports database from pglited to SQL dump file
4. **Supabase CLI** → starts database only (gotrue + postgres)
5. **psql** → restores dump to Supabase database
6. **Verify** → side-by-side queries confirm identical data (built-in)

Both run PostgreSQL 17.x over standard wire protocol.

## Prerequisites

```bash
make build-release
brew install supabase/tap/supabase
brew install deno
```

## Connect

```bash
# Pglited
export PGPASSWORD=password
export PGSSLMODE=disable
psql -h 127.0.0.1 -p 54321 -U postgres -d template1

# Supabase
export PGPASSWORD=postgres
export PGSSLMODE=disable
psql -h 127.0.0.1 -p 54322 -U postgres -d postgres
```
