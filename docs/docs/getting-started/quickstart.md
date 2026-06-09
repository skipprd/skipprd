# Quick Start: S3 to Athena

This guide walks through ingesting JSON data from S3 into Athena-queryable Parquet tables in under 5 minutes.

## 1. Install Skippr

```bash
curl -sL "https://raw.githubusercontent.com/skipprd/skipprd/main/install.sh" | sudo bash
```

The install script places `skippr` on your machine. Runtime source, sink, and schema plugins are downloaded automatically the first time the pipeline needs them.

## 2. Create `skippr.yml`

Create a project file in your working directory:

```yaml
skippr:
  workspace: quickstart
  skippr_s3_bucket: your-state-bucket
  default_warehouse: primary

pipelines:
  bikehire:
    data_source: data_sources.sample
    data_sink: data_sinks.athena
    schema_sink: schema_sinks.glue
    model:
      warehouse: primary

data_sources:
  sample:
    S3:
      s3_bucket: skippr-public-sample-data
      s3_prefix: bike-hire

data_sinks:
  athena:
    Athena:
      s3_bucket: your-output-bucket
      s3_prefix: data/bikehire
      glue_database_name: skippr_quickstart
      athena_workgroup_name: primary
      athena_results_s3_bucket: your-athena-results

schema_sinks:
  glue:
    Glue:
      glue_database_name: skippr_quickstart

warehouses:
  primary:
    kind: athena
    workgroup: primary
    schema: skippr_quickstart
    result_s3: s3://your-athena-results/
```

Set AWS credentials through the normal AWS environment or instance role:

```bash
export AWS_ACCESS_KEY_ID="your-key"
export AWS_SECRET_ACCESS_KEY="your-secret"
export AWS_DEFAULT_REGION="us-east-1"
```

## 3. Discover the schema

Skippr connects to the source, samples records, and infers the full nested schema:

```bash
skippr discover --pipeline bikehire --log
```

You'll see output showing discovered namespaces and fields. The schema is persisted to S3 as pipeline metadata.

## 4. Enable and sync

Enable the pipeline, then run sync to ingest data:

```bash
skippr query --sql "ENABLE PIPELINE bikehire"
skippr sync --pipeline bikehire --log
```

Sync reads from the source, buffers through the WAL, compacts into Parquet, uploads to S3, and registers Glue partitions. Watch the logs for:

- `Resolving runtime ... plugin ... from published registry` — plugin discovery and download on first use
- `Uploaded ...parquet to S3 (rows=..., bytes=...)` — data landing
- `Pipeline sync complete` — run finished successfully

## 5. Query the data

```bash
skippr query --sql "SELECT COUNT(*) FROM bikehire"
```

Your data is now in Athena. You can also query directly from the AWS Athena console.

## Engine-only equivalent

The lightweight `skipprd` binary can run the same engine commands against the same `skippr.yml`:

```bash
skipprd --config skippr.yml discover --pipeline bikehire --log
skipprd --config skippr.yml sync --pipeline bikehire --log
```

## What just happened?

1. **discover** — connected to the S3 source, sampled JSON records, inferred the nested schema (types, field names, nesting), and saved it as pipeline metadata
2. **sync** — ingested all records through the WAL, compacted them into Snappy-compressed Parquet, uploaded to `s3://your-output-bucket/data/bikehire/`, created a Glue database and table with the discovered schema, and registered Hive-style time partitions
3. **query** — executed SQL against the Glue catalog via Athena

## Next steps

- [How Skippr Works](../concepts/how-it-works.md) — pipeline lifecycle, WAL, compaction
- [skippr.yml Reference](../configuration/skippr-yml.md) — canonical project config
- [CLI Reference](../cli/overview.md) — command flags and binary choices
- [SQL Reference](../sql/reference.md) — all supported SQL statements
