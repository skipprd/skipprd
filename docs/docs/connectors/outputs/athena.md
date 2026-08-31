# Athena Output (S3 + Glue)

The primary output destination. Writes Snappy-compressed Parquet to S3 and manages tables in the AWS Glue Data Catalog, making data immediately queryable via Amazon Athena.

## What it does

1. Converts compacted WAL segments into Parquet with Snappy compression
2. Uploads Parquet via S3 multipart upload
3. Enqueues Hive partition registration on the durable catalog outbox (`BatchCreatePartition` / `UpdatePartition`)

Table DDL is driven by a paired [Glue schema sink](../schema_sinks/glue.md). Schema sync may rebuild an empty Glue table when partition keys do not match; ingest waits until live `GetTable` layout matches the catalog intent.

## Configuration

```yaml
data_sinks:
  warehouse:
    Athena:
      s3_bucket: my-output-bucket
      s3_prefix: warehouse/events
      glue_database_name: my_database
      athena_workgroup_name: primary
      athena_results_s3_bucket: my-athena-results
      region: us-east-1
      catalog: AwsDataCatalog
```

`s3_bucket`, `s3_prefix`, `glue_database_name`, `athena_workgroup_name`, and `athena_results_s3_bucket` are ingest fields. `region`, `catalog`, `max_concurrency`, and `discovery_cache_ttl_secs` are optional query/model keys; ingest ignores them.

`athena_results_s3_bucket` is a **bucket name**, not an `s3://` URI.

Environment equivalents:

| Variable | YAML field | Description |
|---|---|---|
| `DATA_OUTPUT_S3_BUCKET` | `s3_bucket` | S3 bucket for Parquet output |
| `DATA_OUTPUT_S3_PREFIX` | `s3_prefix` | Key prefix for output objects |
| `SCHEMA_OUTPUT_GLUE_DATABASE_NAME` | `glue_database_name` | Glue database name |
| `DATA_OUTPUT_ATHENA_WORKGROUP_NAME` | `athena_workgroup_name` | Athena workgroup for queries |
| `DATA_OUTPUT_ATHENA_RESULTS_S3_BUCKET` | `athena_results_s3_bucket` | S3 bucket for Athena query results |

## S3 layout

```
s3://{DATA_OUTPUT_S3_BUCKET}/{DATA_OUTPUT_S3_PREFIX}/{namespace}/
  p_year=2025/
    p_month=03/
      p_day=04/
        {segment_id}.parquet
```

Each namespace becomes a separate Glue table within the configured database.

If Athena is used as a `deadletter_sink`, deadletters are written to the configured deadletter database using the pipeline name as the table name.

## Performance tuning

| Variable | Default | Description |
|---|---|---|
| `UPLOAD_CONCURRENCY` / `UPLOAD_CONCURRENCY_MAX` | auto-tuned (seed 16) | Concurrent multipart object uploads |
| Athena multipart part size | 16 MiB | Fixed in the Athena sink |
| `ATHENA_GLUE_CONTROL_PLANE_CONCURRENCY` / `ATHENA_GLUE_CP_MAX` | auto-tuned (seed 2) | Glue control-plane concurrency |

## Schema sink pairing

```yaml
pipelines:
  events:
    data_sink: data_sinks.landing
    schema_sink: schema_sinks.catalog

schema_sinks:
  catalog:
    Glue:
      glue_database_name: my_database
```

See [Glue schema sink](../schema_sinks/glue.md) for catalog configuration. Partition keys follow time bucketing or [source namespace contracts](../../concepts/source-landing-semantics.md) (for example `date` for [GA4](../inputs/google_analytics.md)).

## Partitioned API sources

When the source declares `replace_partition`, Athena deletes the contract partition prefix under the namespace (for example `date=2024-01-15`) before writing new Parquet. See [Source landing semantics](../../concepts/source-landing-semantics.md).

## AWS permissions required

The IAM identity running Skippr needs:

- `s3:PutObject`, `s3:CreateMultipartUpload`, `s3:UploadPart`, `s3:CompleteMultipartUpload`, `s3:AbortMultipartUpload` on the output bucket
- `glue:CreateDatabase`, `glue:GetDatabase` for database management
- `glue:GetTable`, `glue:GetPartition`, `glue:BatchCreatePartition`, `glue:UpdatePartition` for ingest-time Hive partition registration
- `glue:GetPartitions`, `glue:DeleteTable`, `glue:CreateTable`, `glue:UpdateTable` for table management and empty-table partition-layout heal
- `athena:CreateWorkGroup`, `athena:GetWorkGroup`, `athena:UpdateWorkGroup` if using Athena workgroups
