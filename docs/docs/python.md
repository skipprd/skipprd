---
title: Python
description: skippr Session — discover, sync, df, and query. Same engine as the CLI.
---

# Python

`import skippr` is the engine. YAML describes the pipeline. `Session` runs it. You get Arrow. dbt and Soda stay warehouse tools.

```bash
pip install skippr
```

The wheel and the CLI are the same engine. Connector plugins download on first use from `install.skippr.io`.

## Session

::: code-group

```python [Python]
import skippr

s = skippr.Session(pipeline="bikehire")
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

`pipeline=` is required. `Session` auto-discovers `skippr.yml`. Another pipeline is another `Session`. There is no process-wide pipeline name.

## Connect

Module functions write root `skippr:` keys. `Session.connect()` writes plugin entries. There is no `skippr()` helper for YAML.

```python
import skippr
from skippr import DataSource, DataSink, StorageMode

(
    skippr.workspace("bikehire")
    .storage_mode(StorageMode.LOCAL)
)

s = skippr.Session(pipeline="bikehire")
(
    s.connect()
    .data_source(DataSource.S3)
    .name("sample")
    .s3_bucket("bucket")
    .s3_prefix("bike-hire")
)
s.discover()
```

Python persists when required fields are set. Secret fields must be `${ENV}` references — a Python string, quoted in the shell so the shell does not expand it.

```python
(
    s.connect()
    .data_sink(DataSink.Postgres)
    .name("warehouse")
    .host("localhost")
    .user("skippr")
    .password("${POSTGRES_PASSWORD}")
    .database("analytics")
)
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

See [`skipprd connect`](/cli/connect).

## Config without a file

Build a `Config` and pass it, or chain it onto the session. Either way it applies on run.

```python
import skippr
from skippr import StorageMode

cfg = (
    skippr.Config()
    .workspace("dev")
    .storage_mode(StorageMode.LOCAL)
    .pipelines({"bikehire": {"data_source": "data_sources.src"}})
    .data_sources({"src": {"File": {"path": "events.json"}}})
)

s = skippr.Session(pipeline="bikehire", config=cfg)
s.discover()
s.sync(once=True)
```

```python
s = skippr.Session(pipeline="bikehire").config(cfg)
s.discover()
```

## df and query

Same views as `skipprd query`. Live WAL, unioned with the Skippr datalake when that pipeline has one. No sink and no lake → WAL only.

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

Leave `data_sink` out and `df()` still works. The WAL is the dataset. Add Snowflake or Postgres when you want a warehouse — same `Session`, extra YAML. skipprd does not compact or reclaim that WAL until a sink exists.

## dbt and Soda

Session loads bronze. It does not run dbt.

1. `s.sync(once=True)` lands bronze (WAL, then the sink if you set one).
2. `dbt run` builds silver and gold in the warehouse.
3. `dbt test` and `soda scan` check that warehouse.

See [PostgreSQL](/getting-started/quickstart-postgres) and [Snowflake](/getting-started/quickstart-snowflake) for warehouse sinks.
