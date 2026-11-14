"""Permutation SoT for skippr-perf and skippr-perf-db transform / migrate cells.

Historical ingest is File `sync --once`. Stream ingest is Stdin (ops/aggregates)
or a compaction hold (migrate + query). Copy pipelines cannot set ops or aggregates.
"""

from __future__ import annotations

OPS = ("filter", "project", "derive", "rename", "cast", "compose")
# Iceberg dest schema is discovered from source columns. Ops that add, drop,
# rename, or recast fields write File parquet so dest_has matches the folded batch.
SCHEMA_CHANGING_OPS = ("project", "derive", "rename", "cast", "compose")
INGEST_MODES = ("historical", "stream")
AGG_MEASURES = ("sum", "count", "both")
ALTERS = ("rename", "merge", "drop", "promote")

FILTER_AMOUNT_GT = 50


def ingest_ops_id(op: str, mode: str) -> str:
    if op == "compose" and mode == "historical":
        return "ingest-ops-duckdb"
    if mode == "historical":
        return f"ingest-ops-{op}-duckdb"
    return f"ingest-ops-{op}-stream-duckdb"


def ingest_agg_id(measure: str, mode: str) -> str:
    if measure == "both" and mode == "historical":
        return "ingest-aggregates-duckdb"
    if mode == "historical":
        return f"ingest-aggregates-{measure}-duckdb"
    if measure == "both":
        return "ingest-aggregates-stream-duckdb"
    return f"ingest-aggregates-{measure}-stream-duckdb"


def ingest_migrate_id(alter: str, mode: str) -> str:
    if mode == "historical":
        return f"ingest-migrate-{alter}-duckdb"
    return f"ingest-migrate-{alter}-stream-duckdb"


def query_ops_id(mode: str) -> str:
    if mode == "historical":
        return "q-ops-count-duckdb"
    return "q-ops-stream-iceberg"


def query_agg_id(mode: str) -> str:
    if mode == "historical":
        return "q-aggregates-count-duckdb"
    return "q-aggregates-stream-iceberg"


def query_migrate_id(alter: str, mode: str) -> str:
    if mode == "historical":
        return f"q-migrate-{alter}-duckdb"
    return f"q-migrate-{alter}-stream-iceberg"


def expected_filter_rows(rows: int) -> int:
    # amount = index * 10 for index 1..rows; keep amount > 50 ⇒ index >= 6.
    return max(rows - 5, 0)


def expected_amount_sum_range(start_index: int, rows: int) -> int:
    if rows < start_index:
        return 0
    n = rows - start_index + 1
    return 10 * n * (start_index + rows) // 2


OPS_YAML = {
    "filter": '        - filter: "amount > 50"\n',
    "project": "        - project: [id, amount]\n",
    "derive": '        - derive:\n            amount_gbp: "amount * 0.8"\n',
    "rename": "        - rename:\n            name: display_name\n",
    "cast": "        - cast:\n            amount: float64\n",
    "compose": (
        '        - filter: "amount > 0"\n'
        '        - derive:\n            amount_gbp: "amount * 0.8"\n'
        "        - rename:\n            name: display_name\n"
        "        - cast:\n            amount: float64\n"
        "        - project: [id, display_name, amount, amount_gbp]\n"
    ),
}

AGG_YAML = {
    "sum": (
        "        - id: name_totals\n"
        "          key_fields: [name]\n"
        "          measures:\n"
        "            amount_total: { sum: amount }\n"
    ),
    "count": (
        "        - id: name_totals\n"
        "          key_fields: [name]\n"
        "          measures:\n"
        '            row_count: { count: "*" }\n'
    ),
    "both": (
        "        - id: name_totals\n"
        "          key_fields: [name]\n"
        "          measures:\n"
        "            amount_total: { sum: amount }\n"
        '            row_count: { count: "*" }\n'
    ),
}

ALTER_SQL = {
    "rename": "ALTER TABLE perf_cell.perf_cell RENAME COLUMN name TO display_name",
    "merge": "ALTER TABLE perf_cell.perf_cell MERGE COLUMN amount_string INTO amount",
    "drop": "ALTER TABLE perf_cell.perf_cell DROP COLUMN name",
    "promote": "ALTER TABLE perf_cell.perf_cell ALTER COLUMN id TYPE BIGINT",
}
