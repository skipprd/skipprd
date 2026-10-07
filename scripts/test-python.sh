#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

python3 -m venv .venv-python
# shellcheck source=/dev/null
source .venv-python/bin/activate
# Host pip.conf / PIP_INDEX_URL (CodeArtifact, private indexes) must not apply.
unset PIP_INDEX_URL PIP_EXTRA_INDEX_URL PIP_TRUSTED_HOST
export PIP_CONFIG_FILE=/dev/null
export PIP_INDEX_URL=https://pypi.org/simple
python -m pip install -U pip
python -m pip install "maturin>=1.7,<2" "pyarrow>=17" pytest
# Maturin sets CARGO_ENCODED_RUSTFLAGS for the cdylib. Cargo applies those
# flags to build-scripts; Darwin then fails with `cannot execute binary file`.
# Isolate the maturin target, wrap cargo to drop the encoded flags, and pass
# macOS cdylib link args only to the final `cargo rustc` crate.
if [ "$(uname -s)" = Darwin ]; then
  export CARGO_TARGET_DIR="$ROOT/target/maturin"
  rm -rf "$CARGO_TARGET_DIR/debug/build" "$CARGO_TARGET_DIR/release/build"
  export RUSTC_WRAPPER="$ROOT/scripts/darwin-rustc-wrapper.py"
  chmod +x "$RUSTC_WRAPPER"
  mkdir -p "$ROOT/scripts/bin"
  REAL_CARGO="${CARGO:-$(command -v cargo)}"
  case "$REAL_CARGO" in
    "$ROOT/scripts/bin/cargo"|*/scripts/bin/cargo)
      REAL_CARGO="${CARGO_HOME:-$HOME/.cargo}/bin/cargo"
      ;;
  esac
  cat >"$ROOT/scripts/bin/cargo" <<EOF
#!/bin/sh
unset CARGO_ENCODED_RUSTFLAGS
unset CARGO
exec '${REAL_CARGO}' "\$@"
EOF
  chmod +x "$ROOT/scripts/bin/cargo"
  export PATH="$ROOT/scripts/bin:$PATH"
  export CARGO="$ROOT/scripts/bin/cargo"
  maturin develop --release -- -C link-arg=-undefined -C link-arg=dynamic_lookup
else
  maturin develop --release
fi
python -m pytest python/tests
if [ "$(uname -s)" = Darwin ]; then
  maturin build --release --out target/wheels -- -C link-arg=-undefined -C link-arg=dynamic_lookup
else
  maturin build --release --out target/wheels
fi
python - <<'PY'
from pathlib import Path

PYPI_WHEEL_MAX_BYTES = 100 * 1024 * 1024
wheels = sorted(Path("target/wheels").glob("*.whl"))
if not wheels:
    raise SystemExit("maturin build --release produced no wheels under target/wheels")
oversized = [path for path in wheels if path.stat().st_size > PYPI_WHEEL_MAX_BYTES]
if oversized:
    detail = ", ".join(f"{path.name}={path.stat().st_size}B" for path in oversized)
    raise SystemExit(
        f"skippr wheel exceeds PyPI project file limit ({PYPI_WHEEL_MAX_BYTES} bytes): {detail}"
    )
for path in wheels:
    print(f"{path} {path.stat().st_size}B")
PY
