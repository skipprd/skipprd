#!/usr/bin/env bash
# One skippr-perf cell: catalog id + tests/perf/run.py. Invoked by e2e/perf and e2e/heaptrack.
set -euo pipefail
chmod +x "${SKIPPRD_BIN}" || true
if [ -n "${PLUGIN_BINARIES_DIR:-}" ]; then
  chmod +x "${PLUGIN_BINARIES_DIR}"/skippr-plugin-* || true
fi
work="${PERF_WORK:-${RUNNER_TEMP:-/tmp}/skippr-perf}"
mkdir -p "${work}"
python3 tests/perf/run.py \
  --scenario "${PERF_SCENARIO}" \
  --scale "${PERF_SCALE:-ci}" \
  --profile "${PERF_PROFILE:-none}" \
  --work "${work}"
