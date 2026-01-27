#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
PGLITED="$REPO_ROOT/target/release/pglited"
DATA_DIR="/tmp/pglited_postgrest_demo"
PG_PORT=54321
POSTGREST_PORT=3000

shutdown_existing() {
  echo "Shutting down any existing processes..."
  
  if nc -z 127.0.0.1 "$PG_PORT" 2>/dev/null; then
    echo "Killing process on port $PG_PORT..."
    lsof -ti:$PG_PORT 2>/dev/null | xargs kill -9 2>/dev/null || true
    sleep 1
  fi
  
  if nc -z 127.0.0.1 "$POSTGREST_PORT" 2>/dev/null; then
    echo "Killing process on port $POSTGREST_PORT..."
    lsof -ti:$POSTGREST_PORT 2>/dev/null | xargs kill -9 2>/dev/null || true
    sleep 1
  fi
  
  pkill -f "postgrest" 2>/dev/null || true
  pkill -f "pglited" 2>/dev/null || true
  sleep 1
  
  if [ -d "$DATA_DIR" ]; then
    echo "Removing existing data directory $DATA_DIR..."
    rm -rf "$DATA_DIR" 2>/dev/null || true
  fi
  
  echo "Shutdown complete"
  echo ""
}

shutdown_existing

cleanup() {
  echo "Cleaning up..."
  [ -n "${POSTGREST_PID:-}" ] && kill "$POSTGREST_PID" 2>/dev/null || true
  [ -n "${PGLITED_PID:-}" ]   && kill "$PGLITED_PID"   2>/dev/null || true
  wait 2>/dev/null || true
  echo "Done."
}
trap cleanup EXIT

# --- Pre-flight checks ---
if [ ! -x "$PGLITED" ]; then
  echo "pglited binary not found at $PGLITED"
  echo "Run 'make build-release' first."
  exit 1
fi

if ! command -v postgrest &>/dev/null; then
  echo "PostgREST not found. Installing via Homebrew..."
  eval "$(/opt/homebrew/bin/brew shellenv 2>/dev/null || /usr/local/bin/brew shellenv 2>/dev/null)"
  brew install postgrest
fi

if ! command -v deno &>/dev/null; then
  echo "Deno not found. Install it from https://deno.land"
  exit 1
fi

# --- Start pglited in daemon (file) mode ---
echo "Starting pglited (daemon + file mode → $DATA_DIR, port $PG_PORT)..."
"$PGLITED" "$DATA_DIR" "$PG_PORT" --daemon &
PGLITED_PID=$!

echo "Waiting for pglited to be ready..."
for i in $(seq 1 30); do
  if nc -z 127.0.0.1 "$PG_PORT" 2>/dev/null; then
    echo "pglited is ready (took ${i}s)"
    break
  fi
  sleep 1
done

if ! nc -z 127.0.0.1 "$PG_PORT" 2>/dev/null; then
  echo "pglited failed to start"
  exit 1
fi

# --- Seed data ---
echo ""
deno run --allow-net --allow-env "$SCRIPT_DIR/seed.ts" "$PG_PORT"

# --- Start PostgREST ---
echo ""
echo "Starting PostgREST on port $POSTGREST_PORT..."
postgrest "$SCRIPT_DIR/postgrest.conf" &
POSTGREST_PID=$!

for i in $(seq 1 15); do
  if nc -z 127.0.0.1 "$POSTGREST_PORT" 2>/dev/null; then
    echo "PostgREST is ready (took ${i}s)"
    break
  fi
  sleep 1
done

if ! nc -z 127.0.0.1 "$POSTGREST_PORT" 2>/dev/null; then
  echo "PostgREST failed to start"
  exit 1
fi

# --- Fetch data via PostgREST ---
echo ""
deno run --allow-net "$SCRIPT_DIR/fetch.ts" "http://localhost:$POSTGREST_PORT"

echo ""
echo "Demo complete! Processes still running — press Ctrl-C to stop."
wait
