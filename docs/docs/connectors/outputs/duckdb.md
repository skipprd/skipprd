---
title: DuckDB
description: Land Skipprd pipelines as local Iceberg tables and query them with DuckDB or skipprd query.
---

# DuckDB

Use DuckDB for local analytics, CI, and laptops. Skipprd writes Iceberg tables under `warehouse` and names them with `table_namespace`. The destination publishes those tables on the first compact.

## Before you begin

- A directory Skipprd can create, as a `file:///` URI (three slashes + absolute path).
- Enough disk for the tables plus the write-ahead log.

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSinkDuckdb, Pipeline

cfg = Config.discover()
local = cfg.data_sink(
    "local",
    DataSinkDuckdb(warehouse="file:///var/lib/skipprd/duckdb", table_namespace="bronze"),
)
cfg.pipeline("bikehire", Pipeline(data_source=cfg.get_data_source("sample"), data_sink=local))
cfg.save()
```

```bash [CLI]
skipprd connect data-sink duckdb \
  --pipeline bikehire \
  --name local \
  --warehouse file:///var/lib/skipprd/duckdb \
  --table-namespace bronze
```

```yaml [YAML]
data_sinks:
  local:
    Duckdb:
      warehouse: file:///var/lib/skipprd/duckdb
      table_namespace: bronze
```

:::

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `warehouse` | URI | Required | Iceberg root, usually `file:///…` |
| `table_namespace` | string | Not set | Iceberg namespace for tables |

## How data lands

Supports `append` and `replace_table`. It does not `replace_partition` or `merge_by_key`. Use [SkipprLake](/connectors/outputs/skipprlake) or [Athena Iceberg](/connectors/outputs/athenaiceberg) for those. Retries are exactly once. Tables exist after the first compact. `skipprd query` reads the local tables plus in-flight WAL rows.

## Troubleshooting

| Symptom | Fix |
|---|---|
| Warehouse path rejected | Use an absolute `file:///` URI |
| Source refuses `replace_partition` | Point that source at Iceberg in S3, or set `write_policy: append` when the source allows it |
| Disk full | Free space on `warehouse` and `DATA_DIR` |

## Next steps

- [Streamhouse](/concepts/datalake)
- [skipprd query](/cli/query)
