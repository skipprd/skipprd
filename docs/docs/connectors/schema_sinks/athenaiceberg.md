---
title: Athena Iceberg schema sink
description: Keep Glue Iceberg tables in step with an Athena Iceberg destination.
config_class: DataSinkAthenaIceberg
---

# Athena Iceberg schema

Pair this with the [Athena Iceberg](/connectors/outputs/athenaiceberg) destination. The destination writes Iceberg data; this sink keeps the Glue catalog entry aligned. Use the same warehouse, database, and workgroup on both.

## Before you begin

- The Athena Iceberg destination already in `skippr.yml`.
- Glue rights to create and update Iceberg tables.

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSinkAthenaIceberg, Pipeline

cfg = Config.discover()
lake = cfg.data_sink(
    "lake",
    DataSinkAthenaIceberg(
        warehouse="s3://my-iceberg-warehouse/",
        glue_database_name="analytics",
        athena_workgroup_name="primary",
        athena_results_s3_bucket="my-athena-results",
        region="us-east-1",
    ),
    schema_sink="lake_schema",
)
cfg.pipeline("reports", Pipeline(data_source=cfg.get_data_source("sample"), data_sink=lake))
cfg.save()
```

```bash [CLI]
skipprd connect data-sink athenaiceberg \
  --pipeline reports \
  --name lake \
  --warehouse s3://my-iceberg-warehouse/ \
  --glue-database-name analytics \
  --athena-workgroup-name primary \
  --athena-results-s3-bucket my-athena-results \
  --region us-east-1

skipprd connect schema-sink athenaiceberg \
  --pipeline reports \
  --name lake_schema
```

```yaml [YAML]
schema_sinks:
  lake_schema:
    AthenaIceberg:
      warehouse: s3://my-iceberg-warehouse/
      glue_database_name: analytics
      athena_workgroup_name: primary
      athena_results_s3_bucket: my-athena-results
      region: us-east-1

data_sinks:
  lake:
    AthenaIceberg:
      warehouse: s3://my-iceberg-warehouse/
      glue_database_name: analytics
      athena_workgroup_name: primary
      athena_results_s3_bucket: my-athena-results
    schema_sink: schema_sinks.lake_schema
```

:::

## Options

Same keys as the [Athena Iceberg destination](/connectors/outputs/athenaiceberg#options).

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `warehouse` | URI | Required | Iceberg warehouse root |
| `glue_database_name` | string | Required | Glue database |
| `athena_workgroup_name` | string | Required | Athena workgroup |
| `athena_results_s3_bucket` | string | Required | Results bucket name |
| `region` | string | Not set | AWS region |
| `catalog_id` | string | Not set | Glue catalog id |
| `object_store` | object | Not set | File-store override |

## Troubleshooting

| Symptom | Fix |
|---|---|
| Catalog and data disagree | Copy the destination block into `schema_sinks` — the fields must match |
| Iceberg permission errors | Grant Iceberg table APIs, not only Hive `CreateTable` |

## Next steps

- [Athena Iceberg](/connectors/outputs/athenaiceberg)
- [Datalake](/concepts/datalake)
