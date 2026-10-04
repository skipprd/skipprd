# query

Run engine SQL against Iceberg tables skipprd wrote (SkipprLake, AthenaIceberg,
Duckdb). The **lake contract** is Iceberg `namespace.table` (`bronze.shop`).
`pipeline.namespace` (`shop.shop`) is the local Iceberg ∪ WAL view of ingest
on the sync host. Manage pipelines and schemas, and stream from the WAL.

Hive Athena, Snowflake, and other non-Iceberg sinks are live WAL only. Athena
SQL and DuckDB `iceberg_scan` remain available for those warehouses' own
clients. Use `sde query` for modeled warehouse SQL.

## Usage

```bash
skipprd --config skippr.yml query --sql "<SQL>" [--watch <seconds>] [--plain] [--log [LEVEL]]
```

## Flags

| Flag | Required | Description |
|---|---|---|
| `--sql, -s` | No | The SQL statement to execute. If omitted, opens interactive mode. |
| `--watch` | No | Re-run the query every N seconds (live refresh). |
| `--plain` | No | Print one JSON document `{ "header": [...], "rows": [[...]] }` to stdout instead of the TUI. |
| `--log` | No | Enable logging. |

## Examples

Query a table:

```bash
skipprd query --sql "SELECT COUNT(*) FROM bikehire"
```

Live watch:

```bash
skipprd query --sql "SELECT COUNT(*) FROM bikehire" --watch 5
```

Pipeline management:

```bash
skipprd query --sql "ENABLE PIPELINE bikehire"
skipprd query --sql "DISABLE PIPELINE bikehire"
skipprd query --sql "RESET PIPELINE bikehire"
skipprd query --sql "DROP PIPELINE bikehire"
```

Schema management:

```bash
skipprd query --sql "SCHEMA DUMP bikehire TO 'schema.json'"
skipprd query --sql "LOAD SCHEMA 'schema.json' INTO bikehire"
skipprd query --sql "ALTER SCHEMA bikehire DROP COLUMN old_field"
skipprd query --sql "ALTER SCHEMA bikehire ALTER COLUMN price TYPE DECIMAL(10,2)"
```

Stream from the WAL:

```bash
skipprd --config skippr.yml query --sql "STREAM * FROM bikehire LIMIT 100"
```

See [`skipprd sql-help`](sql-help.md) or `SHOW DOCS` for every supported
statement. `skipprd sql-help --output sql-docs.md` regenerates the checked-in
reference at the repository root.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Pipeline not found (for `ENABLE PIPELINE`), or query error |
