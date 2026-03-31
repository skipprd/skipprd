#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "Seeding Mosquitto (MQTT)..."
while IFS= read -r line; do
  [ -z "$line" ] && continue
  docker exec "$(docker compose -f "$SCRIPT_DIR/../../docker-compose.yml" ps -q mosquitto)" \
    mosquitto_pub -h localhost -t "test/data" -r -m "$line"
done < "$SCRIPT_DIR/testdata/seed.json"

echo "Mosquitto seeded."
