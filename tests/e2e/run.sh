#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

echo "Starting services..."
docker compose -f "$PROJECT_ROOT/docker-compose.yml" up -d --wait

echo "Seeding MSSQL..."
docker compose -f "$PROJECT_ROOT/docker-compose.yml" exec -T mssql bash < "$SCRIPT_DIR/seed-mssql.sh"

echo "Seeding LocalStack..."
docker compose -f "$PROJECT_ROOT/docker-compose.yml" exec -T localstack bash < "$SCRIPT_DIR/seed-localstack.sh"

echo "Running pipelines..."
for pipeline in test_mysql test_mssql test_dynamodb test_kinesis test_sqs test_s3 test_file test_http; do
  echo "  -> $pipeline"
  skippr sync --pipeline "$pipeline" --config "$SCRIPT_DIR/skippr.yml"
done

echo "  -> test_stdin (piped)"
cat "$SCRIPT_DIR/testdata/seed.json" | skippr sync --pipeline test_stdin --config "$SCRIPT_DIR/skippr.yml"

echo "Running soda checks..."
pip install -q soda-core-postgres
soda scan -d skippr_test -c "$SCRIPT_DIR/soda/configuration.yml" "$SCRIPT_DIR/soda/checks.yml"

echo "Tearing down..."
docker compose -f "$PROJECT_ROOT/docker-compose.yml" down

echo "All tests passed!"
