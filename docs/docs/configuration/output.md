# Output Destination

Skippr's primary output is Athena — writing Snappy-compressed Parquet to S3 and managing Glue catalog tables.

## Athena output configuration

| Variable | Default | Description |
|---|---|---|
| `DATA_OUTPUT_S3_BUCKET` | *(required)* | S3 bucket for destination Parquet files |
| `DATA_OUTPUT_S3_PREFIX` | | Key prefix for Parquet output |
| `SCHEMA_OUTPUT_GLUE_DATABASE_NAME` | *(required)* | AWS Glue database name. Created automatically if it doesn't exist. |
| `DATA_OUTPUT_ATHENA_WORKGROUP_NAME` | | Athena workgroup name |
| `DATA_OUTPUT_ATHENA_RESULTS_S3_BUCKET` | | S3 bucket for Athena query results |
| `DATA_OUTPUT_MAX_ASYNC_UPLOADS` | `16` | Maximum concurrent Parquet multipart uploads to S3 |
| `PARQUET_MULTIPART_PART_BYTES` | `67108864` | Multipart upload part size (64 MB default) |
| `GLUE_MAX_CONCURRENCY` | `2` | Max concurrent Glue API calls (throttling control) |

## S3 output layout

Parquet files are written to:

```
s3://{DATA_OUTPUT_S3_BUCKET}/{DATA_OUTPUT_S3_PREFIX}/{namespace}/
  p_year={YYYY}/p_month={MM}/p_day={DD}/
    {segment_id}.parquet
```

Glue tables are created per namespace within the configured database. Hive-style partitions are registered automatically.

## AWS credentials

The output uses the standard AWS credential chain, same as the input. Both the output S3 bucket and Glue catalog must be accessible with the configured credentials.

## File output

For local development, Skippr also supports writing Parquet to local disk:

| Variable | Default | Description |
|---|---|---|
| `DATA_OUTPUT_PATH` | *(required)* | Output directory for Parquet files |

## Deadletter outputs

Pipelines can optionally route deadletters to a separate output registry:

```yaml
pipelines:
  analytics:
    output: data_outputs.main
    deadletters: data_deadletters.archive
```

`data_deadletters` uses the same output plugin shapes as `data_outputs` and supports `Athena`, `S3`, and `File`.

- If `deadletters` is unset, deadletters are discarded.
- If `deadletters` points to an invalid registry entry, startup fails.
- Deadletter Athena tables are named `_dl_<pipeline>` to keep their schema isolated from the primary table.
