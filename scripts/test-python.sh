#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

python3 -m venv .venv-python
# shellcheck source=/dev/null
source .venv-python/bin/activate
python -m pip install -U pip
python -m pip install "maturin>=1.7,<2" "pyarrow>=17" pytest
maturin develop
python -m pytest python/tests/test_session.py
maturin build --out target/wheels
ls target/wheels/*.whl
