# Kinesis Input

Reads records from an Amazon Kinesis Data Stream.

## Supported formats

- Row-based JSON (each record payload interpreted according to pipeline `format` settings)

## How it works

1. Discovers all shards for the configured stream.
2. Reads starting from `TRIM_HORIZON` for each shard.
3. Maintains a checkpoint per shard for resumable ingestion.
4. **Batch mode (default):** drains each shard to the tip and completes.
5. **Stream mode:** continuously polls with adaptive backoff between 100ms and 5s.
6. Namespace convention: `kinesis.{stream_name}`.

## Configuration

```bash
DATA_SOURCE_PLUGIN_NAME=Kinesis
KINESIS_STREAM_NAME=my-stream
AWS_DEFAULT_REGION=us-east-1
```

Or via YAML pipeline config:

```yaml
data_sources:
  source:
    Kinesis:
      stream_name: "my-stream"
      region: "us-east-1"
      mode: batch
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `KINESIS_STREAM_NAME` | *(required)* | Kinesis Data Stream name |
| `AWS_DEFAULT_REGION` | | AWS region for the Kinesis client |
| `stream_name` | | Stream name (YAML) |
| `region` | | Optional region override (YAML) |
| `mode` | `batch` | `batch` (drain to tip) or `stream` (continuous with adaptive backoff) |

## AWS credentials

Kinesis access uses the standard AWS credential chain.

## Namespace convention

```
kinesis.{stream_name}
```

## Authentication

Authentication uses the AWS default credential chain.

- `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`
- IAM roles, instance profiles, or task roles
- AWS SSO or shared config profiles

## Troubleshooting

| Symptom | Fix |
|---|---|
| AccessDenied or stream errors | Verify the AWS credential chain, stream name, region, and Kinesis permissions. |
| no records arriving | Check that producers are writing to the expected stream and shard activity is present. |
