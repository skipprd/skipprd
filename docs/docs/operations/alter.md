---
title: ALTER operations
description: Disable a pipeline, then rename, merge, drop, or widen columns on Iceberg tables without rewriting history.
---

# ALTER operations

After a pipeline has landed data, you sometimes need to change the table: fold a sibling column back, move a field into a nested record, drop a leftover, or widen a type. `ALTER TABLE` does that on Iceberg destinations. Skipprd keeps the Iceberg field id, so existing files stay readable.

This is operator DDL. It is not the same as [schema discovery](/concepts/schema), which adds columns when new fields arrive during sync.

## Before you alter

1. The destination must be Iceberg: [SkipprLake](/connectors/outputs/skipprlake), [Athena Iceberg](/connectors/outputs/athenaiceberg), or [DuckDB](/connectors/outputs/duckdb). WAL-only pipelines fail with `ALTER TABLE requires an Iceberg sink`.
2. Disable the pipeline first. Every `ALTER TABLE` fails if the pipeline is still enabled.
3. Target `<pipeline>.<namespace>`. Quote names that contain hyphens.

```bash
skipprd query --sql "DISABLE PIPELINE bikehire"
skipprd query --sql "ALTER TABLE bikehire.rides RENAME COLUMN region TO detail.region"
skipprd query --sql "ENABLE PIPELINE bikehire"
```

Hyphenated pipeline or namespace:

```bash
skipprd query --sql "ALTER TABLE \"bike-hire\".rides DROP COLUMN legacy_field"
```

Then run `skipprd sync` as usual. Later rows use the new names and types.

## What you can change

| Statement | What it does |
|---|---|
| `RENAME COLUMN <from> TO <to>` | Renames a field. A dotted `TO` path moves it into that record. Iceberg flatten names use underscores (`detail.region` → `detail_region`). |
| `MERGE COLUMN <src> INTO <dst>` | Drops the sibling and retargets type-conflict evolution so later values write to `dst`. Historical src-only rows are not copied. |
| `DROP COLUMN <col>` | Removes the column from skippr metadata and the Iceberg table. |
| `ALTER COLUMN <col> TYPE <type>` | Widens a type. Illegal promotions fail closed. |

Flattened Iceberg names (`detail_truck_reg`) and skippr dotted paths (`detail.truck_reg`) both resolve.

### Rename, including nested moves

```bash
skipprd query --sql "ALTER TABLE bikehire.rides RENAME COLUMN truck_reg TO detail.truck_reg"
skipprd query --sql "ALTER TABLE bikehire.rides RENAME COLUMN region TO detail.region"
```

`RENAME COLUMN region TO detail.region` is valid even when `detail.region` did not exist. Skipprd creates the parent record and the Iceberg column `detail_region`. Existing files keep the old field id.

### Merge a sibling column

When a source starts sending a different type, Skipprd adds a sibling such as `price_string` next to `price`. Fold it back:

```bash
skipprd query --sql "ALTER TABLE bikehire.rides MERGE COLUMN price_string INTO price"
```

Later values that would have gone to `price_string` write to `price`. Rows that only ever had `price_string` are not copied into `price`.

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

```bash
skipprd query --sql "ALTER TABLE bikehire.rides ALTER COLUMN n TYPE BIGINT"
skipprd query --sql "ALTER TABLE bikehire.rides ALTER COLUMN n_byte TYPE BIGINT"
```

Iceberg timestamps are already microseconds. Promoting milli → timestamp updates skippr metadata only.

To widen to a string, `MERGE COLUMN` the typed sibling into a string field. There is no `ALTER COLUMN … TYPE STRING`.

## What fails closed

Skipprd rejects the statement and leaves the table unchanged:

- The pipeline is still enabled.
- The destination is not Iceberg.
- The column does not exist, or the rename target already exists.
- The promotion is not in the table above (`cannot promote decimal to double`).
- Nested `DROP` or `MERGE` of a record that has no top-level Iceberg field.
- A dotted `RENAME` target whose parent path is not a record.

Fix the statement, or pick a legal widen / merge, then retry. You do not need to restore files.

## Check the result

```bash
skipprd schema --pipeline bikehire
skipprd query --plain --sql "SELECT * FROM bikehire.rides LIMIT 5"
```

`skipprd schema` shows the names and types after the alter. Query the table to confirm nested moves landed as `detail_region` (or the dotted path your warehouse exposes).

Run [`skipprd sql-help`](/cli/sql-help) or `skipprd query --sql "SHOW DOCS"` for the exact syntax of every statement.

## Next steps

- [Schema discovery and evolution](/concepts/schema)
- [SkipprLake](/connectors/outputs/skipprlake)
- [skipprd query](/cli/query)
- [Troubleshooting](/operations/troubleshooting)
