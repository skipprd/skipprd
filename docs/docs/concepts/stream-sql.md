---
title: Stream SQL
description: Run SELECT and STREAM against Iceberg plus the live WAL, watch a table fill, and serve committed tables over Flight SQL.
---

# Stream SQL

Skipprd is a Streamhouse engine: ingest writes a write-ahead log (WAL), compact lands Apache Iceberg on your object store, and SQL reads both. You do not wait for a warehouse load to ask what just arrived.

[`skipprd query`](/cli/query) is the SQL client on the machine that holds `DATA_DIR`. [`skipprd serve`](/cli/serve) is for other tools that should see committed Iceberg only.

## What a table is

Each namespace is `<pipeline>.<namespace>`, for example `bikehire.rides`. What you see depends on the destination:

| Destination | `skipprd query` |
|---|---|
| SkipprLake, Athena Iceberg, DuckDB | Committed Iceberg files **union** live WAL slices that have not compacted yet |
| No destination | The WAL is the dataset |
| Other warehouses (Snowflake, Postgres, Athena Hive, …) | Live WAL only. Query the warehouse itself for landed history |

Iceberg ∪ WAL is one SELECT. Compaction does not create a second table. Rows you ingested a second ago and rows already in Parquet sit in the same name.

## SELECT

```bash
skipprd query --plain --sql "SELECT count(*) AS n FROM bikehire.rides"
skipprd query --sql "SELECT station, count(*) AS rides FROM bikehire.rides GROUP BY station ORDER BY rides DESC LIMIT 10"
```

Python is the same engine:

```python
import skippr

session = skippr.Session(skippr.Config.discover().get_pipeline("bikehire"))
table = session.query("SELECT count(*) AS n FROM bikehire.rides")
print(table.to_pylist())
```

`--plain` prints one JSON document for scripts. Without `--sql`, `query` opens an interactive `sql>` prompt.

`--warmup` opens catalogs and runs the statement once, then times the second collect. Use it when you care about query time rather than process start.

## STREAM

`STREAM` reads the WAL as ingest is writing it and keeps returning new rows:

```bash
skipprd query --sql "STREAM * FROM bikehire.rides LIMIT 100"
```

`SELECT` is a snapshot (Iceberg plus the WAL slices frozen for that statement). `STREAM` follows the tail. `LIMIT` stops the stream after that many rows.

`--watch <seconds>` re-runs a `SELECT` in the interactive table view. It is a poll, not `STREAM`.

## How Skippr Store supports the query

Skippr Store is the local KV next to the WAL. Payload bytes stay in `.seg` files or S3. The store is the catalog of which slices are live.

1. Ingest holds the **writer** for that `DATA_DIR` (`writer.lock`). One process writes catalog, offsets, schema, journal, and stream state.
2. `skipprd query` **pins** a Skippr Store snapshot (`query_pin`): which WAL slices are visible, and which Iceberg snapshot they sit beside.
3. A second **reader** process can query while ingest is still writing. It pins a flushed copy of the store. It does not take the writer lock and it does not commit.
4. After compact, `query_pin` reports `wal_pin: drained` when the SELECT was lake-only, or `wal_pin: live` when the statement still held WAL (STREAM, overlap).

If you delete `DATA_DIR`, the pin is gone. Iceberg files in your bucket remain; the live WAL does not.

See [Skippr Store](/configuration/skippr-store).

## Transforms and aggregates on the query path

In-memory `transform.ops` (`filter`, `project`, `derive`, `rename`, `cast`) run **before** WAL commit. SQL sees the transformed rows, not the raw source batch.

Keyed `transform.aggregates` are materialised in the Skippr Store `stream_state` family as ingest commits WAL. They are not a second SQL table name in this engine. You still `SELECT` the pipeline namespace. See [Materialised views](/concepts/materialised-views) and [Transforms](/configuration/transforms).

DDL (`ALTER TABLE`, `skipprd migrate`) rewrites skippr metadata, live WAL, and the destination in-stream. Later SELECTs use the new names. See [ALTER operations](/operations/alter).

## Other tools

`skipprd serve` exposes Iceberg REST and read-only Flight SQL. Flight reads **committed Iceberg**. For live WAL, stay on `skipprd query`.

Grammar for every statement: [`skipprd sql-help`](/cli/sql-help) or `skipprd query --sql "SHOW DOCS"`.

## Next steps

- [skipprd query](/cli/query)
- [skipprd serve](/cli/serve)
- [Skippr Store](/configuration/skippr-store)
- [Materialised views](/concepts/materialised-views)
- [Streamhouse](/concepts/datalake)
