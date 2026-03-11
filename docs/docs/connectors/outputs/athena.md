# Athena Output (S3 + Glue)

The primary output destination. Writes Snappy-compressed Parquet to S3 and manages tables in the AWS Glue Data Catalog, making data immediately queryable via Amazon Athena.

## What it does

1. Converts compacted WAL segments into Parquet with Snappy compression
2. Uploads Parquet via S3 multipart upload
3. Creates the Glue database if it doesn't exist
4. Creates or updates Glue tables with the discovered schema (columns, types, serde)
5. Registers Hive-style partitions for time-bucketed data

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

If Athena is used under `data_deadletters`, deadletters are written to the configured deadletter database using the pipeline name as the table name.

## Performance tuning

| Variable | Default | Description |
|---|---|---|
| `DATA_OUTPUT_MAX_ASYNC_UPLOADS` | `16` | Max concurrent multipart uploads |
| `PARQUET_MULTIPART_PART_BYTES` | `67108864` (64 MB) | Part size for multipart upload |
| `GLUE_MAX_CONCURRENCY` | `2` | Max concurrent Glue API calls |

## Glue table format

Tables are created as `EXTERNAL_TABLE` with:

- SerDe: `ParquetHiveSerDe`
- Input format: `MapredParquetInputFormat`
- Compression: Snappy
- Partition keys derived from time bucketing configuration

## AWS permissions required

The IAM identity running Skippr needs:

- `s3:PutObject`, `s3:CreateMultipartUpload`, `s3:UploadPart`, `s3:CompleteMultipartUpload`, `s3:AbortMultipartUpload` on the output bucket
- `glue:CreateDatabase`, `glue:GetDatabase` for database management
- `glue:CreateTable`, `glue:GetTable`, `glue:UpdateTable` for table management
- `glue:CreatePartition`, `glue:BatchCreatePartition`, `glue:GetPartition` for partition management
- `athena:CreateWorkGroup`, `athena:GetWorkGroup`, `athena:UpdateWorkGroup` if using Athena workgroups
