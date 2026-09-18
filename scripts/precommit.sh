#!/usr/bin/env bash
# Local gate that must stay green before a commit: Python CI contracts and
# `cargo test -p skipprd --lib` (the same lib suite GitHub Actions runs).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:/opt/homebrew/bin:/usr/local/bin:${PATH}"

echo "precommit: python3 .github/scripts/test_python_bindings_ci.py"
python3 .github/scripts/test_python_bindings_ci.py

echo "precommit: cargo run -p skippr-connect-gen -- --check"
cargo run -p skippr-connect-gen -- --check

echo "precommit: cargo test -p skipprd --lib"
cargo test -p skipprd --lib
