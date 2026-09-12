---
title: "Quick Start: Snowflake"
description: Load JSON from S3 into Snowflake with Skipprd discover, Skipprd schema, and Skipprd sync using key-pair auth.
---

# Quick Start: Snowflake

Take public sample JSON on S3, discover nested schema, land it in Snowflake. Skipprd stages Parquet and runs `COPY INTO` through the Snowflake SQL API. Schema DDL runs before data flows. After sync, run SQL in Snowflake.

## Prerequisites

- `skipprd` on `PATH` ([Install](install.md))
- A Snowflake account, warehouse, database, schema, and stage
- Key-pair auth (preferred) or a password
- AWS credentials that can read the sample bucket

```bash
skipprd --version
export SNOWFLAKE_ACCOUNT="myorg-myaccount"
export SNOWFLAKE_USER="skippr_loader"
export SNOWFLAKE_PRIVATE_KEY_PATH="/path/to/rsa_key.p8"
export AWS_ACCESS_KEY_ID="your-key"
export AWS_SECRET_ACCESS_KEY="your-secret"
export AWS_DEFAULT_REGION="us-east-1"
```

## skippr.yml

The sample prefix is JSON. Discover infers the contract; it does not load Snowflake.

```yaml
skippr:
  workspace: quickstart
  skippr_s3_bucket: your-state-bucket

pipelines:
  bikehire:
    data_source: data_sources.sample
    data_sink: data_sinks.warehouse

data_sources:
  sample:
    S3:
      s3_bucket: skippr-public-sample-data
      s3_prefix: bike-hire

data_sinks:
  warehouse:
    Snowflake:
      account: "myorg-myaccount"
      user: "skippr_loader"
      private_key_path: "${SNOWFLAKE_PRIVATE_KEY_PATH}"
      warehouse: "COMPUTE_WH"
      database: "RAW_DATA"
      schema: "PUBLIC"
      role: "LOADER_ROLE"
      stage: "@SKIPPR_STAGE"
```

## Discover, schema, sync

```bash
skipprd discover --pipeline bikehire --log
skipprd schema --pipeline bikehire
skipprd sync --pipeline bikehire --once --log
```

`schema` prints field names and types. `--once` runs a single pass and exits. Then `SELECT` the landed table in Snowflake. Namespace-to-table mapping lowercases names and turns dots into underscores.

Full sink options: [Snowflake](/connectors/outputs/snowflake). Source: [S3](/connectors/inputs/s3).

## Troubleshooting

- **JWT / key-pair errors** — the key must be PKCS8 PEM. If both `SNOWFLAKE_PRIVATE_KEY_PATH` and `SNOWFLAKE_PASSWORD` are set, key-pair wins.
- **COPY INTO fails** — the role needs usage on the stage and `INSERT` on the target schema. Confirm `stage` matches an existing stage.
- **Warehouse suspended** — the warehouse must be resumable by that role.

## Next

- [PostgreSQL](quickstart-postgres.md) for a local warehouse.
- [CDC](/cdc/) when you need final-state replication instead of a bounded file load.
