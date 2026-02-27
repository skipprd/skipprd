#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
SPEC="${REPO_ROOT}/react/runtime/openapi/ws-core.yaml"
OUT_DIR="${REPO_ROOT}/react/runtime/src/ws/api_gen"

mkdir -p "${OUT_DIR}"

run_with_docker() {
  if command -v docker >/dev/null 2>&1; then
    docker run --rm -v "${REPO_ROOT}:/local" openapitools/openapi-generator-cli:v7.9.0 \
      generate -i /local/react/runtime/openapi/ws-core.yaml -g rust -o /local/react/runtime/src/ws/api_gen --global-property models
    return 0
  fi
  return 1
}

run_with_local_jar() {
  local JAR_PATH="${OPENAPI_GEN_JAR:-${REPO_ROOT}/openapi-generator-cli.jar}"
  if [ -f "${JAR_PATH}" ]; then
    java -jar "${JAR_PATH}" generate -i "${SPEC}" -g rust -o "${OUT_DIR}" --global-property models
    return 0
  fi
  return 1
}

if run_with_docker; then
  exit 0
elif run_with_local_jar; then
  exit 0
else
  echo "WARNING: Could not find Docker or local openapi-generator jar."
  echo "Core OpenAPI generation skipped."
  exit 0
fi
