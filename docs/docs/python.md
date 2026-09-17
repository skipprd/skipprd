---
title: Python
description: skipprd Session — discover, sync, df, and query. Same engine as the CLI.
---

# Python

`import skipprd` is the engine. YAML describes the pipeline. `Session` runs it. You get Arrow. dbt and Soda stay warehouse tools.

```bash
pip install skipprd
```

The wheel and the CLI are the same engine. Connector plugins download on first use from `install.skippr.io`.

## Session

::: code-group

```python [Python]
import skipprd

s = skipprd.Session(config="skippr.yml", pipeline="bikehire")
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

`pipeline=` is fixed at construction. Another pipeline is another `Session`. There is no process-wide pipeline name.

## Config without a file

Constructor kwargs match `skippr.yml` fields. Secrets still come from `${ENV}` when you use YAML.

```python
import skipprd

s = skipprd.Session(
    pipeline="bikehire",
    skippr={"workspace": "dev", "skipprd_el_storage_mode": "local"},
    pipelines={"bikehire": {"data_source": "data_sources.src"}},
    data_sources={"src": {"File": {"path": "events.json"}}},
)
s.discover()
s.sync(once=True)
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
