#!/usr/bin/env bash
# One skippr-regression cell. q-* is tests/perf_db/run.py; everything else is tests/perf/run.py.
set -euo pipefail
chmod +x "${SKIPPRD_BIN}" || true
if [ -n "${PLUGIN_BINARIES_DIR:-}" ]; then
  chmod +x "${PLUGIN_BINARIES_DIR}"/skippr-plugin-* || true
fi
work="${PERF_WORK:-${RUNNER_TEMP:-/tmp}/skippr-regression}"
mkdir -p "${work}"
if [[ "${PERF_SCENARIO}" == q-* ]]; then
  runner=tests/perf_db/run.py
else
  runner=tests/perf/run.py
fi
python3 "${runner}" \
  --scenario "${PERF_SCENARIO}" \
  --scale "${PERF_SCALE:-ci}" \
  --profile "${PERF_PROFILE:-none}" \
  --work "${work}"
