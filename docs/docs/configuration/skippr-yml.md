# skippr.yml

`skippr.yml` is the canonical Skipprd project file. One file, one shape: Skipprd plugin entries under `data_sources` and `data_sinks`. `skipprd discover`, `skipprd schema`, and `skipprd sync` run against this file.

`source` and `warehouse` in the example below are **logical names**, not reserved words. They may be `mssql_prod`, `raw_snowflake`, `my_warehouse`, or any other key. Pipelines refer to them by section-qualified reference.

```yaml
skippr:
  workspace: mssql_migration

pipelines:
  mssql-migration:
    data_source: data_sources.source
    data_sink: data_sinks.warehouse

data_sources:
  source:
    Mssql:
      connection_string: ${MSSQL_CONNECTION_STRING}

data_sinks:
  warehouse:
    Snowflake:
      account: ${SNOWFLAKE_ACCOUNT}
      user: ${SNOWFLAKE_USER}
      database: ANALYTICS
      schema: RAW
      warehouse: COMPUTE_WH
      role: ACCOUNTADMIN
      private_key_path: ${SNOWFLAKE_PRIVATE_KEY_PATH}

schema_sinks: {}

dbt:
  target_schema: mssql_migration
  silver_suffix: silver
  gold_suffix: gold
```

Athena ingest lives on the **same** `Athena:` sink object using Skipprd field names (`s3_bucket`, `s3_prefix`, `athena_workgroup_name`, `glue_database_name`, `athena_results_s3_bucket`).

## Root sections

| Section | Used by | Purpose |
|---|---|---|
| `skippr` | Skipprd | Workspace, state bucket, WAL/offset options |
| `pipelines` | Skipprd | Pipeline graph: source, sink, schema sink, transforms |
| `data_sources` | `skipprd discover` / `skipprd sync` | Runtime source plugin configs |
| `data_sinks` | `skipprd sync` | Runtime ingest sinks |
| `deadletter_sinks` | `skipprd sync` | Optional deadletter write targets |
| `schema_sinks` | `skipprd sync` | Runtime schema/catalog plugin configs |

There is no `warehouses:` section.

## `skippr` keys

| Key | What it does |
|-----|----------------|
| `workspace` | Workspace name for this project |
| `wal_s3_bucket` | Dedicated bucket for WAL segments when `WAL_STORAGE=s3` |
| `offset_store` | Where skipprd stores offsets and checkpoints: `sled` (local disk, default), `dynamodb`, or `cloud-tables` (Skippr Cloud Tables) |
| `offset_dynamodb_table` | Table name for DynamoDB or Cloud Tables. Required when `offset_store` is `dynamodb` or `cloud-tables`, and for `WAL_STORAGE=clustered` |

`WAL_STORAGE` (`disk`, `s3`, `clustered`) is an environment variable, not a YAML field. See [offset store](offset-store-dynamodb.md) and [buffering](buffering.md).

## Environment values

Whole YAML scalar values can reference environment variables:

```yaml
connection_string: ${MSSQL_CONNECTION_STRING}
```

Skipprd loads `.env` then `.env.local` next to `skippr.yml` before resolving those references.
