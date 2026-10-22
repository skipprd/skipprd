"""Load the skippr-perf scenario catalog (indent YAML subset)."""

from __future__ import annotations

from pathlib import Path
from typing import Any

HARNESS_DIR = Path(__file__).resolve().parent
CATALOG_PATH = HARNESS_DIR / "catalog.yaml"
OUT_OF_SUITE = (
    "import",
    "wal_import",
    "snowflake",
    "bigquery",
    "databricks",
    "redshift",
    "athena",
    "cloudcycle",
)

KNOWN_ASSERT_KEYS = frozenset(
    {
        "exact_arrow_eq_total",
        "exact_fallback_rows",
        "exact_arrow_rows",
        "bronze_count",
        "amount_sum",
        "leftover_seg_count",
        "identity",
    }
)

SCENARIO_KINDS = frozenset(
    {
        "ingest",
        "copy",
        "copy_stdin",
        "copy_socket",
        "wrap_existing",
        "ingest_s3_skipprlake",
    }
)


def parse_scalar(raw: str) -> Any:
    text = raw.strip()
    if not text or text == "~" or text == "null":
        return None
    if text in {"true", "yes", "True"}:
        return True
    if text in {"false", "no", "False"}:
        return False
    if text.startswith("[") and text.endswith("]"):
        inner = text[1:-1].strip()
        if not inner:
            return []
        return [parse_scalar(part.strip()) for part in inner.split(",")]
    if (text.startswith('"') and text.endswith('"')) or (
        text.startswith("'") and text.endswith("'")
    ):
        return text[1:-1]
    try:
        return int(text)
    except ValueError:
        pass
    return text


def parse_simple_yaml(text: str) -> Any:
    rows: list[tuple[int, str]] = []
    for raw in text.splitlines():
        stripped = raw.split("#", 1)[0].rstrip()
        if not stripped.strip():
            continue
        indent = len(stripped) - len(stripped.lstrip(" "))
        rows.append((indent, stripped.strip()))
    value, _ = _parse_block(rows, 0, 0)
    return value


def _parse_block(rows: list[tuple[int, str]], index: int, indent: int) -> tuple[Any, int]:
    if index >= len(rows):
        return {}, index
    _, first = rows[index]
    if first.startswith("- "):
        items: list[Any] = []
        while index < len(rows) and rows[index][0] == indent and rows[index][1].startswith("- "):
            item_text = rows[index][1][2:].strip()
            index += 1
            if item_text:
                items.append(parse_scalar(item_text))
            elif index < len(rows) and rows[index][0] > indent:
                nested, index = _parse_block(rows, index, rows[index][0])
                items.append(nested)
        return items, index
    mapping: dict[str, Any] = {}
    while index < len(rows) and rows[index][0] == indent:
        _, content = rows[index]
        if ":" not in content:
            raise ValueError(f"expected key: value, got {content!r}")
        key, rest = content.split(":", 1)
        key = key.strip()
        rest = rest.strip()
        index += 1
        if rest:
            mapping[key] = parse_scalar(rest)
            continue
        if index < len(rows) and rows[index][0] > indent:
            nested, index = _parse_block(rows, index, rows[index][0])
            mapping[key] = nested
        else:
            mapping[key] = {}
    return mapping, index


def load_catalog(path: Path | None = None) -> dict[str, Any]:
    catalog_path = path or CATALOG_PATH
    parsed = parse_simple_yaml(catalog_path.read_text(encoding="utf-8"))
    if not isinstance(parsed, dict) or "scenarios" not in parsed:
        raise ValueError(f"{catalog_path} missing scenarios")
    for name, spec in parsed["scenarios"].items():
        if not isinstance(spec, dict):
            raise ValueError(f"{name}: scenario must be a mapping")
        scenario_kind(spec)
        validate_scenario_asserts(name, spec)
    return parsed


def validate_scenario_asserts(name: str, scenario: dict[str, Any]) -> None:
    for key in scenario.get("asserts") or {}:
        if key in KNOWN_ASSERT_KEYS:
            continue
        if key.startswith("min_") and key.endswith("_events"):
            continue
        raise ValueError(f"{name}: unknown catalog assert {key!r}")


def scenario_ids(catalog: dict[str, Any] | None = None) -> list[str]:
    data = catalog or load_catalog()
    return list(data["scenarios"].keys())


def scenario_kind(scenario: dict[str, Any]) -> str:
    kind = str(scenario.get("kind") or "")
    if kind not in SCENARIO_KINDS:
        raise ValueError(f"unknown scenario kind {kind!r}; expected one of {sorted(SCENARIO_KINDS)}")
    return kind


def required_perf_for(catalog: dict[str, Any], scenario: dict[str, Any]) -> list[str]:
    defaults = catalog.get("defaults") or {}
    if "required_perf" in scenario:
        phases = list(scenario.get("required_perf") or [])
    else:
        phases = list(defaults.get("required_perf") or [])
    if not phases:
        raise ValueError("required_perf must not be empty")
    if "sync" not in phases:
        raise ValueError(f"required_perf must include sync: {phases}")
    return phases
