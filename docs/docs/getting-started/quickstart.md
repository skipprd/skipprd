# Quick Start: S3 to Athena

This guide walks through ingesting JSON data from S3 into Athena-queryable Parquet tables in under 5 minutes.

## 1. Install Skippr

```bash
curl -sL "https://raw.githubusercontent.com/skipprd/skipprd/main/install.sh" | sudo bash
```

The install script places `skippr-el` on your machine. Runtime source, sink, and schema plugins are downloaded automatically the first time the pipeline needs them.

## 2. Set your environment

```bash
export AWS_ACCESS_KEY_ID="your-key"
export AWS_SECRET_ACCESS_KEY="your-secret"
export AWS_DEFAULT_REGION="us-east-1"

# Pipeline identity
export PIPELINE_NAME=bikehire

# Source: public sample data on S3
export DATA_SOURCE_PLUGIN_NAME=s3
export DATA_SOURCE_S3_BUCKET=skippr-public-sample-data
export DATA_SOURCE_S3_PREFIX=bike-hire

# Destination: your S3 bucket + Glue catalog
export DATA_OUTPUT_S3_BUCKET=your-output-bucket
export DATA_OUTPUT_S3_PREFIX=data/bikehire
export SCHEMA_OUTPUT_GLUE_DATABASE_NAME=skippr_quickstart

# Skippr state bucket (metadata, offsets, WAL when using S3 WAL)
export SKIPPR_S3_BUCKET=your-state-bucket
```

## 3. Discover the schema

Skippr connects to the source, samples records, and infers the full nested schema:

```bash
skippr-el discover --pipeline bikehire --log
```

You'll see output showing discovered namespaces and fields. The schema is persisted to S3 as pipeline metadata.

## 4. Enable and sync

Enable the pipeline, then run sync to ingest data:

```bash
skippr-el query --sql "ENABLE PIPELINE bikehire"
skippr-el sync --pipeline bikehire --log
```

Sync reads from the source, buffers through the WAL, compacts into Parquet, uploads to S3, and registers Glue partitions. Watch the logs for:

- `Resolving runtime ... plugin ... from published registry` — plugin discovery and download on first use
- `Uploaded ...parquet to S3 (rows=..., bytes=...)` — data landing
- `Pipeline sync complete` — run finished successfully

## 5. Query the data

```bash
skippr-el query --sql "SELECT COUNT(*) FROM bikehire"
```

Your data is now in Athena. You can also query directly from the AWS Athena console.

## What just happened?

1. **discover** — connected to the S3 source, sampled JSON records, inferred the nested schema (types, field names, nesting), and saved it as pipeline metadata
2. **sync** — ingested all records through the WAL, compacted them into Snappy-compressed Parquet, uploaded to `s3://your-output-bucket/data/bikehire/`, created a Glue database and table with the discovered schema, and registered Hive-style time partitions
3. **query** — executed SQL against the Glue catalog via Athena

## Next steps

- [How Skippr Works](../concepts/how-it-works.md) — pipeline lifecycle, WAL, compaction
- [Configuration Reference](../configuration/overview.md) — all environment variables
- [CLI Reference](../cli/discover.md) — command flags and options
- [SQL Reference](../sql/reference.md) — all supported SQL statements
