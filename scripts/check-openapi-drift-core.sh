#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

cd "${REPO_ROOT}"

TARGET_PATHS=("react/runtime/src/ws/api_gen/src/models" "react/runtime/src/ws/api_gen/docs")
before="$(git status --porcelain -- "${TARGET_PATHS[@]}" | LC_ALL=C sort)"

bash "${REPO_ROOT}/scripts/gen-openapi-core.sh"

after="$(git status --porcelain -- "${TARGET_PATHS[@]}" | LC_ALL=C sort)"

if [[ "${before}" != "${after}" ]]; then
  echo "ERROR: Core OpenAPI generated artifacts are out of date."
  echo "Run: bash scripts/gen-openapi-core.sh"
  git diff -- "react/runtime/src/ws/api_gen/src/models" "react/runtime/src/ws/api_gen/docs"
  exit 1
fi

echo "Core OpenAPI artifacts are up to date."
