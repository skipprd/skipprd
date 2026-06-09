# Athena Output (S3 + Glue)

The primary output destination. Writes Snappy-compressed Parquet to S3 and manages tables in the AWS Glue Data Catalog, making data immediately queryable via Amazon Athena.

## What it does

1. Converts compacted WAL segments into Parquet with Snappy compression
2. Uploads Parquet via S3 multipart upload
3. Registers data in the AWS Glue Data Catalog (database, tables, partitions)

Catalog DDL can be driven by a paired [Glue schema sink](../schema_sinks/glue.md) or handled inline during ingest.

## Configuration

```bash
DATA_OUTPUT_S3_BUCKET=my-output-bucket
DATA_OUTPUT_S3_PREFIX=warehouse/events
SCHEMA_OUTPUT_GLUE_DATABASE_NAME=my_database
```

| Variable | Default | Description |
|---|---|---|
| `DATA_OUTPUT_S3_BUCKET` | *(required)* | S3 bucket for Parquet output |
| `DATA_OUTPUT_S3_PREFIX` | | Key prefix for output objects |
| `SCHEMA_OUTPUT_GLUE_DATABASE_NAME` | *(required)* | Glue database name |
| `DATA_OUTPUT_ATHENA_WORKGROUP_NAME` | | Athena workgroup for queries |
| `DATA_OUTPUT_ATHENA_RESULTS_S3_BUCKET` | | S3 bucket for Athena query results |

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
| `DATA_OUTPUT_MAX_ASYNC_UPLOADS` | `16` | Max concurrent multipart uploads |
| `PARQUET_MULTIPART_PART_BYTES` | `67108864` (64 MB) | Part size for multipart upload |
| `GLUE_MAX_CONCURRENCY` | `2` | Max concurrent Glue API calls |

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
- `glue:CreateTable`, `glue:GetTable`, `glue:UpdateTable` for table management
- `glue:CreatePartition`, `glue:BatchCreatePartition`, `glue:GetPartition` for partition management
- `athena:CreateWorkGroup`, `athena:GetWorkGroup`, `athena:UpdateWorkGroup` if using Athena workgroups
