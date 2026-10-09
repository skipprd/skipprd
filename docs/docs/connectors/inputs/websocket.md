---
title: WebSocket source
description: Connect Skipprd to a WebSocket feed and land each text or binary message as a record while the connection is open.
---

# WebSocket source

The WebSocket source opens a connection to a WebSocket server and lands each message it receives as one or more records. Use it to capture a live feed, such as market prices or an internal event stream, that publishes JSON over a WebSocket. Skipprd captures only what arrives while it is connected; WebSocket feeds have no replay.

## Before you begin

You need:

- A `ws://` URL that the machine running Skipprd can reach.
- A feed that starts sending messages as soon as a client connects. Skipprd doesn't send anything after the handshake, so feeds that need a subscribe message aren't supported.

::: warning Current limits
- `wss://` (TLS) URLs aren't supported in current builds.
- `headers` and `ping_interval_seconds` are accepted in the config but not yet applied, so endpoints that need an `Authorization` header can't be reached. Use an endpoint that accepts the connection without custom headers, or pass credentials in the URL query string if the service supports it.
:::

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSourceWebsocket, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "price_feed",
    DataSourceWebsocket(url="ws://feeds.internal:8080/prices"),
)
cfg.pipeline("prices", Pipeline(data_source=src))
cfg.save()
```

```bash [CLI]
skipprd connect data-source websocket \
  --pipeline prices \
  --name price_feed \
  --url ws://feeds.internal:8080/prices
```

```yaml [YAML]
pipelines:
  prices:
    data_source: data_sources.price_feed

data_sources:
  price_feed:
    Websocket:
      url: "ws://feeds.internal:8080/prices"
```

:::

Run the pipeline and check the result:

1. Start the sync. Without `--once`, Skipprd stays connected until you stop it or the server closes the connection.

   ```bash
   skipprd sync --pipeline prices --log
   ```

2. In another terminal, read what landed:

   ```bash
   skipprd df --pipeline prices
   ```

To capture a fixed window instead, set `mode: batch` and `idle_timeout_seconds`, then run `skipprd sync --pipeline prices --once`.

To load the data into a warehouse, add a destination to the pipeline. See [Destinations](/configuration/output).

## Options

| Key | Type | Default | Description |
|---|---|---|---|
| `url` | string | Required | WebSocket URL. Use `ws://`. |
| `headers` | map of strings | — | Not yet applied to the connection. |
| `ping_interval_seconds` | integer | — | Not yet applied to the connection. |
| `mode` | string | `stream` | `stream` stays connected until Skipprd stops or the server closes. `batch` disconnects after `idle_timeout_seconds` with no new message. |
| `idle_timeout_seconds` | integer | `5` | In `batch` mode, seconds without a message before the source finishes. |
| `format` | string | `json` | How each message is parsed: `json`, `csv`, or `xml`. |
| `batch_size_bytes` | integer | — | Not used by this connector. Each message is written individually; tune landing batches with the pipeline [buffer thresholds](/configuration/buffering). |
| `batch_size_seconds` | integer | — | Not used by this connector. |

### Environment variables

| Key | Type | Default | Description |
|---|---|---|---|
| `SKIPPR_RUNTIME_ONCE_IDLE_TIMEOUT_SECONDS` | integer | `60` | With `skipprd sync --once` in `stream` mode, seconds without a new message before the run finishes. |

## What gets synced

**Records.** Text messages land as received. Binary messages are read as UTF-8 text. Empty messages and ping and pong frames are skipped. With the default `format: json`, a JSON object becomes one record and a JSON array becomes one record per element.

**Namespace.** All records land in the namespace `websocket.<host>`, using the host from `url`, for example `websocket.feeds.internal`.

**Delivery.** Messages that arrive while Skipprd isn't connected are lost; the server doesn't resend them. Once received, each message is written to the write-ahead log (WAL) before the next one is read.

**Ordering.** Skipprd processes messages one at a time, in the order the server sends them.

**Restarts.** When the server closes the connection or the connection fails, the source stops. In continuous mode, the next sync loop reconnects; messages sent in between are not captured.

**Schema discovery.** The first time a pipeline runs, Skipprd discovers the schema from live messages. `skipprd sync` does this automatically if you haven't run `skipprd discover`. Messages received during discover aren't landed.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| Error mentioning TLS or an unsupported URL scheme | The URL uses `wss://`. | Use a `ws://` endpoint. |
| Handshake fails with `401` or `403` | The endpoint needs an auth header, which isn't applied yet. | Use an endpoint or query-string credential that doesn't need custom headers. |
| Connected, but no records | The feed waits for a subscribe message. | Point Skipprd at a feed that streams on connect. |
| Run stops on its own | The server closed the connection. | Check the server's idle and session limits. In continuous mode Skipprd reconnects on the next sync loop. |

## Next steps

- [Destinations](/configuration/output): land the feed in a warehouse.
- [HTTP client source](/connectors/inputs/http_client): poll an HTTP API instead.
- [WAL and buffering](/configuration/buffering): control how often batches land.
