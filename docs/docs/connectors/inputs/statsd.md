---
title: StatsD
description: Listen for StatsD metrics and land them as records you can query in a warehouse.
---

# StatsD

Skipprd binds `listen_address` and turns incoming StatsD lines into records. Use it to keep a copy of application metrics next to business data.

## Before you begin

- A free UDP bind address on the host (for example `0.0.0.0:8125`).
- Applications that can send StatsD to that address.

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSourceStatsd, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "metrics",
    DataSourceStatsd(listen_address="0.0.0.0:8125", format="statsd"),
)
cfg.pipeline("metrics", Pipeline(data_source=src))
cfg.save()
```

```bash [CLI]
skipprd connect data-source statsd \
  --pipeline metrics \
  --name metrics \
  --listen-address 0.0.0.0:8125 \
  --format statsd
```

```yaml [YAML]
data_sources:
  metrics:
    Statsd:
      listen_address: "0.0.0.0:8125"
      format: statsd
```

:::

Run `skipprd sync` without `--once` so the listener stays up.

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `listen_address` | string | Required | Bind `host:port` |
| `format` | string | Not set | Payload format |
| `batch_size_bytes` | integer | Not set | Max bytes per batch |
| `batch_size_seconds` | integer | Not set | Max seconds per batch |

## What gets synced

Each metric line becomes a record. Metrics that arrive before the WAL commit can be lost if the process dies — senders should tolerate that. See [Exactly-once delivery](/concepts/exactly-once).

## Troubleshooting

| Symptom | Fix |
|---|---|
| Address in use | Stop the other StatsD listener or change the port |
| No rows | Confirm applications send to this host and port |

## Next steps

- [OTLP](/connectors/inputs/otlp)
- [HTTP server](/connectors/inputs/http_server)
