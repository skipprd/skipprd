---
title: ClickHouse destination
description: Insert Skipprd batches into a ClickHouse table. Retries can duplicate rows — deduplicate downstream.
---

# ClickHouse

Use ClickHouse when that is already your analytics store. Skipprd inserts batches into `database.table`. A retry can write a batch again, so plan a unique key or a downstream dedup. See [Exactly-once delivery](/concepts/exactly-once).

## Before you begin

- A HTTP(S) URL for the ClickHouse service (`https://host:8443`).
- A database and a user that can insert (and create the table if it does not exist).

```bash
export CLICKHOUSE_PASSWORD="change-me"
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSinkClickhouse, EnvRef, Pipeline

cfg = Config.discover()
ch = cfg.data_sink(
    "warehouse",
    DataSinkClickhouse(
        url="https://clickhouse.internal:8443",
        database="analytics",
        user="skippr",
        password=EnvRef("CLICKHOUSE_PASSWORD"),
        table="events",
    ),
)
cfg.pipeline("events", Pipeline(data_source=cfg.get_data_source("sample"), data_sink=ch))
cfg.save()
```

```bash [CLI]
skipprd connect data-sink clickhouse \
  --pipeline events \
  --name warehouse \
  --url https://clickhouse.internal:8443 \
  --database analytics \
  --user skippr \
  --password '${CLICKHOUSE_PASSWORD}' \
  --table events
```

```yaml [YAML]
data_sinks:
  warehouse:
    Clickhouse:
      url: https://clickhouse.internal:8443
      database: analytics
      user: skippr
      password: ${CLICKHOUSE_PASSWORD}
      table: events
```

:::

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `url` | URL | Required | ClickHouse HTTP(S) endpoint |
| `database` | string | Required | Database |
| `user` | string | Required | User |
| `password` | secret | Not set | Password as `${ENV}` |
| `table` | string | Required | Table name |

## How data lands

Batches are inserted into `table`. Deduplicate on your primary key if a crash retries a batch.

## Troubleshooting

| Symptom | Fix |
|---|---|
| Connection refused | Check `url` and TLS |
| Authentication failed | Export the password variable in this shell |
| Table missing | Grant `CREATE` or create the table first |

## Next steps

- [ClickHouse source](/connectors/inputs/clickhouse)
- [Exactly-once delivery](/concepts/exactly-once)
