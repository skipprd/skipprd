---
title: Delta Lake
description: Read a Delta table from object storage or a local path into a Skipprd pipeline.
---

# Delta Lake

Reads one Delta table at `table_uri`. Use it to pull a table another engine already wrote (Databricks, Spark, or a local Delta path).

## Before you begin

- A `s3://`, `file://`, or other URI the process can read.
- Credentials for that store (AWS env vars for S3).

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSourceDeltaLake, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "delta",
    DataSourceDeltaLake(table_uri="s3://acme-delta/events"),
)
cfg.pipeline("events", Pipeline(data_source=src))
cfg.save()
```

```bash [CLI]
skipprd connect data-source delta-lake \
  --pipeline events \
  --name delta \
  --table-uri s3://acme-delta/events
```

```yaml [YAML]
data_sources:
  delta:
    DeltaLake:
      table_uri: s3://acme-delta/events
```

:::

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `table_uri` | URI | Required | Delta table root |
| `storage_options` | map | Not set | Extra store settings (keys the Delta reader accepts) |
| `version` | integer | Not set | Read this snapshot version |
| `filter` | string | Not set | Push-down filter when the reader supports it |
| `batch_size_rows` | integer | Not set | Rows per batch |
| `format` | string | Not set | Format override |
| `batch_size_bytes` | integer | Not set | Max bytes per batch |
| `batch_size_seconds` | integer | Not set | Max seconds per batch |

## What gets synced

The current table (or `version`) becomes records. This is a snapshot read, not Databricks CDC.

## Troubleshooting

| Symptom | Fix |
|---|---|
| Table not found | Check `table_uri` includes the `_delta_log` parent |
| Access denied | Grant read on the prefix |
| Empty read | Confirm `version` still exists |

## Next steps

- [Databricks destination](/connectors/outputs/databricks)
- [S3 source](/connectors/inputs/s3)
