#!/usr/bin/env bash
# Fast local correctness gate for ComputeKind processor + parse/bind.
# Full chaos/sim/fuzz runs on git commit via scripts/precommit.sh.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
export SKIPPR_STREAM_TEST_PROFILE=smoke
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:/opt/homebrew/bin:/usr/local/bin:${PATH}"
echo "stream-transforms smoke: cargo test -p skipprd --lib -- processor:: compute::"
exec cargo test -p skipprd --lib -- processor:: compute::
