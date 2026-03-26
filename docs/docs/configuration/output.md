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

## Snowflake output

Writes compacted Parquet to Snowflake via stage upload and `COPY INTO`.

| Variable | Default | Description |
|---|---|---|
| `SNOWFLAKE_ACCOUNT` | *(required)* | Snowflake account identifier |
| `SNOWFLAKE_USER` | *(required)* | Snowflake login user |
| `SNOWFLAKE_PASSWORD` | | Snowflake login password (when not using key-pair auth) |
| `SNOWFLAKE_PRIVATE_KEY_PATH` | | Path to PKCS8 PEM private key for key-pair auth |
| `SNOWFLAKE_WAREHOUSE` | *(required)* | Compute warehouse name |
| `SNOWFLAKE_DATABASE` | *(required)* | Target database |
| `SNOWFLAKE_SCHEMA` | *(required)* | Target schema |
| `SNOWFLAKE_ROLE` | | Optional role to assume |
| `SNOWFLAKE_STAGE` | `@~` | Snowflake stage for file uploads |

See the [Snowflake connector docs](../connectors/outputs/snowflake.md) for full details.

## Postgres output

Writes batches to PostgreSQL with automatic schema and table creation.

| Variable | Default | Description |
|---|---|---|
| `POSTGRES_HOST` | `localhost` | PostgreSQL host |
| `POSTGRES_PORT` | `5432` | PostgreSQL port |
| `POSTGRES_USER` | | Database user |
| `POSTGRES_PASSWORD` | | Database password |
| `POSTGRES_DATABASE` | | Target database name |
| `POSTGRES_SCHEMA` | `public` | Target schema |
| `POSTGRES_SSLMODE` | | SSL mode (e.g. `disable`, `require`, `prefer`) |

YAML equivalents: `host`, `port`, `user`, `password`, `database`, `schema`, `sslmode`, `format`. See the [Postgres connector docs](../connectors/outputs/postgres.md) for full details.

## Stdout output

Prints line-delimited JSON to standard output for debugging and piping.

| Variable | Default | Description |
|---|---|---|
| *(none)* | | Use `DATA_OUTPUT_PLUGIN_NAME=Stdout` or YAML `Stdout: {}` |

See the [Stdout connector docs](../connectors/outputs/stdout.md) for full details.

## Deadletter outputs

Pipelines can optionally route deadletters to a separate output registry:

```yaml
pipelines:
  analytics:
    data_sink: data_sinks.main
    deadletter_sink: deadletter_sinks.archive
```

`deadletter_sinks` uses the same plugin shapes as `data_sinks` and supports `Athena`, `S3`, `File`, and `Snowflake`.

- If `deadletter_sink` is unset, deadletters are discarded.
- If `deadletter_sink` points to an invalid registry entry, startup fails.
- Deadletter Athena tables are named `_dl_<pipeline>` to keep their schema isolated from the primary table.
