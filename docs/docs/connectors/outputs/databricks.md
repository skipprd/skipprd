---
title: Databricks
description: Write Skipprd pipelines into a Databricks SQL warehouse as Delta tables.
---

# Databricks

Use Databricks when analysts already query a SQL warehouse. Skipprd writes to `catalog.schema.table` using a personal access token.

## Before you begin

- Workspace URL (`https://<workspace>.cloud.databricks.com`).
- A SQL warehouse id.
- A token with rights to write the target schema.
- Optional: a `delta_table_uri` when you write to a path instead of a Unity name.

```bash
export DATABRICKS_TOKEN="dapi..."
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSinkDatabricks, EnvRef, Pipeline

cfg = Config.discover()
dbx = cfg.data_sink(
    "warehouse",
    DataSinkDatabricks(
        workspace_url="https://adb-123.azuredatabricks.net",
        token=EnvRef("DATABRICKS_TOKEN"),
        warehouse_id="abc123",
        catalog="main",
        schema="bronze",
        table="events",
    ),
)
cfg.pipeline("events", Pipeline(data_source=cfg.get_data_source("sample"), data_sink=dbx))
cfg.save()
```

```bash [CLI]
skipprd connect data-sink databricks \
  --pipeline events \
  --name warehouse \
  --workspace-url https://adb-123.azuredatabricks.net \
  --token '${DATABRICKS_TOKEN}' \
  --warehouse-id abc123 \
  --catalog main \
  --schema bronze \
  --table events
```

```yaml [YAML]
data_sinks:
  warehouse:
    Databricks:
      workspace_url: https://adb-123.azuredatabricks.net
      token: ${DATABRICKS_TOKEN}
      warehouse_id: abc123
      catalog: main
      schema: bronze
      table: events
```

:::

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `workspace_url` | URL | Required | Workspace URL |
| `token` | secret | Required | PAT as `${ENV}` |
| `warehouse_id` | string | Required | SQL warehouse id |
| `catalog` | string | Required | Unity catalog |
| `schema` | string | Required | Schema |
| `table` | string | Required | Table |
| `delta_table_uri` | URI | Not set | Write to this Delta path instead of the Unity name |
| `storage_options` | map | Not set | Extra storage settings for `delta_table_uri` |

## How data lands

Rows land in the Delta table. Retries are exactly once. New columns are added when the source schema grows.

## Troubleshooting

| Symptom | Fix |
|---|---|
| 401 / invalid token | Rotate the PAT and update the environment variable |
| Warehouse not running | Start the SQL warehouse or enable auto-start |
| Permission denied | Grant `USE` / `MODIFY` on the schema |

## Next steps

- [Exactly-once delivery](/concepts/exactly-once)
- [Datalake](/concepts/datalake)
