# Input Source

## DATA_SOURCE_PLUGIN_NAME

The input plugin to use for reading data.

| | |
|---|---|
| **Environment variable** | `DATA_SOURCE_PLUGIN_NAME` |
| **Required** | Yes |
| **Values** | `s3`, `file` |

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
