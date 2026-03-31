#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

cleanup() {
  # Kill any background skippr processes
  [ -n "${SKIPPR_PID:-}" ] && kill "$SKIPPR_PID" 2>/dev/null || true
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

echo "Seeding MongoDB..."
bash "$SCRIPT_DIR/seed-mongodb.sh"

echo "Seeding RabbitMQ..."
bash "$SCRIPT_DIR/seed-rabbitmq.sh"

echo "Seeding Mosquitto (MQTT)..."
bash "$SCRIPT_DIR/seed-mosquitto.sh"

echo "Seeding Kafka..."
bash "$SCRIPT_DIR/seed-kafka.sh"

echo "Seeding SFTP..."
bash "$SCRIPT_DIR/seed-sftp.sh"

echo "Seeding Azurite..."
bash "$SCRIPT_DIR/seed-azurite.sh"

echo "Seeding GCS..."
bash "$SCRIPT_DIR/seed-gcs.sh"

echo "Seeding SNS..."
bash "$SCRIPT_DIR/seed-sns.sh"

echo "Seeding EventBridge..."
bash "$SCRIPT_DIR/seed-eventbridge.sh"

echo "Building local skippr-el binary..."
cargo build --bin skippr-el

# ── Standard source → postgres pipelines ──────────────────────────
echo "Running standard pipelines..."
for pipeline in test_mysql test_mssql test_dynamodb test_kinesis test_sqs test_s3 test_file test_http_client; do
  echo "  -> $pipeline"
  "$SKIPPR_BIN" sync --pipeline "$pipeline" --config "$SCRIPT_DIR/skippr-el.yml"
done

echo "  -> test_stdin (piped)"
cat "$SCRIPT_DIR/testdata/seed.json" | "$SKIPPR_BIN" sync --pipeline test_stdin --config "$SCRIPT_DIR/skippr-el.yml"

# ── New source plugins (push-based, batch mode) ──────────────────
echo "Running new source pipelines..."
for pipeline in test_mongodb test_amqp test_mqtt test_kafka test_sftp_in test_postgres_in test_sns test_eventbridge; do
  echo "  -> $pipeline"
  "$SKIPPR_BIN" sync --pipeline "$pipeline" --config "$SCRIPT_DIR/skippr-el.yml"
done

# ── Listener-based sources (skippr runs in background) ────────────
echo "Testing HttpServer source..."
"$SKIPPR_BIN" sync --pipeline test_http_server --config "$SCRIPT_DIR/skippr-el.yml" &
SKIPPR_PID=$!
sleep 3
curl -sf -X POST http://127.0.0.1:18082/ \
  -H "Content-Type: application/json" \
  -d @"$SCRIPT_DIR/testdata/seed.json" || true
sleep 2
kill "$SKIPPR_PID" 2>/dev/null || true
wait "$SKIPPR_PID" 2>/dev/null || true

# ── Output sink tests (file source → new sinks) ──────────────────
echo "Running output sink pipelines..."
for pipeline in test_sink_azure test_sink_sftp test_sink_amqp; do
  echo "  -> $pipeline"
  "$SKIPPR_BIN" sync --pipeline "$pipeline" --config "$SCRIPT_DIR/skippr-el.yml"
done

echo "Running soda checks..."
"$VENV_SODA" scan -d skippr_test -c "$SCRIPT_DIR/soda/configuration.yml" "$SCRIPT_DIR/soda/checks.yml"

echo "All tests passed!"
