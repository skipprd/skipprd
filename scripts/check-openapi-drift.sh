#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

cd "${REPO_ROOT}"

TARGET_PATHS=("react/src/ws/api_gen/src/models" "react/src/ws/api_gen/docs")
before="$(git status --porcelain -- "${TARGET_PATHS[@]}" | LC_ALL=C sort)"

# Regenerate from the authoritative source spec.
bash "${REPO_ROOT}/scripts/gen-openapi.sh"

after="$(git status --porcelain -- "${TARGET_PATHS[@]}" | LC_ALL=C sort)"

# Drift guard: generation should not introduce new deltas.
if [[ "${before}" != "${after}" ]]; then
  echo "ERROR: OpenAPI generated artifacts are out of date."
  echo "Run: bash scripts/gen-openapi.sh"
  echo "Then commit regenerated files under react/src/ws/api_gen/."
  git diff -- "react/src/ws/api_gen/src/models" "react/src/ws/api_gen/docs"
  exit 1
fi

echo "OpenAPI artifacts are up to date."
