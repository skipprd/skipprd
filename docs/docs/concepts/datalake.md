---
title: Datalake
description: Land Skipprd pipelines as Iceberg tables in object storage so Athena, Snowflake, DuckDB, and skipprd query can read the same files.
---

# Datalake

A datalake stores tables as files in object storage so many tools can read them. You do not copy the data into one vendor's warehouse first.

Skipprd is not a warehouse. It lands pipelines as [Apache Iceberg](https://iceberg.apache.org/) tables on Parquet. Athena, Snowflake, DuckDB, and `skipprd query` can all read those tables.

Use a lake destination when you want one copy of the data and several query engines. Use a warehouse destination (Snowflake, BigQuery, PostgreSQL) when that warehouse is the system of record.

## How a pipeline lands in the lake

1. Skipprd reads a batch from the source and commits it to the write-ahead log.
2. It writes the batch into Iceberg files in your bucket or warehouse path.
3. The catalog (Glue, SkipprLake, or a local DuckDB catalog) points at the new snapshot.

You query the Iceberg name (`namespace.table`). You do not open Parquet files by hand.

## Destinations that write Iceberg

| Destination | Catalog | Typical use |
|---|---|---|
| [SkipprLake](/connectors/outputs/skipprlake) | A table you name (`catalog_table`) | Skippr-managed lake, query with `skipprd query` |
| [Athena Iceberg](/connectors/outputs/athenaiceberg) | AWS Glue | SQL in Athena on Iceberg tables |
| [DuckDB](/connectors/outputs/duckdb) | Files on disk (`file://`) | Local analytics and CI |

Hive Athena (the [Athena](/connectors/outputs/athena) destination) writes Hive tables on S3, not Iceberg. `skipprd query` can still read in-flight WAL rows for that pipeline, but the durable table is the Glue/Hive one.

## What Skipprd maintains

Iceberg destinations keep their own tables. They expire old snapshots (they keep recent commits, not unbounded history), compact small files, and drop delete files so metadata stays small. Older snapshots are not a time-travel product promise — query the current table.

## Query

```bash
skipprd query --pipeline bikehire "SELECT * FROM rides LIMIT 20"
```

For Iceberg destinations this is the lake table plus any rows still in the WAL. For the query command itself, see [skipprd query](/cli/query).

## Next steps

- [SkipprLake](/connectors/outputs/skipprlake)
- [Athena Iceberg](/connectors/outputs/athenaiceberg)
- [DuckDB](/connectors/outputs/duckdb)
- [How Skipprd works](/concepts/how-it-works)
