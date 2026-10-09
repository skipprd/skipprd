---
title: MongoDB
description: Copy a MongoDB collection into your destination, once or with change streams.
---

# MongoDB

Reads documents from one collection. Use `cdc_mode: snapshot` for a copy, or `snapshot_then_cdc` after you enable change streams.

## Before you begin

- A connection string the Skipprd host can use.
- Read access on `database.collection`.
- A replica set (or Atlas) when you use CDC — standalone nodes do not offer change streams.

```bash
export MONGODB_URI="mongodb+srv://skippr:${MONGODB_PASSWORD}@cluster.mongodb.net/"
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSourceMongodb, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "app",
    DataSourceMongodb(
        connection_string=EnvRef("MONGODB_URI"),
        database="app",
        collection="orders",
        cdc_mode="snapshot",
    ),
)
cfg.pipeline("orders", Pipeline(data_source=src))
cfg.save()
```

```bash [CLI]
skipprd connect data-source mongodb \
  --pipeline orders \
  --name app \
  --connection-string '${MONGODB_URI}' \
  --database app \
  --collection orders \
  --cdc-mode snapshot
```

```yaml [YAML]
data_sources:
  app:
    Mongodb:
      connection_string: ${MONGODB_URI}
      database: app
      collection: orders
      cdc_mode: snapshot
```

:::

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `connection_string` | secret | Required | MongoDB URI as `${ENV}` |
| `database` | string | Required | Database |
| `collection` | string | Required | Collection |
| `filter` | object | Not set | Query filter for the snapshot |
| `batch_size_rows` | integer | Not set | Documents per batch |
| `format` | string | Not set | Document format override |
| `batch_size_bytes` | integer | Not set | Max bytes per batch |
| `batch_size_seconds` | integer | Not set | Max seconds per batch |
| `cdc_mode` | string | `snapshot` | `snapshot`, `snapshot_then_cdc`, or `cdc_only` |

## What gets synced

Each document becomes a row. Nested fields are discovered as structured columns. CDC applies inserts, updates, and deletes — see [Change data capture](/cdc/).

## Troubleshooting

| Symptom | Fix |
|---|---|
| Auth failed | Check the URI user and that it can read the collection |
| Change stream error | Use a replica set / Atlas, and a CDC mode |
| Timeout | Allow the Skipprd host in Atlas network access |

## Next steps

- [Change data capture](/cdc/)
- [PostgreSQL source](/connectors/inputs/postgres)
