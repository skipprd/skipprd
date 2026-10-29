#!/usr/bin/env bash
# Local gate that must stay green before a commit: Python CI contracts and
# `cargo test -p skipprd --lib` (the full lib suite, including stream-transform
# chaos/sim/fuzz on local FS Sled). Smoke-only iteration:
#   ./scripts/stream-transforms-smoke.sh
# Unset SKIPPR_STREAM_TEST_PROFILE so git commit always runs the full matrix.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:/opt/homebrew/bin:/usr/local/bin:${PATH}"
unset SKIPPR_STREAM_TEST_PROFILE || true

echo "precommit: python3 .github/scripts/test_python_bindings_ci.py"
python3 .github/scripts/test_python_bindings_ci.py

echo "precommit: python3 .github/scripts/test_rust_ci.py"
python3 .github/scripts/test_rust_ci.py

echo "precommit: python3 .github/scripts/test_release_sha_batteries.py"
python3 .github/scripts/test_release_sha_batteries.py

echo "precommit: python3 tests/tpc/test_run.py"
python3 tests/tpc/test_run.py

echo "precommit: python3 tests/perf/test_run.py"
python3 tests/perf/test_run.py

echo "precommit: python3 tests/perf_db/test_run.py"
python3 tests/perf_db/test_run.py

echo "precommit: python3 .github/scripts/test_stream_transforms_ci.py"
python3 .github/scripts/test_stream_transforms_ci.py

echo "precommit: python3 .github/scripts/test_publish_runtime_plugins.py"
python3 .github/scripts/test_publish_runtime_plugins.py

echo "precommit: python3 .github/scripts/test_bump_changed_plugin_crates.py"
python3 .github/scripts/test_bump_changed_plugin_crates.py

echo "precommit: cargo run -p skippr-connect-gen -- --check"
cargo run -p skippr-connect-gen -- --check

echo "precommit: cargo test -p skipprd --lib"
cargo test -p skipprd --lib
