---
title: SQS source
description: Drain or continuously poll an Amazon SQS queue with Skipprd, deleting each message only after it is durable.
---

# SQS source

The SQS source long-polls an Amazon SQS queue and lands each message body as one or more records. By default it drains the queue and finishes, which suits scheduled loads; set `mode: stream` to keep polling. Skipprd deletes messages only after they are safely in its write-ahead log (WAL), so a crash never loses a message.

To consume SNS topics or EventBridge events, use the [SNS source](/connectors/inputs/sns) or [EventBridge source](/connectors/inputs/eventbridge). They read from an SQS queue too, and unwrap the AWS envelope for you.

## Before you begin

You need:

- The queue URL, for example `https://sqs.us-east-1.amazonaws.com/123456789012/orders`.
- AWS credentials that Skipprd can find through the standard AWS credential chain: environment variables (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`), a shared profile, or an instance, task, or pod role.
- An IAM policy that allows Skipprd to receive and delete messages:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": ["sqs:ReceiveMessage", "sqs:DeleteMessage"],
      "Resource": "arn:aws:sqs:us-east-1:123456789012:orders"
    }
  ]
}
```

If the queue is encrypted with a customer-managed KMS key, also allow `kms:Decrypt` on that key.

## Configure

Any standard AWS credential source works. For example, use a named profile:

```bash
export AWS_PROFILE=analytics
```

::: code-group

```python [Python]
from skippr import Config, DataSourceSqs, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "orders_queue",
    DataSourceSqs(
        queue_url="https://sqs.us-east-1.amazonaws.com/123456789012/orders",
        region="us-east-1",
    ),
)
cfg.pipeline("orders", Pipeline(data_source=src))
cfg.save()
```

```bash [CLI]
skipprd connect data-source sqs \
  --pipeline orders \
  --name orders_queue \
  --queue-url https://sqs.us-east-1.amazonaws.com/123456789012/orders \
  --region us-east-1
```

```yaml [YAML]
pipelines:
  orders:
    data_source: data_sources.orders_queue

data_sources:
  orders_queue:
    Sqs:
      queue_url: "https://sqs.us-east-1.amazonaws.com/123456789012/orders"
      region: us-east-1
```

:::

Run the pipeline and check the result:

1. Send a few test messages. The first run discovers the schema from live messages (see [What gets synced](#what-gets-synced)).

   ```bash
   aws sqs send-message \
     --queue-url https://sqs.us-east-1.amazonaws.com/123456789012/orders \
     --message-body '{"order_id": 1001, "status": "paid"}'
   ```

2. Drain the queue once:

   ```bash
   skipprd sync --pipeline orders --once --log
   ```

3. Read what landed:

   ```bash
   skipprd df --pipeline orders
   ```

To load the data into a warehouse, add a destination to the pipeline. See [Destinations](/configuration/output).

## Options

| Key | Type | Default | Description |
|---|---|---|---|
| `queue_url` | string | Required | Full queue URL. The last path segment is the queue name. |
| `region` | string | AWS credential chain | AWS region, for example `us-east-1`. Falls back to `AWS_REGION` or your profile. |
| `endpoint_url` | string | — | Custom SQS endpoint, for example `http://localhost:4566` for LocalStack. |
| `mode` | string | `batch` | `batch` receives until a 20-second long poll returns no messages, then finishes. `stream` keeps polling, backing off up to 5 seconds while the queue is empty. |
| `format` | string | `json` | How each message body is parsed: `json`, `csv`, or `xml`. |
| `batch_size_bytes` | integer | `1024000` | Skipprd writes received messages to the WAL, then deletes them, each time this many bytes accumulate, and at the end of every receive. |
| `batch_size_seconds` | integer | — | Not used by this connector. |

### Environment variables

| Key | Type | Default | Description |
|---|---|---|---|
| `SKIPPR_RUNTIME_ONCE_IDLE_TIMEOUT_SECONDS` | integer | `60` | With `skipprd sync --once` in `stream` mode, seconds without a new message before the run finishes. |

## What gets synced

**Records.** Skipprd reads the message body and ignores message attributes. With the default `format: json`, a JSON object becomes one record, a JSON array becomes one record per element, and newline-delimited JSON becomes one record per line.

**Namespace.** All records land in the namespace `sqs.<queue_name>`, for example `sqs.orders`.

**Delivery.** Skipprd receives up to 10 messages per request, writes them to the WAL, and then deletes them. If Skipprd stops before the delete, or a delete fails, SQS makes the message visible again after its visibility timeout and Skipprd lands it again. Delivery is at least once, so design downstream models to tolerate an occasional duplicate.

**Ordering.** Messages land in the order SQS returns them. Standard queues don't guarantee order; FIFO queues do within a message group.

**Restarts.** Messages stay in the queue until Skipprd deletes them, so a restarted pipeline picks up where it stopped. In continuous mode (`skipprd sync` without `--once`), a `batch` source runs again every `sync_frequency_seconds` (900 seconds by default). See [Pipelines](/configuration/pipeline).

**Schema discovery.** The first time a pipeline runs, Skipprd discovers the schema by reading messages from the queue. `skipprd sync` does this automatically if you haven't run `skipprd discover`. Discover deletes the messages it reads but doesn't land them, so run it against test messages or a test queue.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `AccessDenied` on `ReceiveMessage` or `DeleteMessage` | The IAM policy is missing an action. | Allow both `sqs:ReceiveMessage` and `sqs:DeleteMessage` on the queue ARN. |
| `KMS.AccessDeniedException` | The queue uses a customer-managed key. | Allow `kms:Decrypt` on the key. |
| `QueueDoesNotExist` or wrong-region errors | `queue_url` and `region` don't match. | Copy the URL from the SQS console and set `region` to the region in that URL. |
| The same message lands twice | Skipprd stopped between writing and deleting, or the visibility timeout is shorter than a write. | Expected under at-least-once delivery. Raise the queue's visibility timeout if it happens often. |
| `batch` run finishes immediately | The queue was empty for one 20-second long poll. | Send messages first, or use `mode: stream`. |
| `batch` run ends early with `SQS receive_message` in the log | A receive request failed. In `batch` mode Skipprd ends the run instead of retrying; `stream` mode retries with backoff. | Fix the error in the log (credentials, network, region) and run again. |

## Next steps

- [SNS source](/connectors/inputs/sns) and [EventBridge source](/connectors/inputs/eventbridge): consume AWS events through a queue.
- [Destinations](/configuration/output): land the queue in a warehouse.
- [WAL and buffering](/configuration/buffering): control how often batches land.
