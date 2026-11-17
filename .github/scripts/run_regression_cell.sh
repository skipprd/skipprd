#!/usr/bin/env bash
# One skippr-regression cell. Catalog kind in tests/perf/run.py owns ingest vs query.
set -euo pipefail
chmod +x "${SKIPPRD_BIN}" || true
if [ -n "${PLUGIN_BINARIES_DIR:-}" ]; then
  chmod +x "${PLUGIN_BINARIES_DIR}"/skippr-plugin-* || true
fi
work="${PERF_WORK:-${RUNNER_TEMP:-/tmp}/skippr-regression}"
mkdir -p "${work}"
python3 tests/perf/run.py \
  --scenario "${PERF_SCENARIO}" \
  --scale "${PERF_SCALE:-ci}" \
  --profile "${PERF_PROFILE:-none}" \
  --work "${work}"
