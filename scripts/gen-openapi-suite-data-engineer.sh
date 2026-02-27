#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
SPEC="${REPO_ROOT}/react/suites/data_engineer/openapi/ws-data-engineer.yaml"

if [[ ! -f "${SPEC}" ]]; then
  echo "ERROR: missing suite OpenAPI spec at ${SPEC}"
  exit 1
fi

echo "Suite OpenAPI spec present: ${SPEC}"
echo "Client composition should combine this spec with react/runtime/openapi/ws-core.yaml."
