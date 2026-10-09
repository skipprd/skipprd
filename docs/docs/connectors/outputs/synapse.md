---
title: Azure Synapse
description: Write Skipprd pipelines into Azure Synapse dedicated SQL using a connection string.
---

# Synapse

Use Synapse when the destination is a dedicated SQL pool you already run. Skipprd inserts into `schema.table`.

## Before you begin

- A connection string the Skipprd host can use (SQL auth or AAD, as your pool allows).
- Store it in the environment.

```bash
export SYNAPSE_CONNECTION_STRING="Server=tcp:....sql.azuresynapse.net,1433;..."
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSinkSynapse, EnvRef, Pipeline

cfg = Config.discover()
syn = cfg.data_sink(
    "warehouse",
    DataSinkSynapse(
        connection_string=EnvRef("SYNAPSE_CONNECTION_STRING"),
        schema="dbo",
        table="events",
    ),
)
cfg.pipeline("events", Pipeline(data_source=cfg.get_data_source("sample"), data_sink=syn))
cfg.save()
```

```bash [CLI]
skipprd connect data-sink synapse \
  --pipeline events \
  --name warehouse \
  --connection-string '${SYNAPSE_CONNECTION_STRING}' \
  --schema dbo \
  --table events
```

```yaml [YAML]
data_sinks:
  warehouse:
    Synapse:
      connection_string: ${SYNAPSE_CONNECTION_STRING}
      schema: dbo
      table: events
```

:::

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `connection_string` | secret | Required | Synapse connection string as `${ENV}` |
| `schema` | string | Required | Schema |
| `table` | string | Required | Table |

## How data lands

Rows are inserted into `schema.table`. Retries are exactly once. Create the table first if the user cannot `CREATE TABLE`.

## Troubleshooting

| Symptom | Fix |
|---|---|
| Login failed | Refresh the connection string and export it in this shell |
| Firewall | Allow the Skipprd host IP on the Synapse firewall |
| Table missing | Create it or grant `CREATE` |

## Next steps

- [Exactly-once delivery](/concepts/exactly-once)
- [SQL Server source](/connectors/inputs/mssql)
