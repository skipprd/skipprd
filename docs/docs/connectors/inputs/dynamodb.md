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
from skippr import Config, DataSourceDynamodb, DataSourceDynamodbSourceCdcMode, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "app",
    DataSourceDynamodb(
        table_name="orders",
        region="us-east-1",
        cdc_mode=DataSourceDynamodbSourceCdcMode.SNAPSHOT,
    ),
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

Each item becomes a row. Snapshot copies the table once. CDC modes apply inserts, updates, and deletes. See [Change data capture](/cdc/).

`snapshot_then_cdc` writes a 16-byte `_skippr_order_token` (zeros for the snapshot, then the stream sequence) so a warehouse that upserts on business keys can treat the snapshot as the baseline. Dynamo items have no event-time attribute, so Skipprd sets `_skippr_event_time` to the stream `ApproximateCreationDateTime` when it is present, otherwise ingest time. Point `transform.batch_time_fields` at `_skippr_event_time` when you need day partitioning. The warehouse column is `skippr_event_time` (Skipprd strips the leading underscore).

## Troubleshooting

| Symptom | Fix |
|---|---|
| `AccessDeniedException` | Grant Scan (and stream read for CDC) |
| Stream not found | Enable DynamoDB Streams, then use a CDC mode |
| `CDC order token '' is not fixed-width hex` | The snapshot batch is missing an order token. Use `cdc_mode: snapshot_then_cdc` (not append-only) so Skipprd writes the 16-byte snapshot token. |
| Compact logs `time=` empty, or Iceberg says missing event time | Dynamo items have no timestamp. Set `batch_time_fields: _skippr_event_time`. The Iceberg column is `skippr_event_time`. |
| Wrong region | Match `region` to the table |

## Next steps

- [Change data capture](/cdc/)
- [CDC guarantees](/cdc/guarantees)
