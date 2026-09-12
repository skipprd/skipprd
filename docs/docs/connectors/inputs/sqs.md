# SQS Input

Reads messages from an Amazon SQS queue using long polling.

## Supported formats

- Row-based JSON (each message body interpreted according to pipeline `format` settings)

## How it works

1. Long-polls the queue (20 second wait) for new messages.
2. After successful ingest, messages are deleted from the queue automatically.
3. **Batch mode (default):** processes available messages and completes when the queue is drained (for one-shot sync semantics).
4. **Stream mode:** continuously polls for new messages.
5. Namespace convention: `sqs.{queue_name}`.

## Configuration

```bash
DATA_SOURCE_PLUGIN_NAME=Sqs
SQS_QUEUE_URL=https://sqs.us-east-1.amazonaws.com/123456789012/my-queue
AWS_DEFAULT_REGION=us-east-1
```

Or via YAML pipeline config:

```yaml
data_sources:
  source:
    Sqs:
      queue_url: "https://sqs.us-east-1.amazonaws.com/123456789012/my-queue"
      region: "us-east-1"
      mode: batch
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `SQS_QUEUE_URL` | *(required)* | Full URL of the SQS queue |
| `AWS_DEFAULT_REGION` | | AWS region for the SQS client |
| `queue_url` | | Queue URL (YAML) |
| `region` | | Optional region override (YAML) |
| `mode` | `batch` | `batch` or `stream` |

## AWS credentials

SQS access uses the standard AWS credential chain.

## Namespace convention

Derived from the queue; pattern:

```
sqs.{queue_name}
```

The queue name is the final segment of the queue URL path.

## Authentication

Authentication uses the AWS default credential chain.

- `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`
- IAM roles, instance profiles, or task roles
- AWS SSO or shared config profiles

## Troubleshooting

| Symptom | Fix |
|---|---|
| AccessDenied or queue errors | Verify the AWS credential chain, queue URL, region, and SQS permissions. |
| no messages arriving | Check that producers are sending to the expected queue and that the queue is not empty. |
