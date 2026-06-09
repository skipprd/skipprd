# skippr.yml

`skippr.yml` is the canonical Skippr project file. Both `skippr` and `skipprd` read this file for shared engine commands such as `discover` and `sync`.

The root shape is the engine config:

```yaml
skippr:
  workspace: dev
  skippr_s3_bucket: my-skippr-state
  default_warehouse: primary

pipelines:
  my_pipeline:
    data_source: data_sources.source
    data_sink: data_sinks.landing
    schema_sink: schema_sinks.catalog
    transform:
      batch_time_fields: created_at
      batch_time_unit: day
    model:
      warehouse: primary

data_sources:
  source:
    S3:
      s3_bucket: raw-bucket
      s3_prefix: events/

data_sinks:
  landing:
    Athena:
      s3_bucket: warehouse-bucket
      s3_prefix: bronze/events
      glue_database_name: bronze_events
      athena_workgroup_name: primary
      athena_results_s3_bucket: athena-results

schema_sinks:
  catalog:
    Glue:
      glue_database_name: bronze_events

warehouses:
  primary:
    kind: athena
    workgroup: primary
    schema: bronze_events
    result_s3: s3://athena-results/
```

## Root sections

| Section | Used by | Purpose |
|---|---|---|
| `skippr` | `skippr`, `skipprd` | Workspace, state bucket, WAL/offset options, default warehouse |
| `pipelines` | `skippr`, `skipprd` | Pipeline graph: source, sink, schema sink, transforms, model settings |
| `data_sources` | `skippr`, `skipprd` | Runtime source plugin configs |
| `data_sinks` | `skippr`, `skipprd` | Runtime ingest/write sink plugin configs |
| `deadletter_sinks` | `skippr`, `skipprd` | Optional deadletter write targets |
| `schema_sinks` | `skippr`, `skipprd` | Runtime schema/catalog plugin configs |
| `warehouses` | `skippr` | Query/model/catalog provider configs |
| `dbt` | `skippr` | dbt defaults and runner settings |
| `vector_sources` | `skippr` | File/stdin sources for vector ingestion |
| `llm` | `skippr` | Model/provider defaults for data-engineering workflows |

`skipprd` ignores product-only sections such as `warehouses`, `dbt`, `vector_sources`, and `llm` when running engine commands.

## Data sinks vs warehouses

`data_sinks` are ingest destinations. They write records, manage Parquet/object layout, and coordinate schema sinks.

`warehouses` are query/model/catalog providers. They are used by `skippr query`, `skippr model`, `skippr dbt`, catalog discovery, and data-engineering workflows.

A project often configures both for the same physical system. For example, an Athena data sink writes Parquet and Glue tables, while an Athena warehouse provider runs SQL and powers modeling.

## Environment values

Whole YAML scalar values can reference environment variables:

```yaml
data_sources:
  source:
    Mssql:
      connection_string: ${MSSQL_CONNECTION_STRING}
```

Skippr loads `.env` and `.env.local` next to the config file before resolving `${VAR}` placeholders. Use this for secrets rather than committing credentials.

## Config path

By default, both binaries look for `skippr.yml` in the current directory. You can pass an explicit path:

```bash
skippr --config path/to/skippr.yml sync --pipeline my_pipeline
skipprd --config path/to/skippr.yml sync --pipeline my_pipeline
```
