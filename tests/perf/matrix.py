"""Permutation SoT for skippr-perf and skippr-perf-db transform / migrate / CDC cells.

Historical ingest is File `sync --once`. Stream ingest is Stdin (SQL filter/derive
and CREATE TABLE) or a compaction hold (migrate + query). Copy pipelines cannot
set transform.compute. CDC query cells are SnapshotThenLogExactOnce for every
GitHub-local source (Postgres, MySQL, DynamoDB). File and CDC migrate cells
ALTER already-ingested data, ingest more, then query.
"""

from __future__ import annotations

OPS = ("filter", "derive", "compose")
# Iceberg dest schema is discovered from source columns. Derive/compose add
# columns, so those cells write File parquet so dest_has matches the folded batch.
SCHEMA_CHANGING_OPS = ("derive", "compose")
INGEST_MODES = ("historical", "stream")
AGG_MEASURES = ("sum", "count", "both")
ALTERS = ("rename", "merge", "drop", "promote")
CDC_SOURCES = ("pg", "mysql", "ddb")
CDC_CASES = ("count", "update", "delete", "reinsert", "sum", "overlap", "iceberg-only")
# MERGE needs amount_string; CDC orders tables do not have that column.
CDC_ALTERS = ("rename", "drop", "promote")

CDC_PIPELINES = {
    "pg": "postgres_skipprlake_cdc",
    "mysql": "mysql_skipprlake_cdc",
    "ddb": "dynamodb_skipprlake_cdc",
}
CDC_TABLES = {
    "pg": "postgres_skipprlake_cdc.postgres_orders",
    "mysql": "mysql_skipprlake_cdc.orders",
    "ddb": "dynamodb_skipprlake_cdc.skippr_dynamo_cdc",
}
CDC_CASE_SQL = {
    "count": "INSERT INTO orders (id, name, amount) VALUES (3, 'gamma', 30);",
    "update": "UPDATE orders SET name = 'alpha-prime' WHERE id = 1;",
    "delete": "DELETE FROM orders WHERE id = 2;",
    "reinsert": (
        "DELETE FROM orders WHERE id = 2; "
        "INSERT INTO orders (id, name, amount) VALUES (2, 'beta-new', 50);"
    ),
    "sum": (
        "INSERT INTO orders (id, name, amount) VALUES (3, 'gamma', 30); "
        "UPDATE orders SET amount = 120 WHERE id = 1;"
    ),
    "overlap": "UPDATE orders SET name = 'alpha-wal' WHERE id = 1;",
    "iceberg-only": "INSERT INTO orders (id, name, amount) VALUES (3, 'gamma', 30);",
}

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


def query_cdc_id(case: str, source: str) -> str:
    if case == "overlap":
        return f"q-cdc-overlap-{source}"
    if case == "iceberg-only":
        return f"q-cdc-iceberg-only-{source}"
    return f"q-cdc-union-{case}-{source}"


def query_cdc_migrate_id(alter: str, source: str) -> str:
    return f"q-cdc-migrate-{alter}-{source}"


def alter_sql(alter: str, table: str = "perf_cell.perf_cell") -> str:
    templates = {
        "rename": "ALTER TABLE {table} RENAME COLUMN name TO display_name",
        "merge": "ALTER TABLE {table} MERGE COLUMN amount_string INTO amount",
        "drop": "ALTER TABLE {table} DROP COLUMN name",
        "promote": "ALTER TABLE {table} ALTER COLUMN id TYPE BIGINT",
    }
    if alter not in templates:
        raise ValueError(f"unknown alter {alter!r}")
    return templates[alter].format(table=table)


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

ALTER_SQL = {alter: alter_sql(alter) for alter in ALTERS}
