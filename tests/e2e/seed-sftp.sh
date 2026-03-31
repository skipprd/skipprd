#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "Seeding SFTP..."
SFTP_CTR="$(docker compose -f "$SCRIPT_DIR/../../docker-compose.yml" ps -q sftp)"

docker cp "$SCRIPT_DIR/testdata/seed.json" "$SFTP_CTR:/home/testuser/upload/seed.json"

echo "SFTP seeded."
