#!/usr/bin/env bash
# Local DFS keyword-hub sync with fixture API responses + console bundled metadata.
#
# Proves fast-path ingest against patched metadata (run_date date_candidate).
#
# Usage:
#   AWS_PROFILE=skippr-prod AWS_DEFAULT_REGION=eu-west-1 ./scripts/local-dfs-hub-sync.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CONSOLE_WEB="$(cd "$ROOT/../../upfoundry/console-web" && pwd)"
CONFIG="${ROOT}/local-dfs-keyword-hub.yml"
TENANT="local-dfs-hub-debug"
WORKSPACE="default"
PIPELINE="dataforseo_seo_opportunities"
DATA_DIR="${ROOT}/.skippr/local-dfs-hub-debug"
FIXTURE_DIR="${ROOT}/plugins/data_source/dataforseo_seo_opportunities/fixtures"
GLUE_DB="upfoundry_local_dfs_hub_debug"
BUCKET="upfoundry-prod-datalake"
REGION="${AWS_DEFAULT_REGION:-eu-west-1}"

export AWS_PROFILE="${AWS_PROFILE:-skippr-prod}"
export AWS_DEFAULT_REGION="${REGION}"

# Runtime plugin subprocesses may ignore AWS_PROFILE and pick quarantined static keys
# from ~/.aws/credentials; export the assumed-role session for the whole tree.
if creds="$(AWS_PROFILE="${AWS_PROFILE}" aws configure export-credentials --format env 2>/dev/null)"; then
  # shellcheck disable=SC1090
  eval "${creds}"
  unset AWS_PROFILE AWS_DEFAULT_PROFILE
fi
export SKIPPR_CONFIG_FILE="${CONFIG}"
export DATA_DIR="${DATA_DIR}"
export SKIPPR_PIPELINE="${PIPELINE}"
export SKIPPR_WORKSPACE="${WORKSPACE}"
export TENANT="${TENANT}"
export SKIPPRD_EL_STORAGE_MODE=local
export SKIPPR_DATAFORSEO_SEO_OPPORTUNITIES_FIXTURE_DIR="${FIXTURE_DIR}"
export DATAFORSEO_API_USER=fixture
export DATAFORSEO_API_PASS=fixture
export USE_LOCAL_PLUGIN_CODE=1
export RUST_LOG="${RUST_LOG:-skipprd=info,skippr_plugin=info}"
export DATA_DIR_MIN_FREE_BYTES=0
export DATA_DIR_HIGH_WATERMARK_PCT=0
export DATA_DIR_LOW_WATERMARK_PCT=0

HUB_TABLES=(
  dataforseo_seo_opportunities_keyword_suggestion_daily
  dataforseo_seo_opportunities_keyword_metric_daily
  dataforseo_seo_opportunities_opportunity_score_daily
  dataforseo_seo_opportunities_site_run_daily
  dataforseo_seo_opportunities_seed_keyword_daily
)

echo "==> Patch console bundled metadata"
python3 "${CONSOLE_WEB}/scripts/patch-pipeline-metadata.py"

META_SRC="${CONSOLE_WEB}/skippr/pipeline-metadata/dataforseo_seo_opportunities.json"
META_DST="${DATA_DIR}/${TENANT}/${WORKSPACE}/${PIPELINE}/metadata/metadata.json"
mkdir -p "$(dirname "${META_DST}")"
cp "${META_SRC}" "${META_DST}"
echo "==> Installed metadata at ${META_DST}"

echo "==> Reset local WAL / offsets / work dirs"
rm -rf "${DATA_DIR}/source" "${DATA_DIR}/output" "${DATA_DIR}/segment_buffer" \
  "${DATA_DIR}/sled" "${DATA_DIR}/sled.tmp" "${DATA_DIR}/runtime_source_children"

echo "==> Reset isolated Glue tables + iceberg prefixes (best effort)"
for table in "${HUB_TABLES[@]}"; do
  aws glue delete-table --database-name "${GLUE_DB}" --name "${table}" 2>/dev/null || true
  aws s3 rm "s3://${BUCKET}/datalake/${TENANT}/iceberg/${table}/" --recursive 2>/dev/null || true
done
aws glue create-database --database-input "{\"Name\":\"${GLUE_DB}\"}" 2>/dev/null || true

echo "==> Build local runtime plugins"
MANIFEST_DIR="$(python3 "${ROOT}/.github/scripts/local_runtime_plugins.py" \
  --config "${CONFIG}" \
  --pipeline "${PIPELINE}")"
export SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR="${MANIFEST_DIR}"

echo "==> Build skipprd"
cargo build -p skipprd

echo "==> Run skipprd sync (fixture DFS hub)"
"${ROOT}/target/debug/skipprd" --config "${CONFIG}" sync --pipeline "${PIPELINE}" --once 2>&1 | tee /tmp/local-dfs-hub-sync.log

echo "==> Glue tables"
aws glue get-tables --database-name "${GLUE_DB}" --query 'TableList[].Name' --output text

echo "Done. Log: /tmp/local-dfs-hub-sync.log"
