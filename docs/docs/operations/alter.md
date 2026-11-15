---
title: ALTER operations
description: Rename, merge, drop, or widen columns with skipprd migrate or ALTER TABLE without stopping the pipeline.
---

# ALTER operations

After a pipeline has landed data, you sometimes need to change the table: fold a sibling column back, drop a leftover, or widen a type. `ALTER TABLE` (and `skipprd migrate`) does that on Iceberg destinations and warehouses that advertise table DDL. Skipprd keeps the Iceberg field id, so existing files stay readable.

This is operator DDL. It is not the same as [schema discovery](/concepts/schema), which adds columns when new fields arrive during sync. Apply is in-stream: skipprd may pause ingest internally; you do not stop and start the pipeline. `DISABLE PIPELINE` still freezes ingest, but it is not the ALTER gate.

## Before you alter

1. The destination must be a table: Iceberg ([SkipprLake](/connectors/outputs/skipprlake), [Athena Iceberg](/connectors/outputs/athenaiceberg), [DuckDB](/connectors/outputs/duckdb)) plus Snowflake, Postgres, BigQuery, Redshift, and MotherDuck. Glue/Hive and ClickHouse fail closed. Stream destinations cannot run `ALTER TABLE`.
2. Put SQL files in `migrations/` next to `skippr.yml`, or run `skipprd query --sql "ALTER TABLE …"`. Both use the same apply journal.
3. Target `<pipeline>.<namespace>`. Quote names that contain hyphens.

```bash
skipprd migrate --pipeline bikehire
skipprd query --sql "ALTER TABLE bikehire.rides RENAME COLUMN region TO region_code"
```

Hyphenated pipeline or namespace:

```bash
skipprd query --sql "ALTER TABLE \"bike-hire\".rides DROP COLUMN legacy_field"
```

Then run `skipprd sync` as usual. Later rows use the new names and types.

## What you can change

| Statement | What it does |
|---|---|
| `RENAME COLUMN <from> TO <to>` | Renames a field. `TO` is one identifier; the column keeps its parent. Iceberg flatten names use underscores (`detail.region` → `detail_region`). |
| `MERGE COLUMN <src> INTO <dst>` | Rewrites historical rows with `COALESCE(dst, CAST(src AS dst_type))`, then drops `src`. CAST must be Iceberg-legal. |
| `DROP COLUMN <col>` | Removes the column from the published table. Discover metadata stays; the alter log is the output shape. |
| `ALTER COLUMN <col> TYPE <type>` | Widens a type. Illegal promotions fail closed. |

SQL must match the pipeline output names. Nested pipelines use dotted paths (`detail.region`). Flattened pipelines use underscores (`detail_region`). Do not mix the two forms.

### Rename

```bash
skipprd query --sql "ALTER TABLE bikehire.rides RENAME COLUMN truck_reg TO vehicle_reg"
skipprd query --sql "ALTER TABLE bikehire.rides RENAME COLUMN region TO region_code"
```

`RENAME COLUMN region TO detail.region` fails closed: `TO` is not a nested move. Nested `FROM` paths still rename a leaf in place (`detail.truck_reg TO vehicle_reg` stays under `detail`). Existing files keep the old field id.

### Merge a sibling column

When a source adds a same-type sibling, fold it back:

```bash
skipprd query --sql "ALTER TABLE bikehire.rides MERGE COLUMN price_cents INTO price"
```

Historical rows are rewritten so `price` keeps its value, or the cast `price_cents` when `price` was null. Later source values that used the `price_cents` key write to `price`. String into long fails closed. To keep the value as text, MERGE the typed sibling into a string column.

### Drop a column

```bash
skipprd query --sql "ALTER TABLE bikehire.rides DROP COLUMN legacy_field"
```

### Widen a type

Only Iceberg-legal promotions:

| From | To |
|---|---|
| byte, short, integer | `BIGINT` / long |
| byte, short | `INTEGER` |
| float | `DOUBLE` |
| timestamp_milli | `TIMESTAMP` |
| date | `TIMESTAMP` |

```bash
skipprd query --sql "ALTER TABLE bikehire.rides ALTER COLUMN n TYPE BIGINT"
skipprd query --sql "ALTER TABLE bikehire.rides ALTER COLUMN n_byte TYPE BIGINT"
```

Iceberg timestamps are already microseconds. Promoting milli → timestamp records the output type in the ALTER log; Iceberg data files do not change. Date → timestamp rewrites data files.

To widen to a string, `MERGE COLUMN` the typed sibling into a string field. There is no `ALTER COLUMN … TYPE STRING`.

## What fails closed

Skipprd rejects the statement and leaves the table unchanged:

- The destination is Glue/Hive or ClickHouse (no table ALTER), or a stream.
- The column does not exist, or the rename target already exists.
- The promotion is not in the table above (`cannot promote decimal to double`).
- Nested `DROP` or `MERGE` of a record that has no top-level Iceberg field.
- A dotted `RENAME` `TO` (`RENAME COLUMN region TO detail.region`).

Fix the statement, or pick a legal widen / merge, then retry. You do not need to restore files.

## Check the result

```bash
skipprd query --sql "SHOW PIPELINE bikehire"
skipprd query --plain --sql "SELECT * FROM bikehire.rides LIMIT 5"
```

`SHOW PIPELINE` prints the output names after the ALTER log. Query the table to confirm the new names.

Run [`skipprd sql-help`](/cli/sql-help) or `skipprd query --sql "SHOW DOCS"` for the exact syntax of every statement.

Table destinations (SkipprLake, Athena Iceberg, DuckDB Iceberg, Snowflake, Postgres, BigQuery, Redshift, MotherDuck) change published columns with these statements, not with YAML `transform.ops` `project` / `rename` / `cast`. Stream destinations keep those YAML ops; they cannot run `ALTER TABLE`.

## Next steps

- [`skipprd migrate`](/cli/migrate)
- [Python](/python)
- [Skippr Store](/configuration/skippr-store)
- [Schema discovery and evolution](/concepts/schema)
- [SkipprLake](/connectors/outputs/skipprlake)
- [skipprd query](/cli/query)
- [Troubleshooting](/operations/troubleshooting)
