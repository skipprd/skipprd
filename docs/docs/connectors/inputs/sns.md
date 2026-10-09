---
title: SNS source
description: Land Amazon SNS topic messages with Skipprd by reading them from a subscribed SQS queue and unwrapping the SNS envelope.
---

# SNS source

The SNS source lands messages published to an Amazon SNS topic. SNS can't be read directly, so you subscribe an SQS queue to the topic and Skipprd reads that queue. Skipprd unwraps the SNS envelope and lands the original message, then deletes it from the queue once it is safely in the write-ahead log (WAL).

## Before you begin

You need:

1. **An SQS queue subscribed to the topic.** Skipprd doesn't create the subscription. Create it once:

   ```bash
   aws sns subscribe \
     --topic-arn arn:aws:sns:us-east-1:123456789012:orders \
     --protocol sqs \
     --notification-endpoint arn:aws:sqs:us-east-1:123456789012:orders-from-sns
   ```

   Raw message delivery can be on or off. Skipprd handles both.

2. **A queue policy that lets SNS deliver to the queue:**

   ```json
   {
     "Version": "2012-10-17",
     "Statement": [
       {
         "Effect": "Allow",
         "Principal": { "Service": "sns.amazonaws.com" },
         "Action": "sqs:SendMessage",
         "Resource": "arn:aws:sqs:us-east-1:123456789012:orders-from-sns",
         "Condition": {
           "ArnEquals": { "aws:SourceArn": "arn:aws:sns:us-east-1:123456789012:orders" }
         }
       }
     ]
   }
   ```

3. **AWS credentials for Skipprd** through the standard AWS credential chain (environment variables, a shared profile, or an instance, task, or pod role), with this IAM policy. Skipprd calls only SQS; it makes no SNS API calls.

   ```json
   {
     "Version": "2012-10-17",
     "Statement": [
       {
         "Effect": "Allow",
         "Action": ["sqs:ReceiveMessage", "sqs:DeleteMessage"],
         "Resource": "arn:aws:sqs:us-east-1:123456789012:orders-from-sns"
       }
     ]
   }
   ```

   If the queue uses a customer-managed KMS key, also allow `kms:Decrypt` on that key.

## Configure

Any standard AWS credential source works. For example, use a named profile:

```bash
export AWS_PROFILE=analytics
```

::: code-group

```python [Python]
from skippr import Config, DataSourceSns, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "orders_topic",
    DataSourceSns(
        topic_arn="arn:aws:sns:us-east-1:123456789012:orders",
        sqs_queue_url="https://sqs.us-east-1.amazonaws.com/123456789012/orders-from-sns",
        region="us-east-1",
    ),
)
cfg.pipeline("orders", Pipeline(data_source=src))
cfg.save()
```

```bash [CLI]
skipprd connect data-source sns \
  --pipeline orders \
  --name orders_topic \
  --topic-arn arn:aws:sns:us-east-1:123456789012:orders \
  --sqs-queue-url https://sqs.us-east-1.amazonaws.com/123456789012/orders-from-sns \
  --region us-east-1
```

```yaml [YAML]
pipelines:
  orders:
    data_source: data_sources.orders_topic

data_sources:
  orders_topic:
    Sns:
      topic_arn: "arn:aws:sns:us-east-1:123456789012:orders"
      sqs_queue_url: "https://sqs.us-east-1.amazonaws.com/123456789012/orders-from-sns"
      region: us-east-1
```

:::

Run the pipeline and check the result:

1. Publish a test message. The first run discovers the schema from live messages (see [What gets synced](#what-gets-synced)).

   ```bash
   aws sns publish \
     --topic-arn arn:aws:sns:us-east-1:123456789012:orders \
     --message '{"order_id": 1001, "status": "paid"}'
   ```

2. Start the sync. The SNS source keeps polling until you stop it.

   ```bash
   skipprd sync --pipeline orders --log
   ```

3. In another terminal, read what landed:

   ```bash
   skipprd df --pipeline orders
   ```

To load the data into a warehouse, add a destination to the pipeline. See [Destinations](/configuration/output).

## Options

| Key | Type | Default | Description |
|---|---|---|---|
| `topic_arn` | string | Required | Topic ARN. Skipprd uses the last segment (the topic name) to name the namespace. |
| `sqs_queue_url` | string | Required | URL of the SQS queue subscribed to the topic. |
| `region` | string | AWS credential chain | AWS region of the queue, for example `us-east-1`. |
| `endpoint_url` | string | — | Custom SQS endpoint, for example `http://localhost:4566` for LocalStack. |
| `format` | string | `json` | How each unwrapped message is parsed: `json`, `csv`, or `xml`. |
| `batch_size_bytes` | integer | — | Not used by this connector. Tune landing batches with the pipeline [buffer thresholds](/configuration/buffering). |
| `batch_size_seconds` | integer | — | Not used by this connector. |

### Environment variables

| Key | Type | Default | Description |
|---|---|---|---|
| `SKIPPR_RUNTIME_ONCE_IDLE_TIMEOUT_SECONDS` | integer | `60` | With `skipprd sync --once`, seconds without a new message before the run finishes. |

## What gets synced

**Records.** When the queue message is an SNS envelope, Skipprd lands its `Message` field and drops the rest of the envelope (`TopicArn`, `Timestamp`, `MessageAttributes`, and so on). With raw message delivery, the body is already the message and lands as is. With the default `format: json`, a JSON object becomes one record and a JSON array becomes one record per element.

**Namespace.** All records land in the namespace `sns.<topic_name>`, for example `sns.orders`.

**Delivery.** Skipprd receives up to 10 messages at a time, writes them to the WAL, and then deletes them from the queue. If Skipprd stops before the delete, SQS redelivers the message after its visibility timeout. Delivery is at least once, so a message can occasionally land twice.

**Ordering.** Messages land in the order SQS returns them, which isn't guaranteed for standard queues.

**Running and restarts.** The SNS source always streams: it keeps polling until Skipprd stops. With `--once`, the run finishes after 60 seconds without a new message. Messages published while Skipprd is stopped wait in the queue.

**Schema discovery.** The first time a pipeline runs, Skipprd discovers the schema by reading messages from the queue. `skipprd sync` does this automatically if you haven't run `skipprd discover`. Discover deletes the messages it reads but doesn't land them, so run it against test messages.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| No records after publishing | The queue isn't subscribed, or the queue policy blocks SNS. | Check the subscription is confirmed and the queue policy allows `sqs:SendMessage` from the topic ARN. |
| Run fails with `AccessDenied` | Skipprd's credentials can't receive or delete. | Allow `sqs:ReceiveMessage` and `sqs:DeleteMessage` on the queue ARN. A failed receive stops the run. |
| `QueueDoesNotExist` or wrong-region errors | `sqs_queue_url` and `region` don't match. | Set `region` to the region in the queue URL. |
| The same message lands twice | Skipprd stopped between writing and deleting. | Expected under at-least-once delivery. |

## Next steps

- [SQS source](/connectors/inputs/sqs): read any SQS queue directly.
- [Destinations](/configuration/output): land the topic in a warehouse.
- [WAL and buffering](/configuration/buffering): control how often batches land.
