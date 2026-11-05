---
title: Streamhouse
description: An open Streamhouse architecture. Land Skipprd pipelines as open Iceberg tables in object storage with zero vendor lock-in. Query live stream plus history together in Skippr. Other engines can read the same Iceberg tables.
---

# Streamhouse

A [Streamhouse](https://www.streamhouse.com/) continuously captures, transforms, governs, and serves the live operational state of your business on open files you own, rather than locking rows into periodic batch extracts or proprietary vendor warehouses.

Skipprd is an open Streamhouse engine. It lands pipelines as [Apache Iceberg](https://iceberg.apache.org/) tables on Parquet in your object storage.

## Open architecture. Zero vendor lock-in.

Because storage is open Apache Iceberg and Parquet in a bucket you control, **your data is never trapped**. Compute is decoupled from storage:

1. **Query live plus history in Skippr:** [`skipprd query`](/cli/query) runs one SQL statement against the committed Iceberg table combined with in-flight rows that just arrived. You do not pick a live view or a history view. Readers never see half-written tables because Iceberg snapshot commits are atomic. `STREAM` is that same SQL, tailed to stdout as new rows arrive.
2. **Same Iceberg tables anywhere:** Athena, Snowflake, DuckDB, Databricks, PyIceberg, and local notebooks can query the exact same Iceberg tables in your bucket. There is no second ETL copy and no proprietary format to migrate out of. Those engines see committed history, not the live WAL.

## How a pipeline lands in the Streamhouse

1. Skipprd ingests events from sources (databases, streams, APIs, files) and commits each batch durably.
2. It writes the batch into standard Parquet data files and Iceberg positional delete files in your bucket or storage path.
3. The catalog (AWS Glue, SkipprLake, or a local DuckDB catalog) points atomically to the new Iceberg snapshot.

You query tables by name (`namespace.table`). You do not have to manage raw Parquet files by hand.

## Destinations that write Iceberg

| Destination | Catalog | Typical use |
|---|---|---|
| [SkipprLake](/connectors/outputs/skipprlake) | A table you name (`catalog_table`) | Skippr-managed Streamhouse, query with `skipprd query` |
| [Athena Iceberg](/connectors/outputs/athenaiceberg) | AWS Glue | SQL in Athena on Iceberg tables |
| [DuckDB](/connectors/outputs/duckdb) | Files on disk (`file://`) | Local analytics and CI |

Hive Athena (the [Athena](/connectors/outputs/athena) destination) writes Hive tables on S3, not Iceberg. `skipprd query` can still read in-flight rows for that pipeline, but the durable table is the Glue/Hive one.

## What Skipprd maintains

Iceberg destinations keep their own tables. They expire old snapshots (they keep recent commits, not unbounded history), compact small files, and drop delete files so metadata stays small. Older snapshots are not a time-travel product promise — query the current table.

## Query

Query Skippr. One SQL statement is live plus history. Tables are `<pipeline>.<namespace>`:

```bash
skipprd query --plain --sql "SELECT * FROM bikehire.rides LIMIT 20"
skipprd query --sql "STREAM * FROM bikehire.rides LIMIT 100"
```

For Iceberg destinations, `SELECT` returns the committed lake table plus live WAL slices Skippr Store pinned for that statement. `STREAM` is the same SQL over the same table; it tails new results to stdout as they arrive. `--watch` re-runs a SELECT in the interactive view. See [Stream SQL](/concepts/stream-sql) and [skipprd query](/cli/query).

## Next steps

- [Stream SQL](/concepts/stream-sql)
- [Materialised views](/concepts/materialised-views)
- [Skippr Store](/configuration/skippr-store)
- [SkipprLake](/connectors/outputs/skipprlake)
- [Athena Iceberg](/connectors/outputs/athenaiceberg)
- [DuckDB](/connectors/outputs/duckdb)
- [How Skipprd works](/concepts/how-it-works)
