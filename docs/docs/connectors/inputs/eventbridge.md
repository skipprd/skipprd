# EventBridge Input

Consumes AWS EventBridge events via an SQS queue target.

## How it works

1. EventBridge rule routes matching events to an SQS queue.
2. Skippr polls the SQS queue and extracts the `detail` field from each event envelope.
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
