---
title: Athena Iceberg
description: Land Skipprd pipelines as Apache Iceberg tables in S3, registered in Glue, and queryable from Athena and skipprd query.
---

# Athena Iceberg

Use Athena Iceberg when you want Iceberg tables that Athena can query: merge, replace a day, and schema evolution without rewriting Hive DDL yourself. Pair it with the [Athena Iceberg schema sink](/connectors/schema_sinks/athenaiceberg).

For Hive tables on S3, use [Athena](/connectors/outputs/athena). For a Skippr-managed catalog, use [SkipprLake](/connectors/outputs/skipprlake).

## Before you begin

- An S3 warehouse prefix Skipprd can write (`s3://bucket/path/`).
- A Glue database and an Athena workgroup.
- A results bucket name for Athena.
- IAM for S3 on the warehouse prefix plus Glue Iceberg table APIs.

```bash
export AWS_ACCESS_KEY_ID="..."
export AWS_SECRET_ACCESS_KEY="..."
export AWS_DEFAULT_REGION="us-east-1"
```

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
data_sinks:
  lake:
    AthenaIceberg:
      warehouse: s3://my-iceberg-warehouse/
      glue_database_name: analytics
      athena_workgroup_name: primary
      athena_results_s3_bucket: my-athena-results
      region: us-east-1
    schema_sink: schema_sinks.lake_schema

schema_sinks:
  lake_schema:
    AthenaIceberg:
      warehouse: s3://my-iceberg-warehouse/
      glue_database_name: analytics
      athena_workgroup_name: primary
      athena_results_s3_bucket: my-athena-results
      region: us-east-1
```

:::

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `warehouse` | URI | Required | Iceberg warehouse root (`s3://…`) |
| `glue_database_name` | string | Required | Glue database |
| `athena_workgroup_name` | string | Required | Athena workgroup |
| `athena_results_s3_bucket` | string | Required | Athena results bucket name |
| `region` | string | Not set | AWS region |
| `catalog_id` | string | Not set | Glue catalog id when it is not the account default |
| `object_store` | object | AWS S3 | Override the file store (see SkipprLake for `s3` / `r2` shapes) |

## How data lands

Each source namespace becomes an Iceberg table in `glue_database_name`. Skipprd maintains snapshots: recent commits are kept, older ones expire, and small files are rewritten. Do not rely on time travel to old snapshots.

Supports `append`, `merge_by_key`, `replace_partition`, and `replace_table`. `skipprd query` reads the Iceberg table plus in-flight WAL rows.

## Troubleshooting

| Symptom | Fix |
|---|---|
| Glue / Iceberg permission errors | Grant Iceberg table APIs on the database, not only Hive `CreateTable` |
| `AccessDenied` on the warehouse | Grant `s3:PutObject` and list on the warehouse prefix |
| Source refuses to start (`replace_partition`) | You are on Hive Athena by mistake — this page is the Iceberg destination |

## Next steps

- [Athena Iceberg schema sink](/connectors/schema_sinks/athenaiceberg)
- [Datalake](/concepts/datalake)
- [How sources land](/concepts/source-landing-semantics)
