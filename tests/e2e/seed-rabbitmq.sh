#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "Seeding RabbitMQ..."
RABBITMQ_CTR="$(docker compose -f "$SCRIPT_DIR/../../docker-compose.yml" ps -q rabbitmq)"

docker exec "$RABBITMQ_CTR" rabbitmqadmin declare queue name=test_queue durable=true

while IFS= read -r line; do
  [ -z "$line" ] && continue
  docker exec "$RABBITMQ_CTR" rabbitmqadmin publish exchange=amq.default routing_key=test_queue payload="$line"
done < "$SCRIPT_DIR/testdata/seed.json"

echo "RabbitMQ seeded."
