#!/usr/bin/env bash
set -euo pipefail

CH_URL="${CLICKHOUSE_URL:-http://localhost:18123}"

echo "Seeding ClickHouse..."

curl -s "$CH_URL" --data-binary "CREATE DATABASE IF NOT EXISTS skippr_test"
curl -s "$CH_URL" --data-binary "
  CREATE TABLE IF NOT EXISTS skippr_test.events (
    id UInt64,
    name String,
    value Float64,
    ts DateTime DEFAULT now()
  ) ENGINE = MergeTree() ORDER BY id
"
curl -s "$CH_URL" --data-binary "
  INSERT INTO skippr_test.events (id, name, value) VALUES
    (1, 'alpha', 1.1),
    (2, 'beta', 2.2),
    (3, 'gamma', 3.3)
"

echo "ClickHouse seeded."
