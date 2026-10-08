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
python -m pip install pip==26.2.1
python -m pip install maturin==1.15.0 pyarrow==25.0.1 pyarrow-stubs==20.0.0.20260819 pytest==9.1.1 mypy==2.4.0
if [ "$(uname -s)" = Linux ]; then
  python -m pip install ziglang==0.15.2
fi
rm -rf target/wheels
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
  maturin build --release --out target/wheels -- -C link-arg=-undefined -C link-arg=dynamic_lookup
else
  # manylinux_2_28 (glibc 2.28) installs on every current Linux, not only
  # on hosts as new as the build runner.
  maturin build --release --compatibility manylinux_2_28 --zig --out target/wheels
fi
python -m pip install --force-reinstall --no-deps target/wheels/*.whl
python -m pytest python/tests
# Type-check the installed wheel, not the source tree's `skippr.pyi`.
(
  cd "$(mktemp -d)"
  python -m mypy.stubtest skippr --allowlist "$ROOT/python/stubtest-allowlist.txt"
  python -m mypy --strict --warn-unused-ignores "$ROOT/python/tests/typing"
)
python - <<'PY'
import zipfile
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
    with zipfile.ZipFile(path) as wheel:
        names = set(wheel.namelist())
    for required in ("skippr/__init__.pyi", "skippr/py.typed"):
        if required not in names:
            raise SystemExit(f"{path.name} is missing {required}")
    if "linux" in path.name and "manylinux_2_28" not in path.name:
        raise SystemExit(f"{path.name} must be tagged manylinux_2_28")
    print(f"{path} {path.stat().st_size}B")
PY
