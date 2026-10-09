---
title: SkipprLake
description: Land Skipprd pipelines as Iceberg tables in your object store, with a catalog table Skipprd and skipprd query both use.
---

# SkipprLake

SkipprLake is Skipprd's Iceberg destination. Files go to a warehouse URI you name. Catalog pointers live in a table you name (`catalog_table`) — DynamoDB or Skippr Cloud Tables. That table may be the same one you use as the [state store](/configuration/skippr-store).

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
| `catalog_table` | string | Required | Catalog pointer table. May be the SkipprStore table. |
| `region` | string | Not set | Region for the catalog backend |
| `object_store` | object | `s3` | File store: `file`, `s3`, or `r2` |
| `table_namespace` | string | `default` | Iceberg namespace |

Table files live at `{warehouse}/{table_namespace}/{table_name}`.

## How data lands

Supports `append`, `merge_by_key`, `replace_partition`, and `replace_table`. Skipprd expires old snapshots (recent commits are kept), rewrites small files, and folds delete files. Query results stay the same; do not use old snapshots as time travel.

`skipprd query` unions the Iceberg table with in-flight WAL rows.

## Troubleshooting

| Symptom | Fix |
|---|---|
| Catalog permission errors | Grant read/write on `catalog_table` |
| Two pipelines fight over tables | Give each destination its own `table_namespace` |
| `file://` warehouse fails | Set `object_store.type: file` and use an absolute `file:///` URI |

## Next steps

- [SkipprLake schema sink](/connectors/schema_sinks/skipprlake)
- [Datalake](/concepts/datalake)
- [How sources land](/concepts/source-landing-semantics)
