#!/usr/bin/env bash
set -euo pipefail

echo "Seeding fake-gcs-server..."
curl -sf -X POST "http://127.0.0.1:14443/storage/v1/b" \
  -H "Content-Type: application/json" \
  -d '{"name": "test-bucket"}' || true

echo "GCS seeded."
