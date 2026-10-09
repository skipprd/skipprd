---
title: skippr.yml reference
description: Every section and key in skippr.yml, how ${ENV} references and .env files resolve, and how logical names wire pipelines to connectors.
---

# skippr.yml reference

`skippr.yml` describes what Skipprd moves: your sources, your destinations, and the pipelines that connect them. `skipprd discover`, `skipprd sync`, `skipprd query`, and Python `Session` all read the same file. `skipprd connect` and Python `Config.save()` write it for you, so you rarely edit it by hand, but this page explains every part of it.

## Full example

This file syncs two Postgres tables into Snowflake. The three tabs produce the same `skippr.yml`.

::: code-group

```python [Python]
from skippr import (
    Config,
    DataSinkSnowflake,
    DataSourcePostgres,
    EnvRef,
    LocalStorage,
    Pipeline,
)

cfg = Config().workspace("analytics").storage(LocalStorage())

app_db = cfg.data_source(
    "app_db",
    DataSourcePostgres(
        host="db.internal",
        user="skippr_reader",
        password=EnvRef("APP_DB_PASSWORD"),
        database="app",
        tables=["orders", "customers"],
    ),
)

warehouse = cfg.data_sink(
    "warehouse",
    DataSinkSnowflake(
        account="${SNOWFLAKE_ACCOUNT}",
        user="SKIPPR_LOADER",
        password=EnvRef("SNOWFLAKE_PASSWORD"),
        warehouse="COMPUTE_WH",
        database="ANALYTICS",
        schema="RAW",
        role="LOADER",
    ),
)

cfg.pipeline("orders", Pipeline(data_source=app_db, data_sink=warehouse))
cfg.save("skippr.yml")
```

```bash [CLI]
skipprd --workspace analytics --storage-mode local connect data-source postgres \
  --pipeline orders \
  --name app_db \
  --host db.internal \
  --user skippr_reader \
  --password '${APP_DB_PASSWORD}' \
  --database app \
  --tables orders \
  --tables customers

skipprd connect data-sink snowflake \
  --pipeline orders \
  --name warehouse \
  --account '${SNOWFLAKE_ACCOUNT}' \
  --user SKIPPR_LOADER \
  --password '${SNOWFLAKE_PASSWORD}' \
  --warehouse COMPUTE_WH \
  --database ANALYTICS \
  --schema RAW \
  --role LOADER
```

```yaml [YAML]
# Engine settings shared by every pipeline in this file.
skippr:
  workspace: analytics             # groups pipelines; part of each pipeline's identity
  skipprd_el_storage_mode: local   # keep pipeline metadata on local disk, not S3

# Each pipeline reads one source and writes to optional destinations.
pipelines:
  orders:                          # pipeline name: skipprd sync --pipeline orders
    data_source: data_sources.app_db
    data_sink: data_sinks.warehouse

data_sources:
  app_db:                          # logical name you choose
    Postgres:                      # connector type
      host: db.internal
      user: skippr_reader
      password: ${APP_DB_PASSWORD} # read from the environment at run time
      database: app
      tables: [orders, customers]

data_sinks:
  warehouse:
    Snowflake:
      account: ${SNOWFLAKE_ACCOUNT}
      user: SKIPPR_LOADER
      password: ${SNOWFLAKE_PASSWORD}
      warehouse: COMPUTE_WH
      database: ANALYTICS
      schema: RAW
      role: LOADER
```

:::

Set the referenced variables before you run, then sync:

```bash
export APP_DB_PASSWORD='your-postgres-password'
export SNOWFLAKE_ACCOUNT='xy12345.eu-west-1'
export SNOWFLAKE_PASSWORD='your-snowflake-password'

skipprd discover --pipeline orders
skipprd sync --pipeline orders --once
```

## Where Skipprd finds the file

Skipprd uses the first of these that exists:

1. The path you pass with `--config` (any command).
2. The path in the `SKIPPR_CONFIG_FILE` environment variable.
3. In the current directory: `skippr.yml`, `skippr.yaml`, `skipprd.yml`, `skipprd.yaml`, `skipprd.toml`, `skipprd.json`.

YAML, TOML, and JSON files share the same structure. If none is found, Skipprd starts with an empty configuration, and commands that need a pipeline report that it is not defined.

## Root sections

| Section | Required | What it holds |
|---|---|---|
| `skippr` | No | Engine settings: workspace, where state lives, and the state store. Keys are listed below. |
| `pipelines` | Yes, to run anything | One entry per pipeline. Each names its source and optional destinations. See [Pipelines](/configuration/pipeline). |
| `data_sources` | Yes | Source connectors, keyed by a name you choose. See [Sources](/configuration/input). |
| `data_sinks` | No | Destination connectors that receive synced records. See [Destinations](/configuration/output). |
| `deadletter_sinks` | No | Destinations for records that fail to transform or load. See [Deadletters](/concepts/deadletters). |
| `schema_sinks` | No | Catalogs that receive table definitions, such as AWS Glue for Athena. |
| `dbt`, `vector_sources` | No | Read by Skippr Data Engineer. Skipprd accepts and ignores them. |

Skipprd rejects any other root key, and any unknown key inside these sections, with `Invalid Skippr configuration in '<file>': unknown field ...`. That catches typos before a run starts.

## `skippr` keys

| Key | Type | Default | Description |
|---|---|---|---|
| `workspace` | string | `default` | Groups related pipelines. The workspace and pipeline name together identify a pipeline's saved progress, so changing either one starts that pipeline from the beginning. Falls back to `WORKSPACE_NAME`. |
| `tenant` | string | `default` | Top-level owner of the workspace. Leave unset for self-hosted runs. Falls back to `TENANT`. |
| `skipprd_el_storage_mode` | `s3` or `local` | `s3` | Where pipeline metadata (discovered schemas and stats) is kept. `local` keeps it under `DATA_DIR`; `s3` keeps it in `skippr_s3_bucket`. Falls back to `SKIPPRD_EL_STORAGE_MODE`. |
| `skippr_s3_bucket` | string | none | S3 bucket for pipeline metadata when `skipprd_el_storage_mode` is `s3`, and the default bucket for the WAL when `WAL_STORAGE=s3`. Falls back to `SKIPPR_S3_BUCKET`. |
| `wal_s3_bucket` | string | `skippr_s3_bucket` | Dedicated bucket for the write-ahead log when `WAL_STORAGE=s3`. See [WAL and buffering](/configuration/buffering). |
| `store.type` | `sled`, `dynamodb`, or `cloud-tables` | `sled` | Where sync progress is saved. `sled` is the local on-disk state store. See [State store](/configuration/skippr-store). |
| `store.name` | string | none | DynamoDB or Cloud Tables table name. Required when `store.type` is `dynamodb` or `cloud-tables`. |

The WAL backend is not a YAML key. Set it with the `WAL_STORAGE` environment variable or the `--wal-storage` flag, because it describes the machine Skipprd runs on rather than the data you move.

In Python, `Config().storage(LocalStorage())` writes `skipprd_el_storage_mode: local`, and `Config().storage(S3Storage("bucket"))` writes `skipprd_el_storage_mode: s3` plus `skippr_s3_bucket`. `Config().store(DynamoDbStore("table"))` writes `store.type` and `store.name`.

## Connector entries

Every entry under `data_sources`, `data_sinks`, `deadletter_sinks`, and `schema_sinks` has the same shape: a logical name, then exactly one connector type, then that connector's fields.

```yaml
data_sinks:
  lake:                          # logical name
    schema_sink: schema_sinks.glue   # data and deadletter sinks only: link a catalog
    Athena:                      # connector type
      s3_bucket: my-lake
      s3_prefix: bronze/events
      athena_workgroup_name: primary
      athena_results_s3_bucket: my-athena-results
    # glue_database_name lives on schema_sinks.glue, not here
```

- Write the connector type with the casing shown on its page (`Postgres`, `S3`, `Snowflake`, `AthenaIceberg`).
- Each connector page lists its fields, types, and defaults. Start at the [connector catalog](/connectors/).
- `schema_sink` sits beside the connector type, not inside it.

## Logical names and references

Entry names such as `app_db` and `warehouse` are yours to choose. They are not reserved words. Use names that describe the system: `mssql_prod`, `raw_snowflake`, `clickstream`.

Pipelines and data sinks refer to entries as `<section>.<name>`:

| Field | Must reference |
|---|---|
| `pipelines.<name>.data_source` | `data_sources.<name>` |
| `pipelines.<name>.data_sink` | `data_sinks.<name>` |
| `pipelines.<name>.deadletter_sink` | `deadletter_sinks.<name>` |
| `<sink>.schema_sink` | `schema_sinks.<name>` |

An entry name cannot contain a dot. A reference to an entry that does not exist stops the run with `... references '<ref>', but '<name>' is not defined.`

Pipeline names `deadletters`, `wal`, `_skippr`, `skippr`, and `metadata` are reserved and rejected.

## Environment references

Write `${NAME}` as a value and Skipprd replaces it with the environment variable `NAME` when the pipeline runs. Use this for every secret, so `skippr.yml` is safe to commit.

```yaml
data_sources:
  app_db:
    Postgres:
      password: ${APP_DB_PASSWORD}
```

```bash
export APP_DB_PASSWORD='your-postgres-password'
```

The rules:

- **Whole values only.** `${NAME}` must be the entire value. `s3://${BUCKET}/raw` is not expanded and stays as literal text.
- **Text fields only.** References fill fields whose type is text. Write numbers and booleans directly.
- **Names** match `[A-Za-z_][A-Za-z0-9_]*`.
- **Unset or empty fails.** A missing variable stops the run with `skippr.yml references ${NAME} at <path>, but that environment variable is not set`. An empty one fails the same way with `is empty`.
- **Secrets must be references.** Secret fields (passwords, tokens, keys) reject plaintext. In Python they take `EnvRef("NAME")`; on the CLI, pass `'${NAME}'` in single quotes so your shell does not expand it.
- **When they resolve.** The CLI resolves every reference in the file when it starts, so each referenced variable must be set even if the pipeline you run does not use it. A Python `Session` resolves only the references its own pipeline uses.

## `.env` files

Before resolving references, Skipprd loads two optional files from the directory that contains `skippr.yml`:

1. `.env` sets variables that are unset or empty in your environment. Values you exported in the shell win.
2. `.env.local` then overrides any variable it names, including ones from your shell.

```bash
# .env.local — keep this out of version control
APP_DB_PASSWORD=your-postgres-password
SNOWFLAKE_PASSWORD=your-snowflake-password
```

Add `.env.local` to `.gitignore` and keep shared, non-secret defaults in `.env`.

## Check the file

Run `skipprd doctor` to validate references, connector entries, and secrets without moving data:

```bash
skipprd doctor
```

Each check prints `[ok]` or `[FAIL]`. Add `--output json` for machine-readable results.

## Next steps

- [Configuration overview](/configuration/overview) — how `skippr.yml`, environment variables, and flags combine.
- [Pipelines](/configuration/pipeline) — every pipeline key.
- [Sources](/configuration/input) and [Destinations](/configuration/output) — wire connectors into a pipeline.
- [skipprd connect](/cli/connect) — write `skippr.yml` from the command line.
