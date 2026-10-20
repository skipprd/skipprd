#!/usr/bin/env bash
# Stage local plugin manifests when PLUGIN_BINARIES_DIR is set; otherwise
# leave discovery to the stamped skipprd host ({semver}/manifest-index.json).
e2e_pin_config() {
  local config="$1"
  if [[ -n "${SKIPPR_PLUGIN_PINS:-}" ]]; then
    python3 "${GITHUB_WORKSPACE}/.github/scripts/previous_protocol_compatible_release.py" \
      --pin-config "$config" \
      --pins "${SKIPPR_PLUGIN_PINS}"
  fi
}

e2e_stage_plugins() {
  local config="$1"
  local pipeline="$2"
  local output_dir="$3"
  mkdir -p "$output_dir"
  e2e_pin_config "$config"
  if [[ -n "${PLUGIN_BINARIES_DIR:-}" ]]; then
    mkdir -p target/release
    cp "${PLUGIN_BINARIES_DIR}"/* target/release/
    chmod +x target/release/skippr-plugin-* || true
    python3 "${GITHUB_WORKSPACE}/.github/scripts/local_runtime_plugins.py" \
      --config "$config" \
      --pipeline "$pipeline" \
      --output-dir "$output_dir" \
      --release \
      --skip-cargo-build
    export SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR="$output_dir"
    export USE_LOCAL_PLUGIN_CODE=1
  else
    unset USE_LOCAL_PLUGIN_CODE || true
    unset SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR || true
    export SKIPPR_RUNTIME_PLUGIN_DIR="$(dirname "$output_dir")/runtime-plugins"
    mkdir -p "$SKIPPR_RUNTIME_PLUGIN_DIR"
  fi
}

e2e_assert_downloaded() {
  if [[ -n "${PLUGIN_BINARIES_DIR:-}" ]]; then
    return 0
  fi
  python3 -c '
import sys
from pathlib import Path
sys.path.insert(0, str(Path(".github/scripts").resolve()))
from runtime_e2e_harness import assert_runtime_plugin_patterns_downloaded
assert_runtime_plugin_patterns_downloaded(Path(sys.argv[1]), tuple(sys.argv[2:]))
' "${SKIPPR_RUNTIME_PLUGIN_DIR:?}" "$@"
}
