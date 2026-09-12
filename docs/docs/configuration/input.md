# Input Source

Input connectors are configured under `data_sources:` in `skippr.yml`.

```yaml
pipelines:
  events:
    data_source: data_sources.events

data_sources:
  events:
    S3:
      s3_bucket: my-source-bucket
      s3_prefix: events/
```

Environment variables are best kept for secrets and deployment overrides.

## Environment overrides

## DATA_SOURCE_PLUGIN_NAME

The input connector to use for reading data.

| | |
|---|---|
| **Environment variable** | `DATA_SOURCE_PLUGIN_NAME` |
| **Required** | Yes |
| **Values** | Connector-specific. See the input connector reference for the currently supported runtime plugins. |

In YAML config, each connector block can also include an optional `version` field to pin a published runtime plugin version instead of following the latest registry entry.

## S3 source options

| Variable | Default | Description |
|---|---|---|
| `DATA_SOURCE_S3_BUCKET` | *(required)* | Source S3 bucket name |
| `DATA_SOURCE_S3_PREFIX` | | Key prefix to filter source objects |
| `DATA_SOURCE_S3_DELIMITER` | `/` | S3 delimiter for listing |
| `DATA_SOURCE_S3_PREFIX_ORDERED_DEPTH` | `0` | Depth for ordered prefix scanning |
| `DATA_SOURCE_BATCH_SIZE_BYTES` | `1024000` | Batch size in bytes per read |
| `DATA_SOURCE_BATCH_SIZE_SECONDS` | `600` | Max seconds per batch |

### AWS credentials

S3 access uses the standard AWS credential chain:

- `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`
- `AWS_DEFAULT_REGION` (required)
- Or an instance profile / IAM role when running on EC2/ECS

## File source options

| Variable | Default | Description |
|---|---|---|
| `DATA_SOURCE_PATH` | *(required)* | Path to the directory or file to ingest |

Supports JSON, CSV, and Parquet input files.

## MSSQL source options

| Variable | Default | Description |
|---|---|---|
| `MSSQL_CONNECTION_STRING` | *(required)* | ADO.NET-style connection string for MSSQL |

Additional MSSQL-specific options (available in YAML config): `tables`, `batch_size_rows`, `query_timeout_seconds`. See the [MSSQL connector docs](../connectors/inputs/mssql.md) for details.

## MySQL source options

| Variable | Default | Description |
|---|---|---|
| `MYSQL_CONNECTION_STRING` | *(required)* | MySQL connection string (`mysql_async`) |

Additional MySQL-specific options (YAML): `tables`, `batch_size_rows`, `batch_size_bytes`, `batch_size_seconds`. See the [MySQL connector docs](../connectors/inputs/mysql.md) for details.

## DynamoDB source options

| Variable | Default | Description |
|---|---|---|
| `DYNAMODB_TABLE_NAME` | *(required)* | DynamoDB table to scan |
| `AWS_DEFAULT_REGION` | | AWS region for DynamoDB |

Optional YAML: `table_name`, `region`. See the [DynamoDB connector docs](../connectors/inputs/dynamodb.md) for details.

## Kinesis source options

| Variable | Default | Description |
|---|---|---|
| `KINESIS_STREAM_NAME` | *(required)* | Kinesis Data Stream name |
| `AWS_DEFAULT_REGION` | | AWS region for Kinesis |

Optional YAML: `stream_name`, `region`, `mode` (`batch` or `stream`). See the [Kinesis connector docs](../connectors/inputs/kinesis.md) for details.

## SQS source options

| Variable | Default | Description |
|---|---|---|
| `SQS_QUEUE_URL` | *(required)* | Full URL of the SQS queue |
| `AWS_DEFAULT_REGION` | | AWS region for SQS |

Optional YAML: `queue_url`, `region`, `mode` (`batch` or `stream`). See the [SQS connector docs](../connectors/inputs/sqs.md) for details.

## HTTP source options

| Variable | Default | Description |
|---|---|---|
| `DATA_SOURCE_HTTP_URL` | *(required)* | URL to download via HTTP GET |

Optional YAML: `url`, `format`, `batch_size_bytes`, `batch_size_seconds`. Gzip-compressed responses are supported. See the [HTTP Client connector docs](../connectors/inputs/http_client.md) and [HTTP Server connector docs](../connectors/inputs/http_server.md) for details.

## Stdin source options

| Variable | Default | Description |
|---|---|---|
| *(none required)* | | Reads from standard input |

Optional YAML: `mode` (`batch`, read until EOF — default; `stream`, continuous) and `format`. Typical use: pipe data into Skipprd, e.g. `cat data.json | skipprd sync --pipeline my_pipeline`. See the [Stdin connector docs](../connectors/inputs/stdin.md) for details.
