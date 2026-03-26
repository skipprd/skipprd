# Skippr

Skippr is a data ingestion CLI that reads from sources like S3 or local files, automatically discovers schemas, and writes optimised Parquet to S3 with AWS Glue catalog tables queryable via Athena.

## Key capabilities

- **Schema discovery** — automatically infers nested schemas from JSON, CSV, or Parquet sources
- **Schema evolution** — detects field type changes and handles them without breaking downstream tables
- **Exactly-once delivery** — WAL-backed ingestion with offset tracking and integrity checks, surviving process crashes (including SIGKILL)
- **Athena-native output** — writes Parquet to S3, manages Glue databases/tables, and registers Hive partitions
- **Built-in SQL engine** — query destination tables, stream from the WAL, manage pipelines and schemas via SQL
- **Deadletter handling** — invalid records are captured as queryable Parquet in S3, not silently dropped
- **Stateless compute** — no clustering, no scaling groups. Single binary, single process. Even local disk is optional when using S3 WAL

## How it works

```
Source (S3, file)
  → discover (schema inference)
  → sync (ingest → WAL → compactor → Parquet → S3 + Glue)
  → query (Athena SQL, STREAM from WAL)
```

The three core commands map directly to the pipeline lifecycle:

| Command | Purpose |
|---|---|
| `skippr-el discover` | Connect to source, sample data, infer and persist schema |
| `skippr-el sync` | Ingest data, buffer through WAL, compact and upload Parquet, register partitions |
| `skippr-el query` | Run SQL against destination tables, manage pipelines and schemas |

## Quick start

Install the CLI and run your first pipeline in under 5 minutes. See the [Quick Start](getting-started/quickstart.md) guide.

## License

Skippr is licensed under the [Elastic License 2.0 (ELv2)](license.md).
