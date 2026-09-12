---
title: "Quick Start: PostgreSQL"
description: Load S3 files into PostgreSQL with Skipprd discover and Skipprd sync. No cloud warehouse account.
---

# Quick Start: PostgreSQL

Read objects from S3 and land them in PostgreSQL. Use this when you want a local warehouse without Snowflake or BigQuery.

## Prerequisites

- `skipprd` on `PATH` ([Install](install.md))
- PostgreSQL reachable from the machine running Skipprd
- AWS credentials for the source bucket
- State bucket for pipeline metadata (`skippr_s3_bucket`)

```bash
export POSTGRES_HOST="localhost"
export POSTGRES_USER="skippr"
export POSTGRES_PASSWORD="secret"
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
  files:
    data_source: data_sources.sample
    data_sink: data_sinks.warehouse

data_sources:
  sample:
    S3:
      s3_bucket: skippr-public-sample-data
      s3_prefix: bike-hire

data_sinks:
  warehouse:
    Postgres:
      host: localhost
      port: 5432
      user: skippr
      password: ${POSTGRES_PASSWORD}
      database: analytics
      schema: public
```

## Discover, schema, sync

```bash
skipprd discover --pipeline files --log
skipprd schema --pipeline files
skipprd sync --pipeline files --once --log
```

The sink creates the schema and tables when they do not exist, then inserts rows. See [PostgreSQL sink](/connectors/outputs/postgres) and [S3 source](/connectors/inputs/s3).

## Troubleshooting

- **password authentication failed** — `POSTGRES_PASSWORD` must match the database role. YAML `${POSTGRES_PASSWORD}` reads the environment; a literal password in git is a mistake.
- **S3 AccessDenied** — the AWS principal needs `s3:GetObject` on the source prefix.
- **relation does not exist** — wait for sync to finish; the sink creates tables on first write. Then `SELECT` in PostgreSQL.

## Next

- Swap the source to [PostgreSQL input](/connectors/inputs/postgres) when you are extracting from tables instead of files.
- [Snowflake](quickstart-snowflake.md) when you are ready for a cloud warehouse.
