---
title: MotherDuck destination
description: Write Skipprd pipelines into a MotherDuck database.
---

# MotherDuck

MotherDuck is hosted DuckDB. Use it when the destination should be shareable without running DuckDB on the Skipprd host.

## Before you begin

- A MotherDuck token (`motherduck.com` → settings).
- A database name. Skipprd creates `table` in `schema` when it can.

```bash
export MOTHERDUCK_TOKEN="..."
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSinkMotherduck, EnvRef, Pipeline

cfg = Config.discover()
md = cfg.data_sink(
    "warehouse",
    DataSinkMotherduck(
        motherduck_token=EnvRef("MOTHERDUCK_TOKEN"),
        database="analytics",
        schema="bronze",
        table="events",
    ),
)
cfg.pipeline("events", Pipeline(data_source=cfg.get_data_source("sample"), data_sink=md))
cfg.save()
```

```bash [CLI]
skipprd connect data-sink motherduck \
  --pipeline events \
  --name warehouse \
  --motherduck-token '${MOTHERDUCK_TOKEN}' \
  --database analytics \
  --schema bronze \
  --table events
```

```yaml [YAML]
data_sinks:
  warehouse:
    Motherduck:
      motherduck_token: ${MOTHERDUCK_TOKEN}
      database: analytics
      schema: bronze
      table: events
```

:::

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `motherduck_token` | secret | Required | Token as `${ENV}` |
| `database` | string | Required | Database |
| `table` | string | Required | Table |
| `schema` | string | Not set | Schema (MotherDuck default if unset) |

## How data lands

Retries are exactly once. For a local file warehouse, use [DuckDB](/connectors/outputs/duckdb) instead.

## Troubleshooting

| Symptom | Fix |
|---|---|
| Auth failed | Recreate the token and export it in this shell |
| Database missing | Create it in MotherDuck, or use a token that can create databases |

## Next steps

- [MotherDuck source](/connectors/inputs/motherduck)
- [DuckDB](/connectors/outputs/duckdb)
