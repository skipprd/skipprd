---
title: AMQP source
description: Consume a RabbitMQ or other AMQP 0-9-1 queue with Skipprd, acknowledging each message only after it is durable.
---

# AMQP source

The AMQP source consumes one queue on a RabbitMQ (or other AMQP 0-9-1) broker and lands each message body as one or more records. Use it when your applications already publish events to RabbitMQ and you want them in a warehouse without writing a consumer. Skipprd acknowledges a message only after it is safely in its write-ahead log (WAL), so a crash never loses an acknowledged message.

## Before you begin

You need:

- Network access from the machine running Skipprd to the broker, usually port `5672` (or `5671` for TLS).
- A broker user that can read the queue on its virtual host. To bind the queue to an exchange, the user also needs permission to bind.
- The connection URI, for example `amqp://user:password@rabbit.internal:5672/%2f`. Use `amqps://` for a TLS listener. `%2f` is the URL-encoded default vhost `/`.

Skipprd declares the queue as **durable** when it starts. If the queue already exists as non-durable, the broker rejects the declaration. Create it as durable, or let Skipprd create it.

## Configure

The connection URI contains the password, so keep it in an environment variable. `skippr.yml` stores only the reference.

```bash
export AMQP_URL="amqp://skippr:your-password@rabbit.internal:5672/%2f"
```

::: code-group

```python [Python]
from skippr import Config, DataSourceAmqp, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "order_events",
    DataSourceAmqp(
        connection_string=EnvRef("AMQP_URL"),
        queue="skippr.orders",
        exchange="orders",
        routing_key="order.*",
    ),
)
cfg.pipeline("orders", Pipeline(data_source=src))
cfg.save()
```

```bash [CLI]
skipprd connect data-source amqp \
  --pipeline orders \
  --name order_events \
  --connection-string '${AMQP_URL}' \
  --queue skippr.orders \
  --exchange orders \
  --routing-key 'order.*'
```

```yaml [YAML]
pipelines:
  orders:
    data_source: data_sources.order_events

data_sources:
  order_events:
    Amqp:
      connection_string: "${AMQP_URL}"
      queue: skippr.orders
      exchange: orders
      routing_key: "order.*"
```

:::

Leave out `exchange` and `routing_key` to consume a queue that is already bound.

Run the pipeline and check the result:

1. Publish a few test messages. The first run discovers the schema from live messages (see [What gets synced](#what-gets-synced)).
2. Start the sync. Without `--once`, Skipprd keeps consuming until you stop it.

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
| `connection_string` | secret | Required | AMQP URI as a `${NAME}` reference, for example `amqp://user:pass@host:5672/%2f`. |
| `queue` | string | Required | Queue to consume. Skipprd declares it as durable. |
| `exchange` | string | — | Exchange to bind the queue to. Used only when `routing_key` is also set. |
| `routing_key` | string | — | Binding key for `exchange`. Used only when `exchange` is also set. |
| `consumer_tag` | string | `skippr-<random>` | Consumer tag shown in the broker's management UI. |
| `prefetch_count` | integer | `10` | Maximum unacknowledged messages the broker sends to Skipprd at once. |
| `mode` | string | `stream` | `stream` keeps consuming until Skipprd stops. `batch` stops after `idle_timeout_seconds` with no new message, which suits scheduled jobs that drain a queue. |
| `idle_timeout_seconds` | integer | `5` | In `batch` mode, seconds without a message before the source finishes. |
| `format` | string | `json` | How each message body is parsed: `json`, `csv`, or `xml`. |
| `batch_size_bytes` | integer | — | Not used by this connector. Each message is written individually; tune landing batches with the pipeline [buffer thresholds](/configuration/buffering). |
| `batch_size_seconds` | integer | — | Not used by this connector. |

### Environment variables

| Key | Type | Default | Description |
|---|---|---|---|
| `SKIPPR_RUNTIME_ONCE_IDLE_TIMEOUT_SECONDS` | integer | `60` | With `skipprd sync --once` in `stream` mode, seconds without a new message before the run finishes. |

## What gets synced

**Records.** Skipprd reads the message body and ignores message properties and headers. With the default `format: json`, a JSON object becomes one record, a JSON array becomes one record per element, and newline-delimited JSON becomes one record per line. With `format: csv`, include the header row in every message.

**Namespace.** All records land in the namespace `amqp.<queue>`, for example `amqp.skippr.orders`.

**Delivery.** Skipprd acknowledges each message after it is durable in the WAL. If Skipprd stops before acknowledging, the broker redelivers the message, so delivery is at least once and a message can occasionally land twice.

**Ordering.** Skipprd processes messages one at a time in the order the broker delivers them. Warehouse tables don't keep arrival order, so include an event timestamp if order matters.

**Restarts.** Unacknowledged messages stay on the queue and are delivered again when Skipprd reconnects. Messages published while Skipprd is stopped wait in the durable queue.

**Schema discovery.** The first time a pipeline runs, Skipprd discovers the schema by reading messages from the queue. `skipprd sync` does this automatically if you haven't run `skipprd discover`. Discover acknowledges the messages it reads but doesn't land them, so run it against test messages or a test queue.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `ACCESS_REFUSED` or connection closed at login | Wrong user, password, or vhost in the URI. | Check the URI. URL-encode special characters in the password and the vhost (`/` is `%2f`). |
| `PRECONDITION_FAILED` on queue declare | The queue exists with different settings, for example non-durable. | Recreate the queue as durable, or point `queue` at a durable queue. |
| `environment variable is not set` | `${AMQP_URL}` can't be resolved. | Export the variable in the shell or service that runs `skipprd`. |
| No records, no errors | The queue isn't bound to the exchange your publishers use. | Set both `exchange` and `routing_key`, or check the bindings in the broker's management UI. |
| Only `exchange` set, nothing bound | The binding needs both keys. | Set `routing_key` too. Skipprd binds only when both are present. |

## Next steps

- [Destinations](/configuration/output): land the queue in a warehouse.
- [WAL and buffering](/configuration/buffering): control how often batches land.
- [Exactly-once delivery](/concepts/exactly-once): what Skipprd guarantees after the WAL.
