"""Permutation SoT for skippr-perf and skippr-perf-db transform / migrate cells.

Historical ingest is File `sync --once`. Stream ingest is Stdin (SQL filter/derive
and CREATE TABLE) or a compaction hold (migrate + query). Copy pipelines cannot
set transform.compute.
"""

from __future__ import annotations

OPS = ("filter", "derive", "compose")
# Iceberg dest schema is discovered from source columns. Derive/compose add
# columns, so those cells write File parquet so dest_has matches the folded batch.
SCHEMA_CHANGING_OPS = ("derive", "compose")
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


SQL_YAML = {
    "filter": (
        "        - name: positive_amount\n"
        "          type: sql\n"
        "          value: |\n"
        "            SELECT * FROM perf_cell.perf_cell WHERE amount > 50\n"
    ),
    "derive": (
        "        - name: amount_gbp\n"
        "          type: sql\n"
        "          value: |\n"
        "            SELECT *, amount * 0.8 AS amount_gbp FROM perf_cell.perf_cell\n"
    ),
    "compose": (
        "        - name: amount_gbp_positive\n"
        "          type: sql\n"
        "          value: |\n"
        "            SELECT *, amount * 0.8 AS amount_gbp FROM perf_cell.perf_cell WHERE amount > 0\n"
    ),
}

COMPUTE_YAML = {
    "sum": (
        "        - name: perf_cell.name_totals\n"
        "          type: sql\n"
        "          value: |\n"
        "            CREATE TABLE perf_cell.name_totals AS\n"
        "            SELECT name, SUM(amount) AS amount_total FROM perf_cell.perf_cell GROUP BY name\n"
    ),
    "count": (
        "        - name: perf_cell.name_totals\n"
        "          type: sql\n"
        "          value: |\n"
        "            CREATE TABLE perf_cell.name_totals AS\n"
        "            SELECT name, COUNT(*) AS row_count FROM perf_cell.perf_cell GROUP BY name\n"
    ),
    "both": (
        "        - name: perf_cell.name_totals\n"
        "          type: sql\n"
        "          value: |\n"
        "            CREATE TABLE perf_cell.name_totals AS\n"
        "            SELECT name, SUM(amount) AS amount_total, COUNT(*) AS row_count\n"
        "            FROM perf_cell.perf_cell GROUP BY name\n"
    ),
}

ALTER_SQL = {
    "rename": "ALTER TABLE perf_cell.perf_cell RENAME COLUMN name TO display_name",
    "merge": "ALTER TABLE perf_cell.perf_cell MERGE COLUMN amount_string INTO amount",
    "drop": "ALTER TABLE perf_cell.perf_cell DROP COLUMN name",
    "promote": "ALTER TABLE perf_cell.perf_cell ALTER COLUMN id TYPE BIGINT",
}
