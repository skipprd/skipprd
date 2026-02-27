#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
SPEC="${REPO_ROOT}/react/suites/data_engineer/openapi/ws-data-engineer.yaml"

if [[ ! -f "${SPEC}" ]]; then
  echo "ERROR: missing suite OpenAPI spec at ${SPEC}"
  exit 1
fi

bash "${REPO_ROOT}/scripts/gen-openapi-suite-data-engineer.sh" >/dev/null
echo "Suite OpenAPI spec is present (no generated artifacts configured)."
