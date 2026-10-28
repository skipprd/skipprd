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


def migrate_stream_record(index: int) -> dict[str, Any]:
    """One shape for every in-stream ALTER cell so migrate types compare."""
    return {
        "id": index,
        "name": f"user_{index}",
        "amount": index * 10,
        "amount_string": str(index * 10),
        "note": f"note_{index}",
        "region": "eu",
        "detail": {"truck_reg": f"T{index}", "geofence_id": "g1"},
        "detail_record": {"geofence_id": "g2"},
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


def _factory(kind: str):
    factories = {
        "exact_arrow": exact_arrow_record,
        "exact_arrow_ts": exact_arrow_ts_record,
        "nested_flatten": nested_flatten_record,
        "migrate_stream": migrate_stream_record,
    }
    factory = factories.get(kind)
    if factory is None:
        raise ValueError(f"unknown fixture kind {kind!r}")
    return factory


def write_ndjson(path: Path, kind: str, rows: int) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    factory = _factory(kind)
    with path.open("w", encoding="utf-8") as handle:
        for index in range(1, rows + 1):
            handle.write(json.dumps(factory(index), separators=(",", ":")) + "\n")
    return path


def write_csv(path: Path, kind: str, rows: int) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    factory = _factory(kind)
    with path.open("w", encoding="utf-8") as handle:
        handle.write("id,name,amount\n")
        for index in range(1, rows + 1):
            record = factory(index)
            handle.write(f"{record['id']},{record['name']},{record['amount']}\n")
    return path


def write_json_array(path: Path, kind: str, rows: int) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    factory = _factory(kind)
    payload = [factory(index) for index in range(1, rows + 1)]
    path.write_text(json.dumps(payload, separators=(",", ":")), encoding="utf-8")
    return path


def write_insert_sql(path: Path, rows: int) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    lines = [
        "CREATE TABLE events (id BIGINT, name VARCHAR, amount BIGINT);",
    ]
    for index in range(1, rows + 1):
        record = exact_arrow_record(index)
        name = str(record["name"]).replace("'", "''")
        lines.append(
            f"INSERT INTO events VALUES ({record['id']}, '{name}', {record['amount']});"
        )
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return path


def write_records(path: Path, kind: str, rows: int, *, fmt: str) -> Path:
    if fmt in {"json", "ndjson"}:
        return write_ndjson(path, kind, rows)
    if fmt == "csv":
        return write_csv(path, kind, rows)
    if fmt == "json_array":
        return write_json_array(path, kind, rows)
    raise ValueError(f"unknown fixture format {fmt!r}")
