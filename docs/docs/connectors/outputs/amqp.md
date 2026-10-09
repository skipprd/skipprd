---
title: AMQP destination
description: Publish Skipprd batches to an AMQP exchange. Retries can duplicate messages.
---

# AMQP

Publishes each batch to an exchange. Brokers such as RabbitMQ work. A retry can publish again, so consumers should be idempotent. See [Exactly-once delivery](/concepts/exactly-once).

## Before you begin

- A connection string (`amqps://user:pass@host:5671/vhost`).
- Store credentials in the environment.

```bash
export AMQP_URL="amqps://skippr:${AMQP_PASSWORD}@rabbit.internal:5671/"
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSinkAmqp, EnvRef, Pipeline

cfg = Config.discover()
q = cfg.data_sink(
    "events",
    DataSinkAmqp(
        connection_string=EnvRef("AMQP_URL"),
        exchange="skipprd",
        routing_key="events",
        exchange_type="topic",
    ),
)
cfg.pipeline("events", Pipeline(data_source=cfg.get_data_source("sample"), data_sink=q))
cfg.save()
```

```bash [CLI]
skipprd connect data-sink amqp \
  --pipeline events \
  --name events \
  --connection-string '${AMQP_URL}' \
  --exchange skipprd \
  --routing-key events \
  --exchange-type topic
```

```yaml [YAML]
data_sinks:
  events:
    Amqp:
      connection_string: ${AMQP_URL}
      exchange: skipprd
      routing_key: events
      exchange_type: topic
```

:::

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `connection_string` | secret | Required | AMQP URL as `${ENV}` |
| `exchange` | string | Required | Exchange name |
| `routing_key` | string | Required | Routing key |
| `exchange_type` | string | Not set | `topic`, `direct`, `fanout`, … |
| `format` | string | Not set | Payload format |
| `max_in_flight` | integer | Not set | Max outstanding publishes |
| `max_in_flight_bytes` | integer | Not set | Max outstanding bytes |

## How data lands

Each flushed batch is published to `exchange` with `routing_key`. Bind a queue on the broker to receive it.

## Troubleshooting

| Symptom | Fix |
|---|---|
| Connection refused | Check host, TLS, and vhost |
| ACCESS_REFUSED | Grant publish on the exchange |
| Duplicates after a crash | Expected — make consumers idempotent |

## Next steps

- [AMQP source](/connectors/inputs/amqp)
- [Exactly-once delivery](/concepts/exactly-once)
