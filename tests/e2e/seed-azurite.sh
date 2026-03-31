#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "Seeding Azurite (Azure Blob)..."
export AZURE_STORAGE_CONNECTION_STRING="DefaultEndpointsProtocol=http;AccountName=devstoreaccount1;AccountKey=Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==;BlobEndpoint=http://127.0.0.1:10000/devstoreaccount1;"

# Create container (ignore error if exists)
curl -sf -X PUT "http://127.0.0.1:10000/devstoreaccount1/test-container?restype=container" \
  -H "x-ms-version: 2020-10-02" \
  -H "x-ms-date: $(date -u '+%a, %d %b %Y %H:%M:%S GMT')" || true

echo "Azurite seeded."
