---
title: Python
description: Build a typed Skipprd config in Python, then discover, sync, and query a pipeline with Session. Same engine and skippr.yml as the CLI.
---

# Python

The `skippr` package runs the Skipprd engine inside Python. You build a pipeline with `Config`, run it with `Session`, and read the results as Arrow tables. It is the same engine as the `skipprd` command, and both read and write the same `skippr.yml`, so you can build a config in a notebook and run it from cron with the CLI.

## Before you begin

- Python 3.10 or later on **macOS arm64** or **Linux x86_64**.
- Install the package, and the pandas extra if you want `.to_pandas()`:

```bash
pip install skippr
pip install "skippr[pandas]"   # optional
python -c "import skippr; print(skippr.Session)"
```

The package ships type stubs, so your editor, `mypy`, and coding assistants see every connector class, field, and docstring. Connectors download on first use from `install.skippr.io`. See [Install](/getting-started/install) for details.

## Run a pipeline

`Session` binds the engine to one pipeline. Each Python call has a CLI equivalent.

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
skipprd discover --pipeline bikehire
skipprd sync --pipeline bikehire --once
skipprd df --pipeline bikehire
```

:::

- `Config.discover()` loads `skippr.yml` from the current directory, or starts an empty config that saves there. `Config.load(path)` loads a specific file.
- `Session` takes a `PipelineRef` from a `Config`, never a name string. To run another pipeline, open another `Session`.
- `discover()` samples the source and records the schema. `sync(once=True)` runs one pass and returns. `sync()` keeps running, one pass every `sync_frequency_seconds` (900 by default).

When you create a `Session`, it takes a snapshot of its pipeline's config and resolves every `${NAME}` reference from the environment, including a `.env` file next to `skippr.yml`. A missing variable raises `ValueError` there, before any data moves, not halfway through a sync.

## Build a config

Every connector is a typed class named for its role: `DataSourceS3`, `DataSinkPostgres`, `SchemaSinkGlue`. Required fields are required keyword arguments. Registering a connector returns a typed ref, and `Pipeline` takes refs, not `"data_sources.x"` strings, so a typo is an error in your editor.

::: code-group

```python [Python]
import skippr
from skippr import Config, DataSourceS3, LocalStorage, Pipeline

cfg = Config().workspace("bikehire").storage(LocalStorage())
src = cfg.data_source(
    "sample",
    DataSourceS3(s3_bucket="skippr-public-sample-data", s3_prefix="bike-hire"),
)
bikehire = cfg.pipeline("bikehire", Pipeline(data_source=src, sync_frequency_seconds=60))
cfg.save("skippr.yml")

s = skippr.Session(bikehire)
s.discover()
```

```bash [CLI]
skipprd --workspace bikehire --storage-mode local connect data-source s3 \
  --pipeline bikehire \
  --name sample \
  --s3-bucket skippr-public-sample-data \
  --s3-prefix bike-hire

skipprd discover --pipeline bikehire
```

```yaml [YAML]
skippr:
  workspace: bikehire
  skipprd_el_storage_mode: local

pipelines:
  bikehire:
    data_source: data_sources.sample
    sync_frequency_seconds: 60

data_sources:
  sample:
    S3:
      s3_bucket: skippr-public-sample-data
      s3_prefix: bike-hire
```

:::

Where Skipprd keeps its own state is a class too:

- `LocalStorage()` keeps state in the local data directory (`./data` by default). Good for trying things out and single-machine runs.
- `S3Storage("your-state-bucket")` keeps state in an S3 bucket instead of on local disk.

`cfg.store(...)` chooses where read progress is tracked: `SledStore()` (the local on-disk state store, the default) or `DynamoDbStore("table")`. See [State store](/configuration/skippr-store).

`cfg.to_yaml()` shows the config as YAML without writing it. `cfg.save()` writes back to the file the config was loaded from; `cfg.save(path)` writes somewhere else. `save` writes YAML only and refuses `.json` and `.toml` paths.

## Update an existing config

Registering a name again, `save`, and [`skipprd connect`](/cli/connect) all merge the same way, so Python and the CLI never fight over a file:

- Each top-level field you set replaces that field. A nested field such as `object_store` is replaced whole.
- Fields you did not set, other pipelines, and other entries are kept.
- An entry cannot change connector type. Re-registering `sample` as `DataSourceFile` when the file has `S3` is an error.
- `LocalStorage()` removes any `skippr_s3_bucket`.

::: code-group

```python [Python]
from skippr import Config, DataSourceS3

disk = Config.discover()
disk.data_source("sample", DataSourceS3(s3_bucket="skippr-public-sample-data", s3_prefix="bike-hire-2026"))
disk.save()
```

```bash [CLI]
skipprd connect data-source s3 \
  --pipeline bikehire \
  --name sample \
  --s3-bucket skippr-public-sample-data \
  --s3-prefix bike-hire-2026
```

:::

Registration and `save` check the whole merged config, so you find problems when you write the file rather than when a pipeline runs:

- every entry names a known connector, and unknown keys outside connector blocks are an error;
- every secret is a `${NAME}` reference — a plaintext secret already in the file blocks `save` until you replace it;
- every pipeline with a source is valid;
- no two SkipprLake or DuckDB destinations write to the same table namespace;
- every destination matches the schema sink it links to.

## Keep secrets out of the file

Secret fields accept only `EnvRef`, never a plain string, so a password cannot reach `skippr.yml`. Export the variable before you create the `Session`:

```bash
export POSTGRES_PASSWORD="your-password"
```

::: code-group

```python [Python]
from skippr import Config, DataSinkPostgres, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.get_data_source("sample")
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
cfg.save()
```

```bash [CLI]
skipprd connect data-sink postgres \
  --pipeline bikehire \
  --name warehouse \
  --host localhost \
  --user skippr \
  --password '${POSTGRES_PASSWORD}' \
  --database analytics
```

```yaml [YAML]
data_sinks:
  warehouse:
    Postgres:
      host: localhost
      user: skippr
      password: ${POSTGRES_PASSWORD}
      database: analytics
```

:::

All three write `password: ${POSTGRES_PASSWORD}`. Path-like fields typed as `str`, such as Snowflake `private_key_path`, take the `"${NAME}"` string directly.

## Nested and tagged fields

Nested settings are their own frozen classes, named after their connector. A field that accepts one of several shapes takes one class per variant. For example, a SkipprLake destination whose files live in Cloudflare R2:

```python
from skippr import Config, DataSinkSkipprLake, DataSinkSkipprLakeWarehouseObjectStoreR2, EnvRef

cfg = Config.discover()
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

Fields with a fixed set of values are `Literal` types. `DataSourceGoogleSerpRanks(keywords=[], targets=[], device="tablet")` is a type error in your editor and a `ValueError` at runtime, because `device` accepts only `"desktop"` or `"mobile"`.

## Schema sinks

A schema sink is where Skipprd writes table definitions. How you attach one depends on the destination.

`SkipprLake`, `AthenaIceberg`, and `Duckdb` use one config for both roles. Pass `schema_sink=` a name, and the same config is registered under `schema_sinks.<name>`, as in the example above.

Other destinations link a schema sink you registered first. Athena with Glue:

```python
from skippr import Config, DataSinkAthena, Pipeline, SchemaSinkGlue

cfg = Config.discover()
src = cfg.get_data_source("sample")
glue = cfg.schema_sink(
    "glue",
    SchemaSinkGlue(
        s3_bucket="your-output-bucket",
        s3_prefix="bikehire",
        athena_workgroup_name="primary",
        athena_results_s3_bucket="your-output-bucket",
        glue_database_name="bikehire",
    ),
)
athena = cfg.data_sink(
    "athena",
    DataSinkAthena(
        s3_bucket="your-output-bucket",
        s3_prefix="bikehire",
        athena_workgroup_name="primary",
        athena_results_s3_bucket="your-output-bucket",
    ),
    schema_sink=glue,
)
cfg.pipeline("bikehire", Pipeline(data_source=src, data_sink=athena))
cfg.save()
```

Refs belong to the `Config` that returned them. Passing a ref from one `Config` to another is an error.

## Read data with df and query

`df()` and `query()` read the data Skipprd holds for this pipeline — the same views [`skipprd query`](/cli/query) shows. They read the WAL, combined with the destination's Iceberg tables when the destination is SkipprLake, AthenaIceberg, or DuckDB. For any other destination they read the WAL only.

::: code-group

```python [Python]
import skippr

s = skippr.Session(skippr.Config.discover().get_pipeline("bikehire"))
s.df()                                          # every namespace in this pipeline
s.df("rides")                                   # one namespace: bikehire.rides
s.query("SELECT count(*) FROM bikehire.rides")  # any SQL over this pipeline's views
s.df().schema                                   # the Arrow schema
s.df("rides").to_pandas()                       # needs skippr[pandas]
```

```bash [CLI]
skipprd df --pipeline bikehire
skipprd df --pipeline bikehire --namespace rides
skipprd query --sql "SELECT count(*) FROM bikehire.rides"
```

:::

A **namespace** is one table's worth of records. By default a pipeline has one namespace, named after the pipeline; [transforms](/configuration/transforms) can split records into several. The examples assume a transform has split out a `rides` namespace. Both methods return a `pyarrow.Table`.

## Check a config before a long run

`doctor()` (or `skipprd doctor`) checks that the config is complete and consistent: at least one pipeline and source, every reference points at an entry that exists, the storage mode, and the whole-config rules above. It returns a dict with `ok` and a list of `checks`. It does not connect to your source or destination; `discover()` is the first call that does.

```python
import skippr

s = skippr.Session(skippr.Config.discover().get_pipeline("bikehire"))
result = s.doctor()
if not result["ok"]:
    for check in result["checks"]:
        print(check)
```

## Run without a destination

Leave out `data_sink` and the pipeline still syncs. The WAL is the dataset, and `df()` and `query()` read it. When you want a warehouse, register the destination, register the pipeline again with `data_sink=`, and open a new `Session`. Skipprd does not compact or reclaim the WAL until the pipeline has a destination, so plan disk space for long WAL-only runs.

## dbt and Soda

`Session` loads raw (bronze) data. It does not run dbt.

1. `s.sync(once=True)` lands bronze data in the WAL, and in the destination if you set one.
2. `dbt run` builds your silver and gold models in the warehouse.
3. `dbt test` and `soda scan` check the results in that warehouse.

## Next steps

- [Quickstart: PostgreSQL](/getting-started/quickstart-postgres) or [Snowflake](/getting-started/quickstart-snowflake) — land data in a warehouse from Python.
- [`skippr.yml` reference](/configuration/skippr-yml) — every engine and pipeline setting.
- [Data sources](/connectors/) — the connector classes and their options.
- [Transforms](/configuration/transforms) — namespaces, partitions, and flattening.
- [How Skipprd works](/concepts/how-it-works) — what happens between source and destination.
