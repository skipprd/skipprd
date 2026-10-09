---
title: DynamoDB
description: Copy a DynamoDB table into your destination, once or continuously with change streams.
---

# DynamoDB

Reads items from one table. Use `cdc_mode: snapshot` for a one-off copy, or turn on DynamoDB streams and use `snapshot_then_cdc` to keep the destination in step.

## Before you begin

- IAM that can `Scan` the table. For CDC, also `DescribeStream` / `GetRecords` on the table's stream.
- Streams enabled on the table when you use a CDC mode.
- `region` matching the table.

```bash
export AWS_DEFAULT_REGION="us-east-1"
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSourceDynamodb, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "app",
    DataSourceDynamodb(table_name="orders", region="us-east-1", cdc_mode="snapshot"),
)
cfg.pipeline("orders", Pipeline(data_source=src))
cfg.save()
```

```bash [CLI]
skipprd connect data-source dynamodb \
  --pipeline orders \
  --name app \
  --table-name orders \
  --region us-east-1 \
  --cdc-mode snapshot
```

```yaml [YAML]
data_sources:
  app:
    Dynamodb:
      table_name: orders
      region: us-east-1
      cdc_mode: snapshot
```

:::

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `table_name` | string | Required | DynamoDB table |
| `region` | string | Required | AWS region |
| `endpoint_url` | URL | Not set | Local or compatible endpoint |
| `format` | string | Not set | Item format override |
| `batch_size_bytes` | integer | Not set | Max bytes per batch |
| `batch_size_seconds` | integer | Not set | Max seconds per batch |
| `cdc_mode` | string | `snapshot` | `snapshot`, `snapshot_then_cdc`, or `cdc_only` |

## What gets synced

Each item becomes a row. Snapshot copies the table once. CDC modes apply inserts, updates, and deletes — see [Change data capture](/cdc/).

## Troubleshooting

| Symptom | Fix |
|---|---|
| `AccessDeniedException` | Grant Scan (and stream read for CDC) |
| Stream not found | Enable DynamoDB Streams, then use a CDC mode |
| Wrong region | Match `region` to the table |

## Next steps

- [Change data capture](/cdc/)
- [CDC guarantees](/cdc/guarantees)
