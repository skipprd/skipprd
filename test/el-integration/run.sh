#!/usr/bin/env bash
#
# Integration test: MSSQL -> Snowflake EL via skippr
#
# Prerequisites:
#   export LLM_API_KEY=sk-...
#   export SNOWFLAKE_ACCOUNT=...
#   export SNOWFLAKE_USER=...
#   export SNOWFLAKE_PASSWORD=...
#
# Usage:
#   ./test/el-integration/run.sh
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SKIPPR_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

cleanup() {
  echo "==> Cleaning up Docker..."
  docker compose -f "$SCRIPT_DIR/docker-compose.yml" down -v 2>/dev/null || true
}
trap cleanup EXIT

# ── Preflight checks ────────────────────────────────────────────────
if [ -z "${LLM_API_KEY:-}" ]; then
  echo "WARNING: LLM_API_KEY is not set — LLM-dependent phases will fail."
fi

echo "==> [1/4] Building skipprd engine..."
cargo build --release --manifest-path "$SKIPPR_ROOT/Cargo.toml" --bin skipprd
SKIPPRD_BIN="$SKIPPR_ROOT/target/release/skipprd"
if [ ! -f "$SKIPPRD_BIN" ]; then
  echo "ERROR: skipprd binary not found at $SKIPPRD_BIN"
  exit 1
fi

echo "==> [2/4] Building skippr CLI..."
cargo build --release --manifest-path "$SKIPPR_ROOT/Cargo.toml" --bin skippr
SKIPPR_BIN="$SKIPPR_ROOT/target/release/skippr"
if [ ! -f "$SKIPPR_BIN" ]; then
  echo "ERROR: skippr binary not found at $SKIPPR_BIN"
  exit 1
fi

echo "==> [3/4] Starting MSSQL + seeding via Docker Compose..."
docker compose -f "$SCRIPT_DIR/docker-compose.yml" up -d

echo "    Waiting for seed service to complete..."
for i in $(seq 1 120); do
  seed_status=$(docker compose -f "$SCRIPT_DIR/docker-compose.yml" ps --format json seed 2>/dev/null || echo "")
  if echo "$seed_status" | grep -q '"exited"' 2>/dev/null; then
    seed_exit=$(docker compose -f "$SCRIPT_DIR/docker-compose.yml" ps -a --format json seed 2>/dev/null \
      | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('ExitCode', d.get('exit_code', -1)) if isinstance(d,dict) else d[0].get('ExitCode', d[0].get('exit_code', -1)))" 2>/dev/null || echo "-1")
    if [ "$seed_exit" = "0" ]; then
      echo "    Seed completed successfully."
      break
    else
      echo "    Seed FAILED (exit code: $seed_exit). Logs:"
      docker compose -f "$SCRIPT_DIR/docker-compose.yml" logs seed
      exit 1
    fi
  fi
  if [ "$i" -eq 120 ]; then
    echo "    Timeout waiting for seed — logs:"
    docker compose -f "$SCRIPT_DIR/docker-compose.yml" logs
    exit 1
  fi
  sleep 3
done

MSSQL_CONN="server=tcp:127.0.0.1,1433;database=testdb;user id=sa;password=Skippr!Test123;TrustServerCertificate=true"
export PATH="$(dirname "$SKIPPRD_BIN"):$PATH"
export MSSQL_CONNECTION_STRING="$MSSQL_CONN"
export OPENAI_API_KEY="${LLM_API_KEY}"
export SNOWFLAKE_PRIVATE_KEY_PATH="$SKIPPR_ROOT/snowflake_key.p8"

echo "==> [4/4] Running skippr..."
SKIPPR_DATA="$SKIPPR_ROOT/.react/local/dev/mssql_migration/skippr"
rm -rf "$SKIPPR_DATA"

"$SKIPPR_BIN" --log info run \
  --config "$SKIPPR_ROOT/react-snowflake.yaml"
EXIT_CODE=$?

echo ""
if [ $EXIT_CODE -eq 0 ]; then
  echo "Integration test PASSED."
else
  echo "Integration test FAILED (exit code: $EXIT_CODE)."
fi
exit $EXIT_CODE
