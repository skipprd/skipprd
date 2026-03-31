# SNS Input

Consumes AWS SNS messages via an SQS subscription.

## How it works

1. SNS topic is subscribed to an SQS queue.
2. Skippr polls the SQS queue and extracts the `Message` field from the SNS envelope.
3. Messages are deleted after successful ingest.
4. Namespace convention: `sns.{topic_name}`.

## Configuration

```yaml
data_sources:
  source:
    Sns:
      topic_arn: "arn:aws:sns:us-east-1:123456:my-topic"
      sqs_queue_url: "https://sqs.us-east-1.amazonaws.com/123456/my-sns-queue"
      region: us-east-1
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `topic_arn` | *(required)* | SNS topic ARN |
| `sqs_queue_url` | *(required)* | SQS queue URL subscribed to the topic |
| `region` | | AWS region |
| `endpoint_url` | | Custom endpoint (e.g. LocalStack) |
| `format` | `json` | Data format |
