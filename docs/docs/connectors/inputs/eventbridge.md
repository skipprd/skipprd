---
title: EventBridge source
description: Land Amazon EventBridge events with Skipprd by routing them to an SQS queue and syncing each event's detail as a record.
---

# EventBridge source

The EventBridge source lands events from an Amazon EventBridge bus. You add a rule that targets an SQS queue, and Skipprd reads that queue, lands each event's `detail` object as a record, and deletes the message once it is safely in the write-ahead log (WAL). Use it to load AWS service events or your own custom events into a warehouse.

## Before you begin

You need:

1. **A rule on the bus that targets an SQS queue.** Skipprd doesn't create rules or targets. For example:

   ```bash
   aws events put-rule \
     --name orders-to-skippr \
     --event-bus-name orders-bus \
     --event-pattern '{"source": ["com.example.orders"]}'

   aws events put-targets \
     --rule orders-to-skippr \
     --event-bus-name orders-bus \
     --targets Id=skippr,Arn=arn:aws:sqs:us-east-1:123456789012:orders-events
   ```

2. **A queue policy that lets EventBridge deliver to the queue:**

   ```json
   {
     "Version": "2012-10-17",
     "Statement": [
       {
         "Effect": "Allow",
         "Principal": { "Service": "events.amazonaws.com" },
         "Action": "sqs:SendMessage",
         "Resource": "arn:aws:sqs:us-east-1:123456789012:orders-events",
         "Condition": {
           "ArnEquals": { "aws:SourceArn": "arn:aws:events:us-east-1:123456789012:rule/orders-bus/orders-to-skippr" }
         }
       }
     ]
   }
   ```

3. **AWS credentials for Skipprd** through the standard AWS credential chain (environment variables, a shared profile, or an instance, task, or pod role), with this IAM policy. Skipprd calls only SQS; it makes no EventBridge API calls.

   ```json
   {
     "Version": "2012-10-17",
     "Statement": [
       {
         "Effect": "Allow",
         "Action": ["sqs:ReceiveMessage", "sqs:DeleteMessage"],
         "Resource": "arn:aws:sqs:us-east-1:123456789012:orders-events"
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
from skippr import Config, DataSourceEventbridge, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "order_events",
    DataSourceEventbridge(
        event_bus_name="orders-bus",
        sqs_queue_url="https://sqs.us-east-1.amazonaws.com/123456789012/orders-events",
        region="us-east-1",
    ),
)
cfg.pipeline("orders", Pipeline(data_source=src))
cfg.save()
```

```bash [CLI]
skipprd connect data-source eventbridge \
  --pipeline orders \
  --name order_events \
  --event-bus-name orders-bus \
  --sqs-queue-url https://sqs.us-east-1.amazonaws.com/123456789012/orders-events \
  --region us-east-1
```

```yaml [YAML]
pipelines:
  orders:
    data_source: data_sources.order_events

data_sources:
  order_events:
    Eventbridge:
      event_bus_name: orders-bus
      sqs_queue_url: "https://sqs.us-east-1.amazonaws.com/123456789012/orders-events"
      region: us-east-1
```

:::

Run the pipeline and check the result:

1. Put a test event on the bus. The first run discovers the schema from live messages (see [What gets synced](#what-gets-synced)).

   ```bash
   aws events put-events --entries '[{
     "EventBusName": "orders-bus",
     "Source": "com.example.orders",
     "DetailType": "OrderPaid",
     "Detail": "{\"order_id\": 1001, \"status\": \"paid\"}"
   }]'
   ```

2. Start the sync. The EventBridge source keeps polling until you stop it.

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
| `event_bus_name` | string | Required | Bus name. Skipprd uses it to name the namespace. |
| `sqs_queue_url` | string | Required | URL of the SQS queue your rule targets. |
| `rule_name` | string | — | Not used by this connector. Skipprd doesn't read or manage rules. |
| `region` | string | AWS credential chain | AWS region of the queue, for example `us-east-1`. |
| `endpoint_url` | string | — | Custom SQS endpoint, for example `http://localhost:4566` for LocalStack. |
| `format` | string | `json` | How each event's `detail` is parsed. Leave as `json`. |
| `batch_size_bytes` | integer | — | Not used by this connector. Tune landing batches with the pipeline [buffer thresholds](/configuration/buffering). |
| `batch_size_seconds` | integer | — | Not used by this connector. |

### Environment variables

| Key | Type | Default | Description |
|---|---|---|---|
| `SKIPPR_RUNTIME_ONCE_IDLE_TIMEOUT_SECONDS` | integer | `60` | With `skipprd sync --once`, seconds without a new event before the run finishes. |

## What gets synced

**Records.** Each event lands as one record built from its `detail` object. The envelope fields (`id`, `source`, `detail-type`, `time`, `account`, `region`, `resources`) are dropped. If you need them, copy them into `detail` when you publish, or add an input transformer to the rule target. A message body without a top-level `detail` field, such as an input transformer's output, lands whole.

**Namespace.** All records land in the namespace `eventbridge.<event_bus_name>`, for example `eventbridge.orders-bus`.

**Delivery.** Skipprd receives up to 10 messages at a time, writes them to the WAL, and then deletes them from the queue. If Skipprd stops before the delete, SQS redelivers the message after its visibility timeout. Delivery is at least once, so an event can occasionally land twice.

**Ordering.** Events land in the order SQS returns them, which isn't guaranteed for standard queues.

**Running and restarts.** The EventBridge source always streams: it keeps polling until Skipprd stops. With `--once`, the run finishes after 60 seconds without a new event. Events routed while Skipprd is stopped wait in the queue.

**Schema discovery.** The first time a pipeline runs, Skipprd discovers the schema by reading messages from the queue. `skipprd sync` does this automatically if you haven't run `skipprd discover`. Discover deletes the messages it reads but doesn't land them, so run it against test events.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| No records after `put-events` | The rule pattern doesn't match, or the queue policy blocks EventBridge. | Check the rule's event pattern and target, and that the queue policy allows `sqs:SendMessage` from the rule ARN. |
| Run fails with `AccessDenied` | Skipprd's credentials can't receive or delete. | Allow `sqs:ReceiveMessage` and `sqs:DeleteMessage` on the queue ARN. A failed receive stops the run. |
| `source` or `detail-type` missing in the destination | Only `detail` lands. | Add the fields to `detail`, or use a rule input transformer whose output has no top-level `detail` field, so the whole payload lands. |
| `QueueDoesNotExist` or wrong-region errors | `sqs_queue_url` and `region` don't match. | Set `region` to the region in the queue URL. |

## Next steps

- [SQS source](/connectors/inputs/sqs): read any SQS queue directly.
- [Destinations](/configuration/output): land events in a warehouse.
- [WAL and buffering](/configuration/buffering): control how often batches land.
