#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

python3 -m venv .venv-python
# shellcheck source=/dev/null
source .venv-python/bin/activate
python -m pip install -U pip
python -m pip install "maturin>=1.7,<2" "pyarrow>=17" pytest
# Maturin sets CARGO_ENCODED_RUSTFLAGS for the cdylib. Reusing cargo-test's
# target dir rebuilds build-scripts with those flags and Darwin then fails
# with `cannot execute binary file` (ENOEXEC). Isolate maturin artifacts.
if [ "$(uname -s)" = Darwin ]; then
  export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/var/tmp/cargo-target}-maturin"
  export RUSTC_WRAPPER="$ROOT/scripts/darwin-rustc-wrapper.py"
  chmod +x "$RUSTC_WRAPPER"
fi
maturin develop
python -m pytest python/tests
maturin build --out target/wheels
ls target/wheels/*.whl
