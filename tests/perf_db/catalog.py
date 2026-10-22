"""Load the skippr-perf-db catalog via the shared skippr-perf YAML parser."""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path
from typing import Any

HARNESS_DIR = Path(__file__).resolve().parent
PERF_DIR = HARNESS_DIR.parent / "perf"
CATALOG_PATH = HARNESS_DIR / "catalog.yaml"

_spec = importlib.util.spec_from_file_location(
    "skippr_perf_catalog",
    PERF_DIR / "catalog.py",
)
if _spec is None or _spec.loader is None:
    raise ImportError(f"cannot load {PERF_DIR / 'catalog.py'}")
_perf_catalog = importlib.util.module_from_spec(_spec)
sys.modules["skippr_perf_catalog"] = _perf_catalog
_spec.loader.exec_module(_perf_catalog)

OUT_OF_SUITE = _perf_catalog.OUT_OF_SUITE
required_perf_for = _perf_catalog.required_perf_for
scenario_ids = _perf_catalog.scenario_ids
scenario_kind = _perf_catalog.scenario_kind
validate_scenario_asserts = _perf_catalog.validate_scenario_asserts


def load_catalog(path: Path | None = None) -> dict[str, Any]:
    return _perf_catalog.load_catalog(path or CATALOG_PATH)


__all__ = [
    "CATALOG_PATH",
    "OUT_OF_SUITE",
    "load_catalog",
    "required_perf_for",
    "scenario_ids",
    "scenario_kind",
    "validate_scenario_asserts",
]
