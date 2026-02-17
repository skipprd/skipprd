#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
SPEC="${REPO_ROOT}/ask-ws.yaml"
OUT_DIR="${REPO_ROOT}/react/src/ws/api_gen"

mkdir -p "${OUT_DIR}"

echo "Generating Rust models from ${SPEC} into ${OUT_DIR}"

run_with_docker() {
	if command -v docker >/dev/null 2>&1; then
		echo "Using Docker openapi-generator-cli..."
		docker run --rm -v "${REPO_ROOT}:/local" openapitools/openapi-generator-cli:v7.9.0 \
			generate -i /local/ask-ws.yaml -g rust -o /local/react/src/ws/api_gen --global-property models
		return 0
	else
		return 1
	fi
}

run_with_local_jar() {
	local JAR_PATH="${OPENAPI_GEN_JAR:-${REPO_ROOT}/openapi-generator-cli.jar}"
	if [ -f "${JAR_PATH}" ]; then
		echo "Using local JAR at ${JAR_PATH}..."
		java -jar "${JAR_PATH}" generate -i "${SPEC}" -g rust -o "${OUT_DIR}" --global-property models
		return 0
	else
		return 1
	fi
}

if run_with_docker; then
	echo "Generation complete (Docker)."
elif run_with_local_jar; then
	echo "Generation complete (local JAR)."
else
	echo "WARNING: Could not find Docker or local openapi-generator jar."
	echo "Leaving existing models in ${OUT_DIR} unchanged."
	echo "To install: curl -LO https://repo1.maven.org/maven2/org/openapitools/openapi-generator-cli/7.9.0/openapi-generator-cli-7.9.0.jar"
	echo "Then set OPENAPI_GEN_JAR to that file and re-run."
	exit 0
fi

# openapi-generator (rust/models-only) occasionally fails to add newly generated models
# to src/models/mod.rs. Ensure ExecutionContext is exported when present.
MODELS_MOD="${OUT_DIR}/src/models/mod.rs"
if [ -f "${OUT_DIR}/src/models/execution_context.rs" ] && command -v python3 >/dev/null 2>&1; then
	MODELS_MOD="${MODELS_MOD}" python3 - <<'PY'
from pathlib import Path
import os

mod_path = Path(os.environ["MODELS_MOD"])
txt = mod_path.read_text(encoding="utf-8")
if "pub mod execution_context;" in txt and "pub use execution_context::ExecutionContext;" in txt:
    raise SystemExit(0)

# Insert near other message/context-ish models (after llm_end_response export if present).
needle = "pub mod llm_end_response;\npub use llm_end_response::LlmEndResponse;\n"
insert = needle + "\npub mod execution_context;\npub use execution_context::ExecutionContext;\n"
if needle in txt:
    txt = txt.replace(needle, insert, 1)
else:
    # Fallback: prepend at top.
    txt = "pub mod execution_context;\npub use execution_context::ExecutionContext;\n\n" + txt
mod_path.write_text(txt, encoding="utf-8")
PY
fi

