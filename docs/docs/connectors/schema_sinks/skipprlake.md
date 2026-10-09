---
title: SkipprLake schema sink
description: Keep SkipprLake catalog pointers in step with the SkipprLake destination.
config_class: DataSinkSkipprLake
---

# SkipprLake schema

Pair this with the [SkipprLake](/connectors/outputs/skipprlake) destination. Use the same `warehouse`, `catalog_table`, and `table_namespace` on both.

## Before you begin

- The SkipprLake destination already in `skippr.yml`.
- Write access to `catalog_table`.

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
schema_sinks:
  lake_schema:
    SkipprLake:
      warehouse: s3://my-iceberg-warehouse/
      catalog_table: my-iceberg-catalog
      region: us-east-1
      table_namespace: bronze

data_sinks:
  lake:
    SkipprLake:
      warehouse: s3://my-iceberg-warehouse/
      catalog_table: my-iceberg-catalog
      region: us-east-1
      table_namespace: bronze
    schema_sink: schema_sinks.lake_schema
```

:::

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `warehouse` | URI | Required | Same Iceberg root as the destination |
| `catalog_table` | string | Required | Same catalog table |
| `region` | string | Not set | Catalog region |
| `object_store` | object | Not set | Same file-store override as the destination |
| `table_namespace` | string | Not set | Same namespace |

## Troubleshooting

| Symptom | Fix |
|---|---|
| Tables missing from `skipprd query` | Confirm this sink is referenced and `table_namespace` matches |
| Catalog permission errors | Grant read/write on `catalog_table` |

## Next steps

- [SkipprLake](/connectors/outputs/skipprlake)
- [Datalake](/concepts/datalake)
