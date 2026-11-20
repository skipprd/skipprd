---
title: Athena
description: Write Skipprd pipelines as Hive tables on S3, register them in Glue, and query them with Amazon Athena.
---

# Athena

Use Athena when analysts already run SQL in AWS. Skipprd writes Snappy Parquet to your bucket and registers Hive tables in Glue on the first compact. Set `glue_database_name` on this destination.

This destination writes Hive tables, not Iceberg. For Iceberg (merge, replace-partition, `skipprd query` against the lake), use [Athena Iceberg](/connectors/outputs/athenaiceberg).

## Before you begin

- An S3 bucket for Parquet and a **second** bucket (or prefix policy) for Athena query results. `athena_results_s3_bucket` is a bucket name, not an `s3://` URI.
- A Glue database name Skipprd may create.
- An Athena workgroup the role can use.
- IAM that can `s3:PutObject` on the data prefix, `s3:GetObject` / `s3:ListBucket` as needed, and Glue `CreateDatabase`, `CreateTable`, `UpdateTable`, `BatchCreatePartition`.

```bash
export AWS_ACCESS_KEY_ID="..."
export AWS_SECRET_ACCESS_KEY="..."
export AWS_DEFAULT_REGION="us-east-1"
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSinkAthena, Pipeline

cfg = Config.discover()
warehouse = cfg.data_sink(
    "warehouse",
    DataSinkAthena(
        s3_bucket="my-output-bucket",
        s3_prefix="warehouse/events",
        glue_database_name="my_database",
        athena_workgroup_name="primary",
        athena_results_s3_bucket="my-athena-results",
        region="us-east-1",
    ),
)
cfg.pipeline("events", Pipeline(data_source=cfg.get_data_source("sample"), data_sink=warehouse))
cfg.save()
```

```bash [CLI]
skipprd connect data-sink athena \
  --pipeline events \
  --name warehouse \
  --s3-bucket my-output-bucket \
  --s3-prefix warehouse/events \
  --glue-database-name my_database \
  --athena-workgroup-name primary \
  --athena-results-s3-bucket my-athena-results \
  --region us-east-1
```

```yaml [YAML]
data_sinks:
  warehouse:
    Athena:
      s3_bucket: my-output-bucket
      s3_prefix: warehouse/events
      glue_database_name: my_database
      athena_workgroup_name: primary
      athena_results_s3_bucket: my-athena-results
      region: us-east-1
```

:::

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `s3_bucket` | string | Required | Bucket for Parquet |
| `s3_prefix` | string | Required | Key prefix for tables |
| `glue_database_name` | string | Required | Glue database Skipprd may create |
| `athena_workgroup_name` | string | Required | Athena workgroup |
| `athena_results_s3_bucket` | string | Required | Bucket name for Athena results |
| `format` | string | Not set | Output format when you override the default Parquet |
| `region` | string | Not set | AWS region |
| `catalog` | string | `AwsDataCatalog` | Glue catalog name |
| `max_concurrency` | integer | Not set | Query/model only |
| `discovery_cache_ttl_secs` | integer | Not set | Query/model only |

## How data lands

Each source namespace becomes one Glue table on the first compact. Ingest alone does not create the table. Files land under `s3://<bucket>/<prefix>/<namespace>/` partitioned by day. A retry overwrites the same object path, so the final files hold each row once.

`skipprd query` on an Athena pipeline shows in-flight WAL rows. Query landed Parquet in the Athena console (or your warehouse client).

Supports `append`, `replace_partition`, and `replace_table`. It does not merge by key — use [Athena Iceberg](/connectors/outputs/athenaiceberg) or [SkipprLake](/connectors/outputs/skipprlake) for that. See [How sources land](/concepts/source-landing-semantics).

## Troubleshooting

| Symptom | Fix |
|---|---|
| `TABLE_NOT_FOUND` | Confirm `glue_database_name`, then wait for the first compact |
| `AccessDenied` on S3 | Grant `s3:PutObject` on the data prefix and results bucket |
| Glue `AccessDenied` | Grant database and table APIs on `glue_database_name` |
| Empty Athena results | Confirm `athena_results_s3_bucket` is a bucket name, not a URI |

## Next steps

- [Quickstart: S3 to Athena](/getting-started/quickstart)
- [How sources land](/concepts/source-landing-semantics)
