#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
PGLITED="$REPO_ROOT/target/release/pglited"
DATA_DIR="/tmp/pglited_supabase_demo"
PGLITED_PORT=54321
SUPABASE_DB_PORT=54322
DUMP_FILE="/tmp/pglited_dump.sql"

cleanup_all() {
  echo "🛑 Shutting down all existing processes..."  
  if nc -z 127.0.0.1 "$PGLITED_PORT" 2>/dev/null; then
    echo "   Killing process on port $PGLITED_PORT..."
    lsof -ti:$PGLITED_PORT 2>/dev/null | xargs kill -9 2>/dev/null || true
    sleep 2
  fi
  
  if nc -z 127.0.0.1 "$SUPABASE_DB_PORT" 2>/dev/null; then
    echo "   Killing process on port $SUPABASE_DB_PORT..."
    lsof -ti:$SUPABASE_DB_PORT 2>/dev/null | xargs kill -9 2>/dev/null || true
    sleep 2
  fi
  
  pkill -f "supabase start" 2>/dev/null || true
  pkill -f "supabase db start" 2>/dev/null || true
  sleep 2
  
  if [ -d "$DATA_DIR" ]; then
    echo "   Removing existing data directory $DATA_DIR..."
    rm -rf "$DATA_DIR" 2>/dev/null || true
  fi
  
  if [ -f "$DUMP_FILE" ]; then
    echo "   Removing existing dump file $DUMP_FILE..."
    rm -f "$DUMP_FILE" 2>/dev/null || true
  fi
  
  if [ -d "supabase" ]; then
    echo "   Removing Supabase Docker volume supabase_db_supabase-port..."
    docker volume rm supabase_db_supabase-port 2>/dev/null || true
    echo "   Removing existing supabase directory..."
    rm -rf supabase 2>/dev/null || true
  fi
  
  echo "   ✅ All processes stopped and cleaned"
  echo ""
}

cleanup_all

cleanup_final() {
  echo ""
  echo "🧹 Cleaning up..."  
  if [ -n "${SUPABASE_RUNNING:-}" ]; then
    echo "   Stopping Supabase CLI..."
    supabase stop || true
  fi
  
  echo "   Stopping pglited (PID: $PGLITED_PID)..."
  kill "$PGLITED_PID" 2>/dev/null || true
  sleep 2
  
  lsof -ti:$PGLITED_PORT 2>/dev/null | xargs kill -9 2>/dev/null || true
  lsof -ti:$SUPABASE_DB_PORT 2>/dev/null | xargs kill -9 2>/dev/null || true
  pkill -f "pglited" 2>/dev/null || true
  pkill -f "supabase" 2>/dev/null || true
  
  if [ -f "$DUMP_FILE" ]; then
    echo "   Removing dump file $DUMP_FILE..."
    rm -f "$DUMP_FILE" 2>/dev/null || true
  fi
  
  if [ -d "supabase" ]; then
    echo "   Removing Supabase Docker volume supabase_db_supabase-port..."
    docker volume rm supabase_db_supabase-port 2>/dev/null || true
  fi
  
  echo "   ✅ Cleanup complete!"
}

trap cleanup_final EXIT

echo "═════════════════════════════════════════════════════════"
echo "  Pglited → Supabase CLI Migration Demo"
echo "═══════════════════════════════════════════════════════════"
echo ""

check_command() {
  if ! command -v "$1" &>/dev/null; then
    echo "❌ Error: $1 not found"
    case "$1" in
      pglited)
        echo "   Run 'make build-release' in $REPO_ROOT"
        ;;
      deno)
        echo "   Install from https://deno.land"
        ;;
      docker)
        echo "   Install Docker and ensure it's running"
        ;;
      supabase)
        echo "   Run: brew install supabase/tap/supabase"
        ;;
    esac
    exit 1
  fi
}

check_pglited_binary() {
  if [ ! -x "$PGLITED" ]; then
    echo "❌ pglited binary not found at $PGLITED"
    echo "   Run 'make build-release' in $REPO_ROOT"
    exit 1
  fi
}

check_command docker
check_command supabase
check_command deno
check_pglited_binary

echo "✅ All prerequisites found"
echo ""

echo "📦 Step 1: Starting pglited in file mode..."
echo "   Data directory: $DATA_DIR"
echo "   Port: $PGLITED_PORT"
"$PGLITED" "$DATA_DIR" "$PGLITED_PORT" --daemon &
PGLITED_PID=$!

echo "   ⏳ Waiting for pglited to be ready..."
for i in $(seq 1 30); do
  if nc -z 127.0.0.1 "$PGLITED_PORT" 2>/dev/null; then
    echo "   ✅ pglited ready on port $PGLITED_PORT (took ${i}s)"
    break
  fi
  sleep 1
done

if ! nc -z 127.0.0.1 "$PGLITED_PORT" 2>/dev/null; then
  echo "   ❌ pglited failed to start"
  exit 1
fi
echo ""

echo "🌱 Step 2: Seeding database with sample data..."
deno run --allow-net --allow-env "$SCRIPT_DIR/seed.ts" "$PGLITED_PORT"
echo ""

echo "📦 Step 3: Creating Supabase project structure..."
if [ -d "supabase" ]; then
  echo "   ⚠️  Supabase directory exists, removing..."
  rm -rf supabase
fi

supabase init
echo "   ✅ Supabase project initialized"
echo ""

echo "💾 Step 4: Dumping pglited database..."
PGPASSWORD=password PGSSLMODE=disable pg_dump \
  -h 127.0.0.1 \
  -p "$PGLITED_PORT" \
  -U postgres \
  -d template1 \
  --no-owner \
  --no-acl \
  --schema=app \
  > "$DUMP_FILE"

if [ ! -f "$DUMP_FILE" ] || [ ! -s "$DUMP_FILE" ]; then
  echo "   ❌ Failed to dump database"
  exit 1
fi

DUMP_SIZE=$(du -h "$DUMP_FILE" | cut -f1)
echo "   ✅ Database dumped to $DUMP_FILE ($DUMP_SIZE)"
echo ""

echo "🚀 Step 5: Starting Supabase database only..."
supabase db start &
SUPABASE_RUNNING=1

echo "   ⏳ Waiting for Supabase database to be ready..."
sleep 5

if ! nc -z 127.0.0.1 "$SUPABASE_DB_PORT" 2>/dev/null; then
  echo "   ❌ Supabase failed to start"
  exit 1
fi

echo "   ✅ Supabase database ready on port $SUPABASE_DB_PORT"
echo ""

echo "📥 Step 6: Restoring dump to Supabase..."
PGPASSWORD=postgres PGSSLMODE=disable psql \
  -h 127.0.0.1 \
  -p "$SUPABASE_DB_PORT" \
  -U postgres \
  -d postgres \
  < "$DUMP_FILE"

echo "   ✅ Database restored to Supabase"
echo ""

echo "🔍 Step 7: Verifying data migration..."
echo ""
echo "   PGLITED:"
PGPASSWORD=password PGSSLMODE=disable psql -h 127.0.0.1 -p "$PGLITED_PORT" -U postgres -d template1 -c "
  SELECT 'Products' as table_name, COUNT(*) as row_count FROM app.products
  UNION ALL
  SELECT 'Users', COUNT(*) FROM app.users
  UNION ALL
  SELECT 'Orders', COUNT(*) FROM app.orders
  UNION ALL
  SELECT 'Order Items', COUNT(*) FROM app.order_items
  ORDER BY table_name;
"
echo ""
echo "   SUPABASE:"
PGPASSWORD=postgres PGSSLMODE=disable psql -h 127.0.0.1 -p "$SUPABASE_DB_PORT" -U postgres -d postgres -c "
  SELECT 'Products' as table_name, COUNT(*) as row_count FROM app.products
  UNION ALL
  SELECT 'Users', COUNT(*) FROM app.users
  UNION ALL
  SELECT 'Orders', COUNT(*) FROM app.orders
  UNION ALL
  SELECT 'Order Items', COUNT(*) FROM app.order_items
  ORDER BY table_name;
"
echo ""

echo "📋 Step 8: Sample data from both databases..."
echo ""
echo "   PGLITED - First 5 products:"
PGPASSWORD=password PGSSLMODE=disable psql -h 127.0.0.1 -p "$PGLITED_PORT" -U postgres -d template1 -c "SELECT id, name, category, price, in_stock FROM app.products ORDER BY id LIMIT 5;"
echo ""
echo "   SUPABASE - First 5 products:"
PGPASSWORD=postgres PGSSLMODE=disable psql -h 127.0.0.1 -p "$SUPABASE_DB_PORT" -U postgres -d postgres -c "SELECT id, name, category, price, in_stock FROM app.products ORDER BY id LIMIT 5;"
echo ""

echo "💰 Orders by User:"
echo ""
echo "   PGLITED:"
PGPASSWORD=password PGSSLMODE=disable psql -h 127.0.0.1 -p "$PGLITED_PORT" -U postgres -d template1 -c "
  SELECT u.id, u.name, COUNT(o.id) as order_count, COALESCE(SUM(o.total), 0) as total_spent
  FROM app.users u
  LEFT JOIN app.orders o ON u.id = o.user_id
  GROUP BY u.id, u.name
  ORDER BY u.id;
"
echo ""
echo "   SUPABASE:"
PGPASSWORD=postgres PGSSLMODE=disable psql -h 127.0.0.1 -p "$SUPABASE_DB_PORT" -U postgres -d postgres -c "
  SELECT u.id, u.name, COUNT(o.id) as order_count, COALESCE(SUM(o.total), 0) as total_spent
  FROM app.users u
  LEFT JOIN app.orders o ON u.id = o.user_id
  GROUP BY u.id, u.name
  ORDER BY u.id;
"
echo ""
echo ""

echo "📈 Products by Category:"
echo ""
echo "   PGLITED:"
PGPASSWORD=password PGSSLMODE=disable psql -h 127.0.0.1 -p "$PGLITED_PORT" -U postgres -d template1 -c "
  SELECT category, COUNT(*) as product_count, MIN(price) as min_price, MAX(price) as max_price, AVG(price) as avg_price
  FROM app.products
  GROUP BY category
  ORDER BY category;
"
echo ""
echo "   SUPABASE:"
PGPASSWORD=postgres PGSSLMODE=disable psql -h 127.0.0.1 -p "$SUPABASE_DB_PORT" -U postgres -d postgres -c "
  SELECT category, COUNT(*) as product_count, MIN(price) as min_price, MAX(price) as max_price, AVG(price) as avg_price
  FROM app.products
  GROUP BY category
  ORDER BY category;
"
echo ""
echo ""

echo "═════════════════════════════════════════════════════════════"
echo "  ✅ Migration Demo Complete!"
echo "═════════════════════════════════════════════════════════════"
echo ""
echo "📝 Summary:"
echo "   • Started pglited in file mode"
echo "   • Seeded database with products, users, orders"
echo "   • Dumped database using pg_dump"
echo "   • Started Supabase database only (gotrue + postgres)"
echo "   • Restored dump to Supabase database"
echo "   • Verified data integrity across both systems"
echo ""
echo "🔌 Connection Strings:"
echo "   Pglited:   postgresql://postgres:password@127.0.0.1:$PGLITED_PORT/template1"
echo "   Supabase:  postgresql://postgres:postgres@127.0.0.1:$SUPABASE_DB_PORT/postgres"
echo ""
echo "🧪 Try it yourself:"
echo "   PSQL to pglited:   PGPASSWORD=password PGSSLMODE=disable psql -h 127.0.0.1 -p $PGLITED_PORT -U postgres -d template1"
echo "   PSQL to Supabase:  PGPASSWORD=postgres PGSSLMODE=disable psql -h 127.0.0.1 -p $SUPABASE_DB_PORT -U postgres -d postgres"
echo ""
echo "Press Ctrl+C to stop and cleanup..."

wait
