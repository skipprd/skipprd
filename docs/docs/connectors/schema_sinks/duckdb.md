---
title: DuckDB schema sink
description: Keep a local DuckDB Iceberg catalog in step with the DuckDB destination.
config_class: DataSinkDuckdb
---

# DuckDB schema

Pair this with the [DuckDB](/connectors/outputs/duckdb) destination. Both sides use the same `warehouse` and `table_namespace`.

## Before you begin

- A `file:///` warehouse directory Skipprd can write.

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSinkDuckdb, Pipeline

cfg = Config.discover()
local = cfg.data_sink(
    "local",
    DataSinkDuckdb(warehouse="file:///var/lib/skipprd/duckdb", table_namespace="bronze"),
    schema_sink="local_schema",
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

skipprd connect schema-sink duckdb \
  --pipeline bikehire \
  --name local_schema
```

```yaml [YAML]
schema_sinks:
  local_schema:
    Duckdb:
      warehouse: file:///var/lib/skipprd/duckdb
      table_namespace: bronze

data_sinks:
  local:
    Duckdb:
      warehouse: file:///var/lib/skipprd/duckdb
      table_namespace: bronze
    schema_sink: schema_sinks.local_schema
```

:::

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `warehouse` | URI | Required | Same `file:///` root as the destination |
| `table_namespace` | string | Not set | Same namespace as the destination |

## Troubleshooting

| Symptom | Fix |
|---|---|
| Catalog empty | Confirm `schema_sink` is set on the data sink and discover has run |
| Path rejected | Use an absolute `file:///` URI |

## Next steps

- [DuckDB](/connectors/outputs/duckdb)
- [skipprd query](/cli/query)
