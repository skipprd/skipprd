# Output Destination

Ingest outputs are configured under `data_sinks:` and optional `schema_sinks:` in `skippr.yml`. `data_sink` on a pipeline is optional. Leave it out to stop at the write-ahead log; skipprd does not compact or reclaim until a sink exists.

```yaml
pipelines:
  events:
    data_sink: data_sinks.landing
    schema_sink: schema_sinks.catalog

data_sinks:
  landing:
    Athena:
      s3_bucket: my-warehouse-bucket
      s3_prefix: bronze/events
      glue_database_name: bronze_events
      athena_workgroup_name: primary
      athena_results_s3_bucket: athena-results

schema_sinks:
  catalog:
    Glue:
      glue_database_name: bronze_events
```

`data_sinks` write landed data. `schema_sinks` manage destination catalog DDL where that is separate from the data sink. Query and modeling compile the same `data_sinks` plugin object.

Optional `version:` on a sink or schema block pins that connector to a specific release.

Connector reference:

- [Data sinks](../connectors/index.md#data-sinks) — Athena, Snowflake, Iceberg, S3, and others
- [Schema sinks](../connectors/schema_sinks/glue.md) — Glue, Iceberg catalog DDL
- [Connector index](../connectors/index.md) — full list

## Athena output configuration

| Variable | Default | Description |
|---|---|---|
| `DATA_OUTPUT_S3_BUCKET` | *(required)* | S3 bucket for destination Parquet files |
| `DATA_OUTPUT_S3_PREFIX` | | Key prefix for Parquet output |
| `SCHEMA_OUTPUT_GLUE_DATABASE_NAME` | *(required)* | AWS Glue database name. See [Glue schema sink](../connectors/schema_sinks/glue.md). |
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

Glue tables are created per namespace within the configured database. Hive-style partitions are registered automatically. For explicit catalog wiring, use a [Glue schema sink](../connectors/schema_sinks/glue.md).

## AWS credentials

The output uses the standard AWS credential chain, same as the input. Both the output S3 bucket and Glue catalog must be accessible with the configured credentials.

## File output

For local development, Skipprd also supports writing Parquet to local disk:

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
