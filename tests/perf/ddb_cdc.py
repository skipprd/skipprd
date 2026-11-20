"""DynamoDB CDC harness helpers: snapshot visibility before stream mutations."""

from __future__ import annotations

import subprocess
import time
from typing import Any, Callable

DDB_SEED_SNAPSHOT_ROW_COUNT = 2
DDB_SNAPSHOT_COUNT_SQL = (
    "SELECT count(*) AS n FROM dynamodb_skipprlake_cdc.dynamodb_skippr_dynamo_cdc"
)
ICEBERG_SNAPSHOT_COUNT_SQL = "SELECT count(*) AS n FROM iceberg_scan('{scan}')"


def snapshot_count_from_doc(doc: dict[str, Any]) -> int | None:
    rows = doc.get("rows") or []
    if not rows or not rows[0]:
        return None
    try:
        return int(float(rows[0][0]))
    except (TypeError, ValueError):
        return None


def ddb_snapshot_rows_ready(count: int = DDB_SEED_SNAPSHOT_ROW_COUNT) -> Callable[[dict[str, Any]], bool]:
    def ready(doc: dict[str, Any]) -> bool:
        got = snapshot_count_from_doc(doc)
        return got is not None and got >= count

    return ready


def wait_iceberg_snapshot_row_count(
    iceberg_scan: Callable[[str], str],
    count_sql: str,
    *,
    expected: int = DDB_SEED_SNAPSHOT_ROW_COUNT,
    deadline: float,
    fail: Callable[[str], None],
) -> None:
    """Poll DuckDB Iceberg scan until bronze has at least `expected` rows."""
    while time.time() < deadline:
        try:
            out = iceberg_scan(count_sql)
            lines = [ln.strip() for ln in out.splitlines() if ln.strip()]
            if lines:
                body = lines[-1].split(",")[-1] if "," in lines[-1] else lines[-1]
                if int(body) >= expected:
                    return
        except (subprocess.CalledProcessError, ValueError, IndexError):
            pass
        time.sleep(0.25)
    fail(f"expected at least {expected} snapshot rows in Iceberg before stream mutations")

