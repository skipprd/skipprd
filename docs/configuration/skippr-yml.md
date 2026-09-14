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

## Environment values

Whole YAML scalar values can reference environment variables:

```yaml
connection_string: ${MSSQL_CONNECTION_STRING}
```

Skipprd loads `.env` then `.env.local` next to `skippr.yml` before resolving those references.
