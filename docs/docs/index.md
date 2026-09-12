---
title: Skipprd
description: Self-hosted ELT engine. Describe a source and a destination in skippr.yml, then run Skipprd discover, Skipprd schema, and Skipprd sync.
---

# Skipprd

Skipprd is an ELT engine in a single, portable binary. It discovers your source data, evolves your schemas, fixes common serialisation issues and more, on the fly during ingest. It guarantees deliverable, backwards-compatible data to your destination. New field, no problem. Existing field changed type, no problem. Nested data types evolved a new array, no problem. It just works.

You describe a source and a destination in one `skippr.yml`, then Skipprd reads the shape of the data and moves it — durably — into a warehouse you already run. Discovery walks the source, maps types the same way every time, and writes that contract down. Change data capture aims at the table as it should be: order tokens, tombstones, a final state you can reconcile, not a pile of logs to replay by hand. Ingest is WAL-first. A batch is real once it is committed. Crash recovery starts there. Rows travel from the machine running `skipprd` to your destination.

Plugins arrive on demand from `install.skippr.io`. The host stays small; connectors version on their own.

## Install

You need a machine that can reach the source and the destination, plus credentials for both. Published binaries cover macOS arm64 and Linux x86_64.

```bash
brew tap skipprd/tap
brew install skipprd
```

Or:

```bash
curl -sL https://raw.githubusercontent.com/skipprd/skipprd/main/install.sh | sh
skipprd --version
```

Runtime source, sink, and schema plugins download on first use and cache under `~/.skippr/runtime_plugins`. Full setup, including a user-local install directory, is in [Install](/getting-started/install).

## First pipeline

Take JSON objects in S3, discover their nested schema, land them in Snowflake, then run SQL in Snowflake.

```bash
skipprd --version
```

Export credentials the process can read. AWS reads the sample JSON. Snowflake uses a key pair.

```bash
export AWS_ACCESS_KEY_ID="your-key"
export AWS_SECRET_ACCESS_KEY="your-secret"
export AWS_DEFAULT_REGION="us-east-1"
export SNOWFLAKE_ACCOUNT="myorg-myaccount"
export SNOWFLAKE_USER="skippr_loader"
export SNOWFLAKE_PRIVATE_KEY_PATH="/path/to/rsa_key.p8"
```

Write `skippr.yml` in the working directory. The sample prefix is JSON. Discover infers the contract; it does not load Snowflake.

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

```bash
skipprd discover --pipeline bikehire --log
skipprd schema --pipeline bikehire
skipprd sync --pipeline bikehire --once --log
```

`discover` walks the JSON and writes field names and types. `schema` prints that contract. `sync --once` loads one pass into Snowflake and exits. Without `--once`, sync keeps reading. Then `SELECT` the landed table in Snowflake.

Full walkthrough: [Snowflake](/getting-started/quickstart-snowflake). Source and sink reference: [S3](/connectors/inputs/s3), [Snowflake](/connectors/outputs/snowflake). Pipeline WAL and resume: [pipeline flow](/getting-started/how-it-works).

Other warehouses: [PostgreSQL](/getting-started/quickstart-postgres), [BigQuery](/getting-started/quickstart-bigquery), [S3 to Athena](/getting-started/quickstart).

## When something fails

- **`skipprd: command not found`** — the install directory is not on `PATH`. Install to `/usr/local/bin` or set `SKIPPR_INSTALL_DIR`.
- **Plugin download fails** — the host needs HTTPS access to `install.skippr.io`. Check the network path from the machine running Skipprd, not from a laptop that will not run the job.
- **Destination auth errors** — credentials live in the environment (`SNOWFLAKE_PRIVATE_KEY_PATH`, `POSTGRES_PASSWORD`, `GOOGLE_APPLICATION_CREDENTIALS`). They do not belong in git.

More in [troubleshooting](/operations/troubleshooting).
