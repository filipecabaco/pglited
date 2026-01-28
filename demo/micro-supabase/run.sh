#!/usr/bin/env bash
set -euo pipefail

# ── Paths & Ports ────────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
PGLITED="$REPO_ROOT/target/release/pglited"
BIN_DIR="$SCRIPT_DIR/bin"

PG_PORT=54321
AUTH_PORT=9999
POSTGREST_PORT=3000

AUTH_VERSION="2.185.0"
POSTGREST_VERSION="14.3"

# ── Cleanup ──────────────────────────────────────────────────────
cleanup() {
  echo ""
  echo "Shutting down..."
  [ -n "${AUTH_PID:-}" ]      && kill "$AUTH_PID"      2>/dev/null || true
  [ -n "${POSTGREST_PID:-}" ] && kill "$POSTGREST_PID" 2>/dev/null || true
  pkill -f "pglited.*${PG_PORT}" 2>/dev/null || true
  wait 2>/dev/null || true
  echo "All processes stopped."
}
trap cleanup EXIT

shutdown_existing() {
  for port in "$PG_PORT" "$AUTH_PORT" "$POSTGREST_PORT"; do
    if nc -z 127.0.0.1 "$port" 2>/dev/null; then
      echo "Killing existing process on port $port..."
      lsof -ti:"$port" 2>/dev/null | xargs kill -9 2>/dev/null || true
    fi
  done
  pkill -f "pglited" 2>/dev/null || true
  sleep 1
}

shutdown_existing

# ── Preflight ────────────────────────────────────────────────────
echo "=== Micro-Supabase Demo ==="
echo "    pglited + Supabase Auth + PostgREST"
echo ""

# ── Detect OS / Arch ────────────────────────────────────────────
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Darwin) OS_LABEL="macos" ;;
  Linux)  OS_LABEL="linux" ;;
  *)      echo "Unsupported OS: $OS"; exit 1 ;;
esac

case "$ARCH" in
  x86_64)        ARCH_LABEL="x86_64" ;;
  aarch64|arm64) ARCH_LABEL="arm64"  ;;
  *)             echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

echo "Platform: ${OS_LABEL}/${ARCH_LABEL}"
echo ""

if [ ! -x "$PGLITED" ]; then
  echo "pglited binary not found at $PGLITED"
  echo "Run 'make build-release' in $REPO_ROOT first."
  exit 1
fi

REQUIRED_CMDS="curl tar deno"
if [ "$OS" = "Darwin" ]; then
  REQUIRED_CMDS="$REQUIRED_CMDS git go"
fi

for cmd in $REQUIRED_CMDS; do
  if ! command -v "$cmd" &>/dev/null; then
    echo "Required command not found: $cmd"
    exit 1
  fi
done

# ── Download helpers ─────────────────────────────────────────────
mkdir -p "$BIN_DIR"

download_auth() {
  if [ -x "$BIN_DIR/auth" ]; then
    echo "Auth binary cached at $BIN_DIR/auth — skipping download."
    return
  fi

  # macOS: build from source (pre-built binaries are Linux-only)
  if [ "$OS_LABEL" = "macos" ]; then
    if ! command -v go &>/dev/null; then
      echo "Go is required to build Auth on macOS."
      echo "Install: brew install go"
      exit 1
    fi

    local auth_src="$BIN_DIR/auth-src"
    if [ ! -d "$auth_src" ]; then
      echo "Cloning supabase/auth v${AUTH_VERSION}..."
      git clone --depth 1 --branch "v${AUTH_VERSION}" \
        https://github.com/supabase/auth.git "$auth_src"
    fi

    echo "Building Auth from source (this may take a moment)..."
    (cd "$auth_src" && go build -o "$BIN_DIR/auth" .)
    chmod +x "$BIN_DIR/auth"
    echo "Auth built -> $BIN_DIR/auth"
    return
  fi

  # Linux: download pre-built binary
  local asset
  case "$ARCH_LABEL" in
    x86_64) asset="auth-v${AUTH_VERSION}-x86.tar.gz"   ;;
    arm64)  asset="auth-v${AUTH_VERSION}-arm64.tar.gz"  ;;
  esac

  local url="https://github.com/supabase/auth/releases/download/v${AUTH_VERSION}/${asset}"
  echo "Downloading Auth v${AUTH_VERSION}..."
  echo "  $url"
  curl -fSL "$url" -o "$BIN_DIR/$asset"
  tar -xzf "$BIN_DIR/$asset" -C "$BIN_DIR"
  rm -f "$BIN_DIR/$asset"
  chmod +x "$BIN_DIR/auth"
  echo "Auth installed -> $BIN_DIR/auth"
}

download_postgrest() {
  if [ -x "$BIN_DIR/postgrest" ]; then
    echo "PostgREST binary cached at $BIN_DIR/postgrest — skipping download."
    return
  fi

  if ! command -v xz &>/dev/null; then
    echo "xz is required to extract PostgREST."
    echo "Install: brew install xz (macOS) / apt install xz-utils (Linux)"
    exit 1
  fi

  local asset
  case "${OS_LABEL}-${ARCH_LABEL}" in
    linux-x86_64)  asset="postgrest-v${POSTGREST_VERSION}-linux-static-x86-64.tar.xz" ;;
    linux-arm64)   asset="postgrest-v${POSTGREST_VERSION}-ubuntu-aarch64.tar.xz"       ;;
    macos-arm64)   asset="postgrest-v${POSTGREST_VERSION}-macos-aarch64.tar.xz"        ;;
    macos-x86_64)
      echo "No native PostgREST x86_64 macOS build; using aarch64 (Rosetta 2)."
      asset="postgrest-v${POSTGREST_VERSION}-macos-aarch64.tar.xz"
      ;;
    *) echo "No PostgREST binary for ${OS_LABEL}-${ARCH_LABEL}"; exit 1 ;;
  esac

  local url="https://github.com/PostgREST/postgrest/releases/download/v${POSTGREST_VERSION}/${asset}"
  echo "Downloading PostgREST v${POSTGREST_VERSION}..."
  echo "  $url"
  curl -fSL "$url" -o "$BIN_DIR/$asset"
  tar -xJf "$BIN_DIR/$asset" -C "$BIN_DIR"
  rm -f "$BIN_DIR/$asset"
  chmod +x "$BIN_DIR/postgrest"
  echo "PostgREST installed -> $BIN_DIR/postgrest"
}

# ── Step 1: Download binaries ───────────────────────────────────
echo "--- Downloading binaries ---"
download_auth
echo ""
download_postgrest
echo ""

# ── Step 2: Start pglited (in-memory) ──────────────────────────
echo "--- Starting pglited (in-memory, port $PG_PORT) ---"
# Set search_path to include auth schema for Supabase Auth compatibility
# Auth queries tables with unqualified names (e.g., "identities" instead of "auth.identities")
"$PGLITED" "memory://" "$PG_PORT" --daemon --extensions uuid_ossp,pgcrypto \
  --init-sql "SET search_path TO auth, public, api"

echo "Waiting for pglited..."
for i in $(seq 1 30); do
  if nc -z 127.0.0.1 "$PG_PORT" 2>/dev/null; then
    echo "pglited ready (${i}s)"
    break
  fi
  sleep 1
done

if ! nc -z 127.0.0.1 "$PG_PORT" 2>/dev/null; then
  echo "pglited failed to start."
  exit 1
fi
echo ""

# ── Step 3: Seed database ──────────────────────────────────────
echo "--- Seeding database ---"
deno run --allow-net --allow-env "$SCRIPT_DIR/seed.ts" "$PG_PORT"
echo ""

# ── Step 4: Start Auth ─────────────────────────────────────────
echo "--- Starting Supabase Auth (port $AUTH_PORT) ---"
env $(grep -v '^#' "$SCRIPT_DIR/auth.env" | grep -v '^\s*$' | xargs) \
  "$BIN_DIR/auth" &
AUTH_PID=$!

echo "Waiting for Auth..."
for i in $(seq 1 30); do
  if nc -z 127.0.0.1 "$AUTH_PORT" 2>/dev/null; then
    echo "Auth ready (${i}s)"
    break
  fi
  sleep 1
done

if ! nc -z 127.0.0.1 "$AUTH_PORT" 2>/dev/null; then
  echo "Auth failed to start. Check logs above for details."
  exit 1
fi
echo ""

# ── Step 5: Start PostgREST ────────────────────────────────────
echo "--- Starting PostgREST (port $POSTGREST_PORT) ---"
"$BIN_DIR/postgrest" "$SCRIPT_DIR/postgrest.conf" &
POSTGREST_PID=$!

echo "Waiting for PostgREST..."
for i in $(seq 1 15); do
  if nc -z 127.0.0.1 "$POSTGREST_PORT" 2>/dev/null; then
    echo "PostgREST ready (${i}s)"
    break
  fi
  sleep 1
done

if ! nc -z 127.0.0.1 "$POSTGREST_PORT" 2>/dev/null; then
  echo "PostgREST failed to start. Check logs above for details."
  exit 1
fi
echo ""

# ── Step 6: Verify ─────────────────────────────────────────────
echo "--- Verifying services ---"
echo ""

echo "Auth /health:"
curl -s "http://localhost:${AUTH_PORT}/health" | head -c 500
echo ""
echo ""

echo "PostgREST /products:"
curl -s "http://localhost:${POSTGREST_PORT}/products?limit=3" | head -c 500
echo ""
echo ""

# ── Summary ─────────────────────────────────────────────────────
echo "==========================================================="
echo "  Micro-Supabase is running!"
echo "==========================================================="
echo ""
echo "Services:"
echo "  pglited    -> postgresql://postgres:password@127.0.0.1:${PG_PORT}/template1"
echo "  Auth       -> http://localhost:${AUTH_PORT}"
echo "  PostgREST  -> http://localhost:${POSTGREST_PORT}"
echo ""
echo "Binaries stored in:"
echo "  $BIN_DIR/"
echo ""
echo "Sample commands:"
echo ""
echo "  # Sign up a new user"
echo "  curl -s -X POST http://localhost:${AUTH_PORT}/signup \\"
echo "    -H 'Content-Type: application/json' \\"
echo "    -d '{\"email\":\"user@example.com\",\"password\":\"testpassword123\"}'"
echo ""
echo "  # Get a JWT token"
echo "  curl -s -X POST http://localhost:${AUTH_PORT}/token?grant_type=password \\"
echo "    -H 'Content-Type: application/json' \\"
echo "    -d '{\"email\":\"user@example.com\",\"password\":\"testpassword123\"}'"
echo ""
echo "  # List products (anonymous)"
echo "  curl -s http://localhost:${POSTGREST_PORT}/products"
echo ""
echo "  # List products (authenticated — replace <TOKEN>)"
echo "  curl -s http://localhost:${POSTGREST_PORT}/products \\"
echo "    -H 'Authorization: Bearer <TOKEN>'"
echo ""
echo "Press Ctrl-C to stop all services."
wait
