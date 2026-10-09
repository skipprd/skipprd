---
title: "Quickstart: Snowflake"
description: Load sample JSON from S3 into a Snowflake table with key-pair authentication, using skippr.yml, skipprd discover, and skipprd sync.
---

# Quickstart: Snowflake

In about fifteen minutes you will load public sample JSON from S3 into a Snowflake table and query it.

Skipprd infers the schema, creates the Snowflake schema and table for you, stages Parquet files, and loads them with `COPY INTO`. You provide a Snowflake user, a warehouse, and a database.

## Before you begin

You need:

- **Skipprd installed.** Check with `skipprd --version`, or `python -c "import skippr"` if you use Python. See [Install](/getting-started/install).
- **A Snowflake account** and a role that can create users, roles, and grants (for example `ACCOUNTADMIN` or `SECURITYADMIN`) to run the setup SQL below once.
- **Your Snowflake account identifier**, in `orgname-accountname` form (for example `myorg-myaccount`). Find it in Snowsight under your account menu.
- **OpenSSL**, to create a key pair. Check with `openssl version`.
- **AWS credentials** that can read `s3://skippr-public-sample-data/bike-hire/`. Any AWS account works.

Export your AWS credentials and create an empty working directory. Skipprd reads `skippr.yml` from the current directory, so run every command from there.

```bash
export AWS_ACCESS_KEY_ID="your-access-key-id"
export AWS_SECRET_ACCESS_KEY="your-secret-access-key"
export AWS_DEFAULT_REGION="us-east-1"

mkdir skippr-snowflake && cd skippr-snowflake
```

## 1. Create a Snowflake user for Skipprd

Skipprd signs in with a key pair rather than a password. Key-pair sign-in keeps working when your account enforces multi-factor authentication, which password sign-in does not.

Generate an unencrypted PKCS#8 private key and its public key. Skipprd reads the private key file directly, so it must not have a passphrase.

```bash
openssl genrsa 2048 | openssl pkcs8 -topk8 -inform PEM -out rsa_key.p8 -nocrypt
openssl rsa -in rsa_key.p8 -pubout -out rsa_key.pub
export SNOWFLAKE_PRIVATE_KEY_PATH="$PWD/rsa_key.p8"
```

In a Snowflake worksheet, create a role, a user with that public key, and the grants Skipprd needs. Paste the contents of `rsa_key.pub` without the `-----BEGIN PUBLIC KEY-----` and `-----END PUBLIC KEY-----` lines.

```sql
CREATE ROLE IF NOT EXISTS LOADER_ROLE;
CREATE USER IF NOT EXISTS skippr_loader
  DEFAULT_ROLE = LOADER_ROLE
  RSA_PUBLIC_KEY = 'MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA...';
GRANT ROLE LOADER_ROLE TO USER skippr_loader;

CREATE WAREHOUSE IF NOT EXISTS COMPUTE_WH WAREHOUSE_SIZE = XSMALL AUTO_SUSPEND = 60 AUTO_RESUME = TRUE;
CREATE DATABASE IF NOT EXISTS RAW_DATA;

GRANT USAGE ON WAREHOUSE COMPUTE_WH TO ROLE LOADER_ROLE;
GRANT USAGE, CREATE SCHEMA ON DATABASE RAW_DATA TO ROLE LOADER_ROLE;
```

`CREATE SCHEMA` on the database lets Skipprd create the `SKIPPR_QUICKSTART` schema. Because `LOADER_ROLE` then owns that schema, it can create and alter tables in it without further grants.

## 2. Describe the pipeline

Each tab writes the same `skippr.yml`. Replace `myorg-myaccount` with your account identifier.

::: code-group

```python [Python]
from skippr import Config, DataSinkSnowflake, DataSourceS3, LocalStorage, Pipeline

cfg = Config().workspace("quickstart").storage(LocalStorage())
sample = cfg.data_source(
    "sample",
    DataSourceS3(s3_bucket="skippr-public-sample-data", s3_prefix="bike-hire"),
)
warehouse = cfg.data_sink(
    "warehouse",
    DataSinkSnowflake(
        account="myorg-myaccount",
        user="skippr_loader",
        private_key_path="${SNOWFLAKE_PRIVATE_KEY_PATH}",
        role="LOADER_ROLE",
        warehouse="COMPUTE_WH",
        database="RAW_DATA",
        schema="SKIPPR_QUICKSTART",
    ),
)
cfg.pipeline("bikehire", Pipeline(data_source=sample, data_sink=warehouse))
cfg.save("skippr.yml")
```

```bash [CLI]
skipprd --workspace quickstart --storage-mode local connect data-source s3 \
  --pipeline bikehire \
  --name sample \
  --s3-bucket skippr-public-sample-data \
  --s3-prefix bike-hire

skipprd connect data-sink snowflake \
  --pipeline bikehire \
  --name warehouse \
  --account myorg-myaccount \
  --user skippr_loader \
  --private-key-path '${SNOWFLAKE_PRIVATE_KEY_PATH}' \
  --role LOADER_ROLE \
  --warehouse COMPUTE_WH \
  --database RAW_DATA \
  --schema SKIPPR_QUICKSTART
```

```yaml [YAML]
skippr:
  workspace: quickstart
  skipprd_el_storage_mode: local

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
      account: myorg-myaccount
      user: skippr_loader
      private_key_path: ${SNOWFLAKE_PRIVATE_KEY_PATH}
      role: LOADER_ROLE
      warehouse: COMPUTE_WH
      database: RAW_DATA
      schema: SKIPPR_QUICKSTART
```

:::

What the settings mean:

- `private_key_path: ${SNOWFLAKE_PRIVATE_KEY_PATH}` is read from the environment when the pipeline starts, so the path never has to live in the file. In the CLI, single quotes stop your shell from expanding it early. You can also put the variable in a `.env` file next to `skippr.yml`.
- With `private_key_path` set, Skipprd uses key-pair sign-in even if `password` is also set.
- Skipprd stages files in your user stage (`@~`) unless you set `stage` to a named stage.
- `LocalStorage()` (`skipprd_el_storage_mode: local`) keeps Skipprd's own state, such as the discovered schema and how far it has read, in `./data`. That is fine for a trial; for production, see [State store](/configuration/skippr-store).

## 3. Discover the schema

::: code-group

```python [Python]
import skippr

s = skippr.Session(skippr.Config.discover().get_pipeline("bikehire"))
s.discover()
```

```bash [CLI]
skipprd discover --pipeline bikehire
skipprd schema --pipeline bikehire
```

:::

`discover` samples the JSON and records each field's name and type. It does not touch Snowflake. `skipprd schema` prints the fields, which become your table's columns. Nested objects and arrays become Snowflake `OBJECT` and `ARRAY` columns rather than being flattened.

## 4. Sync

::: code-group

```python [Python]
import skippr

s = skippr.Session(skippr.Config.discover().get_pipeline("bikehire"))
s.sync(once=True)
```

```bash [CLI]
skipprd sync --pipeline bikehire --once
```

:::

Sync reads the source and writes each batch to Skipprd's write-ahead log (WAL) first. A crash retries that committed batch; Snowflake applies it once. It then creates the `SKIPPR_QUICKSTART` schema and the `BIKEHIRE` table if they do not exist, uploads Parquet to the stage, and loads it with `COPY INTO`. When new fields appear later, Skipprd adds the columns before loading.

`--once` (`once=True`) runs a single pass and exits. Without it, sync keeps running and checks the source for new data on a schedule.

## Check it worked

In a Snowflake worksheet, run:

```sql
USE WAREHOUSE COMPUTE_WH;
SELECT COUNT(*) AS row_count FROM RAW_DATA.SKIPPR_QUICKSTART.BIKEHIRE;
SELECT * FROM RAW_DATA.SKIPPR_QUICKSTART.BIKEHIRE LIMIT 10;
```

You should see a non-zero `row_count` and the columns `skipprd schema` printed in step 3. The table is named after the pipeline, in upper case.

## Troubleshooting

- **`250001` or a connection failure** — the account identifier is wrong. Use the `orgname-accountname` form (for example `myorg-myaccount`), or the account locator with its region (for example `xy12345.us-east-1`).
- **`JWT token is invalid`** — Snowflake does not have the matching public key. Rerun `ALTER USER skippr_loader SET RSA_PUBLIC_KEY = '...'` with the contents of `rsa_key.pub`, and check that `SNOWFLAKE_PRIVATE_KEY_PATH` points at the matching `rsa_key.p8`.
- **An error reading or parsing the private key** — the file is encrypted or not PKCS#8. Regenerate it with the `openssl ... -nocrypt` command in step 1.
- **`390197 — Multi-factor authentication is required`** — the sink is using a password. Set `private_key_path` and use key-pair sign-in.
- **`Insufficient privileges` or `does not exist or not authorized`** — `LOADER_ROLE` is missing a grant, or `role` is not set in the config. Rerun the grants in step 1 and confirm `role: LOADER_ROLE`.
- **The warehouse is suspended and does not resume** — the role needs `USAGE` on the warehouse, and the warehouse needs `AUTO_RESUME = TRUE`.
- **`skippr.yml references ${SNOWFLAKE_PRIVATE_KEY_PATH} ... but that environment variable is not set`** — export it in the shell that runs Skipprd, or add it to a `.env` file next to `skippr.yml`.

For more detail, rerun the failing command with `--log debug`, or see [Troubleshooting](/operations/troubleshooting).

## Next steps

- [Snowflake destination](/connectors/outputs/snowflake) — named stages, external staging, and every option.
- [S3 source](/connectors/inputs/s3) — point the pipeline at your own bucket.
- [Change data capture](/cdc/) — replicate a database into Snowflake and keep it at current state.
- [Schema discovery and evolution](/concepts/schema) — how types are inferred and how new fields are added.
- [Quickstart: PostgreSQL](/getting-started/quickstart-postgres) — try a warehouse you can run locally.
