#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

cleanup() {
  docker compose -f "$PROJECT_ROOT/docker-compose.yml" down
}

trap cleanup EXIT

export AWS_ACCESS_KEY_ID=test
export AWS_SECRET_ACCESS_KEY=test
export AWS_DEFAULT_REGION=us-east-1

SKIPPR_BIN="$PROJECT_ROOT/target/debug/skippr-el"
VENV_DIR="$SCRIPT_DIR/.venv"
VENV_PYTHON="$VENV_DIR/bin/python"
VENV_PIP="$VENV_DIR/bin/pip"
VENV_SODA="$VENV_DIR/bin/soda"

if [ ! -x "$VENV_PYTHON" ]; then
  python3 -m venv "$VENV_DIR"
fi

echo "Starting services..."
docker compose -f "$PROJECT_ROOT/docker-compose.yml" up -d --wait

echo "Seeding MSSQL..."
export PYTHON_BIN="$VENV_PYTHON"
"$VENV_PIP" install -q python-tds soda-core-postgres
bash "$SCRIPT_DIR/seed-mssql.sh"

echo "Seeding LocalStack..."
bash "$SCRIPT_DIR/seed-localstack.sh"

echo "Building local skippr-el binary..."
cargo build --bin skippr-el

echo "Running pipelines..."
for pipeline in test_mysql test_mssql test_dynamodb test_kinesis test_sqs test_s3 test_file test_http; do
  echo "  -> $pipeline"
  "$SKIPPR_BIN" sync --pipeline "$pipeline" --config "$SCRIPT_DIR/skippr-el.yml"
done

echo "  -> test_stdin (piped)"
cat "$SCRIPT_DIR/testdata/seed.json" | "$SKIPPR_BIN" sync --pipeline test_stdin --config "$SCRIPT_DIR/skippr-el.yml"

echo "Running soda checks..."
"$VENV_SODA" scan -d skippr_test -c "$SCRIPT_DIR/soda/configuration.yml" "$SCRIPT_DIR/soda/checks.yml"

echo "All tests passed!"
