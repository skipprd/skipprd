#!/usr/bin/env bash
# Run cargo against skipprd using path patches into a sibling ../react checkout (no react-cargo token).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REACT_ROOT="${SKIPPR_REACT_ROOT:-$(cd "$ROOT/../react" 2>/dev/null && pwd || true)}"

if [[ -z "$REACT_ROOT" || ! -f "$REACT_ROOT/Cargo.toml" ]]; then
  echo "error: local react checkout not found. Set SKIPPR_REACT_ROOT or clone react next to skipprd." >&2
  exit 1
fi

PATCH_ARGS=()
add_patch() {
  PATCH_ARGS+=(--config "patch.\"react-cargo\".$1.path=\"$2\"")
}

add_patch react "$REACT_ROOT/src/runtime"
add_patch react-core "$REACT_ROOT/src/core"
add_patch react-http-protocol "$REACT_ROOT/src/http-protocol"
add_patch react-transport "$REACT_ROOT/src/transport"
add_patch react-view "$REACT_ROOT/src/view"
add_patch react-module-storage-s3 "$REACT_ROOT/src/modules/adaptors/storage-s3"
add_patch react-module-storage-local "$REACT_ROOT/src/modules/adaptors/storage-local"
add_patch react-module-storage-memory "$REACT_ROOT/src/modules/adaptors/storage-memory"
add_patch react-module-provider-vector-lance "$REACT_ROOT/src/modules/providers/vector-lance"
add_patch react-suite-debugger "$REACT_ROOT/src/suites/suite_debugger"

cd "$ROOT"
exec cargo "${PATCH_ARGS[@]}" "$@"
