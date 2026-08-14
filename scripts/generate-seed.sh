#!/usr/bin/env bash
# Regenerates assets/pglite_npm/dist/pgdata_seed.tar, the pre-initialised
# PostgreSQL data directory that file:// databases are bootstrapped from.
#
# Run this after changing the PGlite version or the bundled extension set:
#
#   mise run seed
#
# The seed is built by the pglited binary itself, so a debug build must exist.
# Extensions that cannot be created (preload-only modules such as auto_explain)
# are skipped; run with PGLITE_DEBUG=1 to see which.
set -euo pipefail

cd "$(dirname "$0")/.."

DIST_DIR="assets/pglite_npm/dist"
SEED_PATH="$DIST_DIR/pgdata_seed.tar"
BINARY="${PGLITED_BINARY:-target/debug/pglited}"

if [ ! -d "$DIST_DIR" ]; then
  echo "error: $DIST_DIR not found; run 'cargo build' first to download PGlite assets" >&2
  exit 1
fi

if [ ! -x "$BINARY" ]; then
  echo "error: $BINARY not found; run 'cargo build' first" >&2
  exit 1
fi

# The seed is deliberately extension-free. Running CREATE EXTENSION here writes
# catalog entries and shared_preload_libraries settings that reference shared
# objects only present when that extension is passed at runtime, so a seeded
# database would refuse to start ("could not access file pg_stat_statements").
# Extensions are loaded per-run with --extensions instead.
#
# Set PGLITED_SEED_EXTENSIONS to bake some in anyway, at your own risk.
extensions="${PGLITED_SEED_EXTENSIONS:-}"

echo "Generating $SEED_PATH"
echo "Extensions: ${extensions:-<none>}"

# Build to a temporary file so a failure leaves the existing seed intact.
tmp_seed="$(mktemp -t pgdata_seed)"
trap 'rm -f "$tmp_seed"' EXIT

if [ -n "$extensions" ]; then
  "$BINARY" --dump-datadir "$tmp_seed" --extensions "$extensions"
else
  "$BINARY" --dump-datadir "$tmp_seed"
fi

mv "$tmp_seed" "$SEED_PATH"
trap - EXIT

echo "Wrote $SEED_PATH ($(du -h "$SEED_PATH" | cut -f1))"
echo "Rebuild to embed it: cargo build"
