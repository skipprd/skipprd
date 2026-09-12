---
title: "Quick Start: S3 to Athena"
description: Ingest JSON from S3 into Athena-queryable Parquet with Skipprd discover and Skipprd sync.
---

# Quick Start: S3 to Athena

Load public sample JSON from S3 into Parquet that Athena can query. The engine infers schema, writes through the WAL, and registers Glue tables.

## Prerequisites

- `skipprd` on `PATH` ([Install](install.md))
- AWS credentials that can read the sample bucket and write your output bucket, plus Glue and Athena in the same region

```bash
export AWS_ACCESS_KEY_ID="your-key"
export AWS_SECRET_ACCESS_KEY="your-secret"
export AWS_DEFAULT_REGION="us-east-1"
```

## skippr.yml

```yaml
skippr:
  workspace: quickstart
  skippr_s3_bucket: your-state-bucket

pipelines:
  bikehire:
    data_source: data_sources.sample
    data_sink: data_sinks.athena
    schema_sink: schema_sinks.glue

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
```

## Discover, schema, sync

```bash
skipprd discover --pipeline bikehire --log
skipprd schema --pipeline bikehire
skipprd sync --pipeline bikehire --once --log
```

After sync, query the Glue/Athena table in Athena.

Sync logs to watch:

- `Resolving runtime ... plugin ... from published registry` — first-use plugin download
- `Uploaded ...parquet to S3` — data landing
- `Pipeline sync complete` — the pass finished

## Troubleshooting

- **Glue / Athena access denied** — the principal needs `glue:CreateTable` (and related) on `skippr_quickstart` plus S3 write on `your-output-bucket`.
- **Empty table in Athena** — confirm `--once` completed without deadletters, then query in Athena.
- **Wrong region** — `AWS_DEFAULT_REGION` must match the buckets and Glue catalog.

## Next

- [Pipeline flow](how-it-works.md)
- [Athena sink](/connectors/outputs/athena)
- [S3 source](/connectors/inputs/s3)
