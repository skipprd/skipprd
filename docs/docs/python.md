---
title: Python
description: skippr Config and Session — typed config builder, discover, sync, df, and query. Same engine as the CLI.
---

# Python

`import skippr` is the engine. `Config` describes the pipelines. `Session` runs one. You get Arrow. dbt and Soda stay warehouse tools.

```bash
pip install skippr
```

The wheel and the CLI are the same engine. Connector plugins download on first use from `install.skippr.io`. The wheel ships type stubs, so editors, `mypy`, and coding agents see every plugin class, field, and doc comment.

## Session

::: code-group

```python [Python]
import skippr

cfg = skippr.Config.discover()
s = skippr.Session(cfg.get_pipeline("bikehire"))
s.doctor()
s.discover()
s.sync(once=True)
s.df()
```

```bash [CLI]
skipprd doctor
skipprd discover --pipeline bikehire --log
skipprd sync --pipeline bikehire --once --log
skipprd df --pipeline bikehire
```

:::

`Session` takes a `PipelineRef`, never a name string. `Config.discover()` loads the discovered `skippr.yml`. `Config.load(path)` loads a specific file. Another pipeline is another `Session`. There is no process-wide pipeline name.

A `Session` snapshots its pipeline's entries when it is created, with `${ENV}` references resolved. Other pipelines' entries are not read. A missing variable or a failed startup check raises `ValueError` there, not mid-sync.

## Build a config

Every plugin is a typed class named for its role: `DataSourceS3`, `DataSinkPostgres`, `SchemaSinkGlue`. Required fields are required keyword arguments. Registration returns a typed ref, and `Pipeline` takes refs, not `"data_sources.x"` strings.

```python
import skippr
from skippr import Config, DataSourceS3, LocalStorage, Pipeline

cfg = Config().workspace("bikehire").storage(LocalStorage())
src = cfg.data_source(
    "sample",
    DataSourceS3(s3_bucket="skippr-public-sample-data", s3_prefix="bike-hire"),
)
bikehire = cfg.pipeline("bikehire", Pipeline(data_source=src, sync_frequency_seconds=60))

s = skippr.Session(bikehire)
s.discover()
```

Engine state is a class too: `LocalStorage()` or `S3Storage(bucket)`. SkipprStore is `SledStore()`, `DynamoDbStore(table)`, or `CloudTablesStore(table)`.

`cfg.to_yaml()` renders the config. `cfg.save()` writes it back to the file it was loaded from; `cfg.save(path)` writes elsewhere.

## Registration merges like `skipprd connect`

Re-registering a name, `save`, and [`skipprd connect`](/cli/connect) share one merge rule. For a `Config` loaded from a file, `cfg.to_yaml()` before `save` loads to the same config as that file after it:

- Each top-level field you set replaces that field. A nested field such as `object_store` is replaced whole.
- Fields you did not set, sibling pipelines, and other entries are kept.
- An entry cannot change plugin kind. Re-registering `sample` as `DataSourceFile` when the file has `S3` is an error.
- `LocalStorage()` removes `skippr_s3_bucket`.

```python
disk = Config.discover()
disk.data_source("sample", DataSourceS3(s3_bucket="skippr-public-sample-data", s3_prefix="bike-hire-2026"))
disk.save()
```

Unknown keys outside plugin blocks are a load error. In engine settings outside plugin blocks, `${ENV}` references go in string fields only. `save` writes YAML; it refuses `.json` and `.toml` paths. Registration and `save` check the whole merged config: every entry must name a known plugin, every secret must be `${NAME}`, every pipeline with a source must validate, no two sinks may share a SkipprLake or Duckdb namespace, and every sink must pair with the schema sink it links. A plaintext secret already in the file fails registration and `save` until it is replaced.

## Secrets

Secret fields take `EnvRef`, never a string. Plaintext cannot reach `skippr.yml`.

```python
from skippr import DataSinkPostgres, EnvRef

warehouse = cfg.data_sink(
    "warehouse",
    DataSinkPostgres(
        host="localhost",
        user="skippr",
        password=EnvRef("POSTGRES_PASSWORD"),
        database="analytics",
    ),
)
cfg.pipeline("bikehire", Pipeline(data_source=src, data_sink=warehouse))
```

```bash
skipprd connect data-sink postgres \
  --pipeline bikehire \
  --name warehouse \
  --host localhost \
  --user skippr \
  --password '${POSTGRES_PASSWORD}' \
  --database analytics
```

Both write `password: ${POSTGRES_PASSWORD}`.

## Nested and tagged fields

Nested structs are frozen classes named after their plugin. A tagged field takes one class per variant. SkipprLake `object_store` on R2:

```python
from skippr import DataSinkSkipprLake, DataSinkSkipprLakeWarehouseObjectStoreR2, EnvRef

lake = cfg.data_sink(
    "lake",
    DataSinkSkipprLake(
        warehouse="s3://my-iceberg-warehouse/",
        catalog_table="my-iceberg-catalog",
        object_store=DataSinkSkipprLakeWarehouseObjectStoreR2(
            endpoint="https://<account>.r2.cloudflarestorage.com",
            access_key_id="<access-key-id>",
            secret_access_key=EnvRef("OBJECTS_SECRET_ACCESS_KEY"),
        ),
    ),
    schema_sink="lake",
)
```

String enums are `Literal` types. `DataSourceGoogleSerpRanks(keywords=[], targets=[], device="tablet")` is a type error in the editor and a `ValueError` at runtime.

## Schema sinks

`SkipprLake`, `AthenaIceberg`, and `Duckdb` share one config between the data sink and the schema sink. Pass `schema_sink=` as a name and the same config is registered under `schema_sinks.<name>`.

Other sinks link a schema sink you registered first:

```python
from skippr import DataSinkAthena, SchemaSinkGlue

glue = cfg.schema_sink(
    "glue",
    SchemaSinkGlue(
        s3_bucket="out",
        s3_prefix="bikehire",
        athena_workgroup_name="bikehire",
        athena_results_s3_bucket="out",
        glue_database_name="bikehire",
    ),
)
athena = cfg.data_sink(
    "athena",
    DataSinkAthena(
        s3_bucket="out",
        s3_prefix="bikehire",
        athena_workgroup_name="bikehire",
        athena_results_s3_bucket="out",
    ),
    schema_sink=glue,
)
cfg.pipeline("bikehire", Pipeline(data_source=src, data_sink=athena))
```

Refs belong to the `Config` that returned them. Using a ref from another `Config` is an error.

## df and query

This pipeline's views, the same ones `skipprd query` shows for it. Live WAL, unioned with Iceberg when the pipeline sink is SkipprLake, AthenaIceberg, or Duckdb. No Iceberg sink → WAL only.

```python
s.df()                          # every namespace for this pipeline
s.df("rides")                   # SELECT * FROM bikehire.rides
s.query("SELECT count(*) FROM bikehire.rides")
s.df("rides").to_pandas()
s.df().schema                   # Arrow schema
```

`df()` and `query()` return `pyarrow.Table`. Pandas is `.to_pandas()`.

`doctor()` checks config, env refs, source, sink if present, and WAL. Run it before a long sync.

## Optional sink

Leave `data_sink` out and `df()` still works. The WAL is the dataset. Add Snowflake or Postgres when you want a warehouse: register the sink, re-register the pipeline with `data_sink=`, and open a new `Session`. skipprd does not compact or reclaim that WAL until a sink exists.

## dbt and Soda

Session loads bronze. It does not run dbt.

1. `s.sync(once=True)` lands bronze (WAL, then the sink if you set one).
2. `dbt run` builds silver and gold in the warehouse.
3. `dbt test` and `soda scan` check that warehouse.

See [PostgreSQL](/getting-started/quickstart-postgres) and [Snowflake](/getting-started/quickstart-snowflake) for warehouse sinks.
