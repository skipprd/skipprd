---
title: Athena
description: Write Skipprd pipelines as Hive tables on S3, register them in Glue, and query them with Amazon Athena.
---

# Athena

Use Athena when analysts already run SQL in AWS. Skipprd writes Snappy Parquet to your bucket and registers Hive tables in Glue. Pair this destination with the [Glue schema sink](/connectors/schema_sinks/glue) so databases and tables exist before the first query.

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
from skippr import Config, DataSinkAthena, Pipeline, SchemaSinkGlue

cfg = Config.discover()
catalog = cfg.schema_sink(
    "catalog",
    SchemaSinkGlue(
        s3_bucket="my-output-bucket",
        s3_prefix="warehouse/events",
        glue_database_name="my_database",
        athena_workgroup_name="primary",
        athena_results_s3_bucket="my-athena-results",
    ),
)
warehouse = cfg.data_sink(
    "warehouse",
    DataSinkAthena(
        s3_bucket="my-output-bucket",
        s3_prefix="warehouse/events",
        athena_workgroup_name="primary",
        athena_results_s3_bucket="my-athena-results",
        region="us-east-1",
    ),
    schema_sink=catalog,
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
  --athena-workgroup-name primary \
  --athena-results-s3-bucket my-athena-results \
  --region us-east-1

skipprd connect schema-sink glue \
  --pipeline events \
  --name catalog \
  --s3-bucket my-output-bucket \
  --s3-prefix warehouse/events \
  --glue-database-name my_database \
  --athena-workgroup-name primary \
  --athena-results-s3-bucket my-athena-results
```

```yaml [YAML]
data_sinks:
  warehouse:
    Athena:
      s3_bucket: my-output-bucket
      s3_prefix: warehouse/events
      athena_workgroup_name: primary
      athena_results_s3_bucket: my-athena-results
      region: us-east-1
    schema_sink: schema_sinks.catalog

schema_sinks:
  catalog:
    Glue:
      s3_bucket: my-output-bucket
      s3_prefix: warehouse/events
      glue_database_name: my_database
      athena_workgroup_name: primary
      athena_results_s3_bucket: my-athena-results
```

:::

Point `data_sinks.warehouse.schema_sink` at the Glue entry. There is no pipeline-level `schema_sink` field.

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `s3_bucket` | string | Required | Bucket for Parquet |
| `s3_prefix` | string | Required | Key prefix for tables |
| `athena_workgroup_name` | string | Required | Athena workgroup |
| `athena_results_s3_bucket` | string | Required | Bucket name for Athena results |
| `format` | string | Not set | Output format when you override the default Parquet |
| `region` | string | Not set | AWS region |
| `catalog` | string | `AwsDataCatalog` | Glue catalog name |
| `max_concurrency` | integer | Not set | Query/model only |
| `discovery_cache_ttl_secs` | integer | Not set | Query/model only |

## How data lands

Each source namespace becomes one Glue table. Files land under `s3://<bucket>/<prefix>/<namespace>/` partitioned by day. A retry overwrites the same object path, so the final files hold each row once.

`skipprd query` on an Athena pipeline shows in-flight WAL rows. Query landed Parquet in the Athena console (or your warehouse client).

Supports `append`, `replace_partition`, and `replace_table`. It does not merge by key — use [Athena Iceberg](/connectors/outputs/athenaiceberg) or [SkipprLake](/connectors/outputs/skipprlake) for that. See [How sources land](/concepts/source-landing-semantics).

## Troubleshooting

| Symptom | Fix |
|---|---|
| `TABLE_NOT_FOUND` | Pair the Glue schema sink, run discover, then sync |
| `AccessDenied` on S3 | Grant `s3:PutObject` on the data prefix and results bucket |
| Glue `AccessDenied` | Grant database and table APIs on `glue_database_name` |
| Empty Athena results | Confirm `athena_results_s3_bucket` is a bucket name, not a URI |

## Next steps

- [Glue schema sink](/connectors/schema_sinks/glue)
- [Quickstart: S3 to Athena](/getting-started/quickstart)
- [How sources land](/concepts/source-landing-semantics)
