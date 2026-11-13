---
title: SkipprLake
description: Land Skipprd pipelines as Iceberg tables in your object store, with a catalog table Skipprd and skipprd query both use.
---

# SkipprLake

SkipprLake is Skipprd's Iceberg destination. Files go to a warehouse URI you name. Catalog pointers live in a DynamoDB table you name (`catalog_table`). That lake catalog is not [Skippr Store](/configuration/skippr-store): never set `skippr.store.type` to select it.

Pair it with the [SkipprLake schema sink](/connectors/schema_sinks/skipprlake).

## Before you begin

- A warehouse URI: `s3://bucket/path/` or `file:///absolute/path/` for local files.
- A catalog table name, and credentials for that backend.
- Unique `(catalog_table, table_namespace)` per SkipprLake destination. Two pipelines must not share that pair.

```bash
export AWS_ACCESS_KEY_ID="..."
export AWS_SECRET_ACCESS_KEY="..."
export AWS_DEFAULT_REGION="us-east-1"
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSinkSkipprLake, Pipeline

cfg = Config.discover()
lake = cfg.data_sink(
    "lake",
    DataSinkSkipprLake(
        warehouse="s3://my-iceberg-warehouse/",
        catalog_table="my-iceberg-catalog",
        region="us-east-1",
        table_namespace="bronze",
    ),
    schema_sink="lake_schema",
)
cfg.pipeline("reports", Pipeline(data_source=cfg.get_data_source("sample"), data_sink=lake))
cfg.save()
```

```bash [CLI]
skipprd connect data-sink skipprlake \
  --pipeline reports \
  --name lake \
  --warehouse s3://my-iceberg-warehouse/ \
  --catalog-table my-iceberg-catalog \
  --region us-east-1 \
  --table-namespace bronze

skipprd connect schema-sink skipprlake \
  --pipeline reports \
  --name lake_schema
```

```yaml [YAML]
data_sinks:
  lake:
    SkipprLake:
      warehouse: s3://my-iceberg-warehouse/
      catalog_table: my-iceberg-catalog
      region: us-east-1
      table_namespace: bronze
    schema_sink: schema_sinks.lake_schema

schema_sinks:
  lake_schema:
    SkipprLake:
      warehouse: s3://my-iceberg-warehouse/
      catalog_table: my-iceberg-catalog
      region: us-east-1
      table_namespace: bronze
```

:::

Local files:

```yaml
object_store:
  type: file
# warehouse must be file:///...
```

Cloudflare R2 (or another path-style S3 store):

```yaml
object_store:
  type: r2
  endpoint: ${OBJECTS_S3_ENDPOINT}
  region: auto
  access_key_id: ${OBJECTS_ACCESS_KEY_ID}
  secret_access_key: ${OBJECTS_SECRET_ACCESS_KEY}
  path_style: true
```

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `warehouse` | URI | Required | Iceberg root (`s3://…` or `file:///…`) |
| `catalog_table` | string | Required | DynamoDB table for Iceberg catalog pointers. Not Skippr Store. |
| `region` | string | Not set | Region for the catalog backend |
| `object_store` | object | `s3` | File store: `file`, `s3`, or `r2` |
| `table_namespace` | string | `default` | Iceberg namespace |

Table files live at `{warehouse}/{table_namespace}/{table_name}`.

## How data lands

Supports `append`, `merge_by_key`, `replace_partition`, and `replace_table`. Skipprd expires old snapshots (recent commits are kept), rewrites small files, and folds delete files. Query results stay the same; do not use old snapshots as time travel.

With `transform.batch_time_fields` and `transform.batch_time_unit`, Skipprd adds one hidden Iceberg time transform (`year`, `month`, `day`, or `hour`) on the event-time column, plus identity partitions from `batch_partition_fields`. It does not write `year`/`month`/`day` int columns. `minute` is rejected (Iceberg has no `minute()`). POSIX millis and micros in numeric columns resolve as event time, then the transform runs on the timestamp. Deadletter Iceberg tables stay unpartitioned. Filter with `WHERE event_ts >= TIMESTAMP '2026-11-07 00:00:00' AND event_ts < TIMESTAMP '2026-11-08 00:00:00'`. Recreate an existing Iceberg table if the live spec still identity-partitions calendar ints.

`skipprd query` returns the current table (Iceberg plus uncompacted WAL, CDC applied).

## Troubleshooting

| Symptom | Fix |
|---|---|
| Catalog permission errors | Grant read/write on `catalog_table` |
| Two pipelines fight over tables | Give each destination its own `table_namespace` |
| `file://` warehouse fails | Set `object_store.type: file` and use an absolute `file:///` URI |

## Next steps

- [ALTER operations](/operations/alter) — rename, merge, drop, and widen columns after the table exists
- [SkipprLake schema sink](/connectors/schema_sinks/skipprlake)
- [Streamhouse](/concepts/datalake)
- [How sources land](/concepts/source-landing-semantics)
