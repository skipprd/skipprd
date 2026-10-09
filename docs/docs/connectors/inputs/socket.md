---
title: Socket
description: Accept a stream of records on an address Skipprd listens on.
---

# Socket

Listen on `address` and parse framed payloads as records. Use it for simple network producers that are not HTTP, Kafka, or a cloud queue.

## Before you begin

- A free bind address on the Skipprd host (`0.0.0.0:9000` or similar).
- A producer that can connect and send the framing you choose.

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSourceSocket, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "net",
    DataSourceSocket(mode="tcp", address="0.0.0.0:9000", framing="line", format="json"),
)
cfg.pipeline("net", Pipeline(data_source=src))
cfg.save()
```

```bash [CLI]
skipprd connect data-source socket \
  --pipeline net \
  --name net \
  --mode tcp \
  --address 0.0.0.0:9000 \
  --framing line \
  --format json
```

```yaml [YAML]
data_sources:
  net:
    Socket:
      mode: tcp
      address: "0.0.0.0:9000"
      framing: line
      format: json
```

:::

Run `skipprd sync` (not `--once`) so the listener stays up.

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `mode` | string | Required | `tcp` or `udp` |
| `address` | string | Required | Bind address `host:port` |
| `framing` | string | Not set | How messages are split (`line`, …) |
| `format` | string | Not set | Payload format |
| `batch_size_bytes` | integer | Not set | Max bytes per batch |
| `batch_size_seconds` | integer | Not set | Max seconds per batch |

## What gets synced

Each framed message becomes a record. Push sources can lose a message that arrived after accept and before the WAL commit — have producers retry. See [Exactly-once delivery](/concepts/exactly-once).

## Troubleshooting

| Symptom | Fix |
|---|---|
| Address already in use | Pick another port, or stop the other listener |
| No rows | Confirm the producer uses the same `mode`, port, and `framing` |
| Parse errors | Match `format` to the payload |

## Next steps

- [HTTP server](/connectors/inputs/http_server)
- [Kafka](/connectors/inputs/kafka)
