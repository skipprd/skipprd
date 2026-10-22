"""Consistent-schema NDJSON for exact-Arrow, plus nested JSON for the slow path."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any


def exact_arrow_record(index: int) -> dict[str, Any]:
    return {"id": index, "name": f"user_{index}", "amount": index * 10}


def exact_arrow_ts_record(index: int) -> dict[str, Any]:
    record = exact_arrow_record(index)
    record["occurred_at"] = "2024-01-15T12:34:56Z"
    return record


def nested_flatten_record(index: int) -> dict[str, Any]:
    return {
        "id": index,
        "user": {"name": f"user_{index}"},
        "amount": index * 10,
    }


def is_exact_arrow_eligible_record(record: dict[str, Any], flatten: bool) -> bool:
    if flatten:
        return False
    for value in record.values():
        if isinstance(value, (dict, list)):
            return False
    return True


def expected_amount_sum(rows: int) -> int:
    return 10 * rows * (rows + 1) // 2


def write_ndjson(path: Path, kind: str, rows: int) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    factories = {
        "exact_arrow": exact_arrow_record,
        "exact_arrow_ts": exact_arrow_ts_record,
        "nested_flatten": nested_flatten_record,
    }
    factory = factories.get(kind)
    if factory is None:
        raise ValueError(f"unknown fixture kind {kind!r}")
    with path.open("w", encoding="utf-8") as handle:
        for index in range(1, rows + 1):
            handle.write(json.dumps(factory(index), separators=(",", ":")) + "\n")
    return path
