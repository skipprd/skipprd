# EventBridge Input

Consumes AWS EventBridge events via an SQS queue target.

## How it works

1. EventBridge rule routes matching events to an SQS queue.
2. Skipprd polls the SQS queue and extracts the `detail` field from each event envelope.
3. Messages are deleted after successful ingest.
4. Namespace convention: `eventbridge.{event_bus_name}`.

## Configuration

```yaml
data_sources:
  source:
    Eventbridge:
      event_bus_name: my-bus
      sqs_queue_url: "https://sqs.us-east-1.amazonaws.com/123456/my-eb-queue"
      region: us-east-1
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `event_bus_name` | *(required)* | EventBridge bus name |
| `sqs_queue_url` | *(required)* | SQS queue URL receiving events |
| `region` | | AWS region |
| `endpoint_url` | | Custom endpoint (e.g. LocalStack) |
| `format` | `json` | Data format |

## Authentication

Authentication uses the AWS default credential chain.

- `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`
- IAM roles, instance profiles, or task roles
- AWS SSO or shared config profiles

## Troubleshooting

| Symptom | Fix |
|---|---|
| no events arriving | Verify the EventBridge rule targets the expected SQS queue and that matching events are being emitted. |
| AccessDenied | Check the AWS credential chain and queue access for the configured region. |
