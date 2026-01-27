# PostgREST Demo

Instant REST API from pglited PostgreSQL database.

## Run

```bash
cd demo/postgrest && ./run.sh
```

Automatically cleans up existing processes on startup and exit (Ctrl+C).

## How It Works

1. **Pglited** starts in file mode → persistent PostgreSQL database
2. **Seed** → creates products table with sample data
3. **PostgREST** → connects to pglited, introspects schema
4. **REST API** → auto-generated endpoints for all tables/relations
5. **Query** → HTTP requests with filters, sorts, limits via query params

PostgREST reads PostgreSQL schema and generates REST API automatically.

## Prerequisites

```bash
make build-release
brew install postgrest
brew install deno
```

## API Endpoints

```bash
curl http://localhost:3000/products
curl http://localhost:3000/products?category=eq.coffee
curl http://localhost:3000/products?price=lt.4.00
```
