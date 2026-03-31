#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "Seeding Kafka..."
KAFKA_CTR="$(docker compose -f "$SCRIPT_DIR/../../docker-compose.yml" ps -q kafka)"

docker exec "$KAFKA_CTR" kafka-topics.sh \
  --bootstrap-server localhost:9092 \
  --create --if-not-exists \
  --topic test_topic \
  --partitions 1 \
  --replication-factor 1

while IFS= read -r line; do
  [ -z "$line" ] && continue
  echo "$line" | docker exec -i "$KAFKA_CTR" kafka-console-producer.sh \
    --bootstrap-server localhost:9092 \
    --topic test_topic
done < "$SCRIPT_DIR/testdata/seed.json"

echo "Kafka seeded."
