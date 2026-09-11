# skippr.yml

`skippr.yml` is the canonical Skippr project file. One file, one shape: skipprd plugin entries under `data_sources` and `data_sinks`. The engine binary is `skipprd`. Data Engineer is `sde`. Cloud is `skippr`. `skipprd discover` and `skipprd sync` invoke the skipprd runtime against this file; `sde model` and `sde query` compile the same sinks into the modeling stack in memory.

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

Athena ingest and query live on the **same** `Athena:` sink object using skipprd field names (`s3_bucket`, `s3_prefix`, `athena_workgroup_name`, `glue_database_name`, `athena_results_s3_bucket`) plus optional query-only keys (`region`, `catalog`, `max_concurrency`, `discovery_cache_ttl_secs`).

## Root sections

| Section | Used by | Purpose |
|---|---|---|
| `skippr` | `skippr` | Workspace, state bucket, WAL/offset options |
| `pipelines` | `skippr` | Pipeline graph: source, sink, schema sink, transforms |
| `data_sources` | `skipprd discover` / `skipprd sync` | Runtime source plugin configs |
| `data_sinks` | `skippr` | Runtime ingest sinks; also the query/model destination |
| `deadletter_sinks` | `skipprd sync` | Optional deadletter write targets |
| `schema_sinks` | `skipprd sync` | Runtime schema/catalog plugin configs |
| `dbt` | `sde model` | dbt naming: `target_schema`, `silver_suffix`, `gold_suffix` |
| `vector_sources` | `sde vector` | File/stdin sources for vector ingestion |

There is no `warehouses:` section. Query, model, and catalog use the pipeline's `data_sink`.

## Environment values

Whole YAML scalar values can reference environment variables:

```yaml
connection_string: ${MSSQL_CONNECTION_STRING}
```

Skippr loads `.env` then `.env.local` next to `skippr.yml` before resolving those references.
