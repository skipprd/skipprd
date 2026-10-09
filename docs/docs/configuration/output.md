---
title: Destinations
description: Choose a destination, wire it and an optional schema sink into a pipeline, and route failed records to a deadletter sink.
---

# Destinations

A destination, or data sink, is where a pipeline writes synced records: a warehouse, a lakehouse table format, object storage, or a message queue. You declare destinations under `data_sinks:` in `skippr.yml` and point a pipeline's `data_sink` at one.

Skipprd writes each batch to its write-ahead log (WAL) before the destination. A crash retries from the WAL. Whether a retry can duplicate a row depends on the destination — see [Exactly-once delivery](/concepts/exactly-once).

## Choose a destination

| Kind | Connectors |
|---|---|
| Warehouses and lakes | [Athena](/connectors/outputs/athena), [Athena Iceberg](/connectors/outputs/athenaiceberg), [BigQuery](/connectors/outputs/bigquery), [ClickHouse](/connectors/outputs/clickhouse), [Databricks](/connectors/outputs/databricks), [DuckDB](/connectors/outputs/duckdb), [MotherDuck](/connectors/outputs/motherduck), [PostgreSQL](/connectors/outputs/postgres), [Redshift](/connectors/outputs/redshift), [SkipprLake](/connectors/outputs/skipprlake), [Snowflake](/connectors/outputs/snowflake), [Synapse](/connectors/outputs/synapse) |
| Cloud storage | [Azure Blob](/connectors/outputs/azure_blob), [GCS](/connectors/outputs/gcs), [S3](/connectors/outputs/s3), [SFTP](/connectors/outputs/sftp) |
| Messaging and local | [AMQP](/connectors/outputs/amqp), [Local file](/connectors/outputs/file), [Stdout](/connectors/outputs/stdout) |

Each connector page lists its fields, how tables and files are named, and what permissions it needs.

## Add a destination to a pipeline

This example loads the `orders` pipeline from [Sources](/configuration/input) into a Postgres schema called `raw`.

1. Add the destination and wire it to the pipeline:

   ::: code-group

   ```python [Python]
   from skippr import Config, DataSinkPostgres, EnvRef, Pipeline

   cfg = Config.load("skippr.yml")
   warehouse = cfg.data_sink(
       "warehouse",
       DataSinkPostgres(
           host="warehouse.internal",
           user="skippr_loader",
           password=EnvRef("WAREHOUSE_PASSWORD"),
           database="analytics",
           schema="raw",
       ),
   )
   app_db = cfg.get_data_source("app_db")
   cfg.pipeline("orders", Pipeline(data_source=app_db, data_sink=warehouse))
   cfg.save()
   ```

   ```bash [CLI]
   skipprd connect data-sink postgres \
     --pipeline orders \
     --name warehouse \
     --host warehouse.internal \
     --user skippr_loader \
     --password '${WAREHOUSE_PASSWORD}' \
     --database analytics \
     --schema raw
   ```

   ```yaml [YAML]
   pipelines:
     orders:
       data_source: data_sources.app_db
       data_sink: data_sinks.warehouse

   data_sinks:
     warehouse:
       Postgres:
         host: warehouse.internal
         user: skippr_loader
         password: ${WAREHOUSE_PASSWORD}
         database: analytics
         schema: raw
   ```

   :::

2. Provide the secrets for both ends:

   ```bash
   export APP_DB_PASSWORD='your-source-password'
   export WAREHOUSE_PASSWORD='your-warehouse-password'
   ```

3. Run one sync:

   ::: code-group

   ```python [Python]
   import skippr

   session = skippr.Session(skippr.Config.discover().get_pipeline("orders"))
   session.sync(once=True)
   ```

   ```bash [CLI]
   skipprd sync --pipeline orders --once
   ```

   :::

## Check it worked

The run ends with `Pipeline sync complete` in the log (add `--log` to see it). Then list the tables Skipprd created in the destination:

```sql
SELECT table_name FROM information_schema.tables WHERE table_schema = 'raw';
```

How tables are named and which columns Skipprd adds differ by destination; see the "How data lands" section of the connector page.

## Schema sinks

Some destinations keep table definitions in a separate catalog. A schema sink keeps that catalog in step with the schemas Skipprd discovers. Link it from the data sink entry with `schema_sink:`, not from the pipeline.

The most common case is Athena, which reads Parquet from S3 through tables in AWS Glue:

::: code-group

```python [Python]
from skippr import Config, DataSinkAthena, Pipeline, SchemaSinkGlue

cfg = Config.load("skippr.yml")
glue = cfg.schema_sink(
    "glue",
    SchemaSinkGlue(
        s3_bucket="acme-lake",
        s3_prefix="bronze/orders",
        athena_workgroup_name="primary",
        athena_results_s3_bucket="acme-athena-results",
        glue_database_name="bronze",
    ),
)
lake = cfg.data_sink(
    "lake",
    DataSinkAthena(
        s3_bucket="acme-lake",
        s3_prefix="bronze/orders",
        athena_workgroup_name="primary",
        athena_results_s3_bucket="acme-athena-results",
    ),
    schema_sink=glue,
)
cfg.pipeline("orders", Pipeline(data_source=cfg.get_data_source("app_db"), data_sink=lake))
cfg.save()
```

```bash [CLI]
skipprd connect data-sink athena \
  --pipeline orders \
  --name lake \
  --s3-bucket acme-lake \
  --s3-prefix bronze/orders \
  --athena-workgroup-name primary \
  --athena-results-s3-bucket acme-athena-results

skipprd connect schema-sink glue \
  --pipeline orders \
  --name glue \
  --s3-bucket acme-lake \
  --s3-prefix bronze/orders \
  --athena-workgroup-name primary \
  --athena-results-s3-bucket acme-athena-results \
  --glue-database-name bronze
```

```yaml [YAML]
pipelines:
  orders:
    data_source: data_sources.app_db
    data_sink: data_sinks.lake

data_sinks:
  lake:
    schema_sink: schema_sinks.glue
    Athena:
      s3_bucket: acme-lake
      s3_prefix: bronze/orders
      athena_workgroup_name: primary
      athena_results_s3_bucket: acme-athena-results

schema_sinks:
  glue:
    Glue:
      s3_bucket: acme-lake
      s3_prefix: bronze/orders
      athena_workgroup_name: primary
      athena_results_s3_bucket: acme-athena-results
      glue_database_name: bronze
```

:::

`skipprd connect schema-sink` links the schema sink to the pipeline's existing data sink, so add the data sink first.

AthenaIceberg, SkipprLake, and DuckDB are paired: the data sink and its schema sink must hold identical settings. Python registers both from one `data_sink(...)` call, and `skipprd connect` writes both. If the two copies differ, Skipprd refuses to run. The connector pages show the pairing.

## Run without a destination

`data_sink` is optional. Without it, synced records stay in the WAL and you can query them with [`skipprd query`](/cli/query). This is useful while you explore a new source. Add a destination when you are ready to load a warehouse.

## Route failed records to a deadletter sink

A record that fails to transform or load becomes a deadletter. Without a deadletter sink, Skipprd discards deadletters and logs `Discarded <n> deadletter records because no deadletter sink is configured`. To keep them, add a `deadletter_sinks` entry and point the pipeline at it:

::: code-group

```python [Python]
from skippr import Config, DataSinkS3, Pipeline

cfg = Config.load("skippr.yml")
rejects = cfg.deadletter_sink(
    "rejects",
    DataSinkS3(s3_bucket="acme-lake", s3_prefix="deadletters/orders"),
)
cfg.pipeline(
    "orders",
    Pipeline(
        data_source=cfg.get_data_source("app_db"),
        data_sink=cfg.get_data_sink("warehouse"),
        deadletter_sink=rejects,
    ),
)
cfg.save()
```

```yaml [YAML]
pipelines:
  orders:
    data_source: data_sources.app_db
    data_sink: data_sinks.warehouse
    deadletter_sink: deadletter_sinks.rejects

deadletter_sinks:
  rejects:
    S3:
      s3_bucket: acme-lake
      s3_prefix: deadletters/orders
```

:::

`skipprd connect` does not write deadletter sinks; use YAML or Python. A deadletter sink uses the same connector types and fields as a data sink, but it must be a separate entry from the pipeline's `data_sink`. See [Deadletters](/concepts/deadletters) for what a deadletter record contains.

## Next steps

- [Transforms](/configuration/transforms) — choose tables, partitions, and sort order before data lands.
- [WAL and buffering](/configuration/buffering) — how batches reach the destination.
- [Exactly-once delivery](/concepts/exactly-once)
- [Troubleshooting](/operations/troubleshooting)
