---
title: HTTP server source
description: Run an HTTP endpoint in Skipprd that accepts POSTed JSON, CSV, or XML and lands each request body as records.
---

# HTTP server source

The HTTP server source runs an HTTP listener inside Skipprd and lands the body of every `POST` request as records. Use it for webhooks, or to let applications push events without a broker in between. You can protect the endpoint with a bearer token.

## Before you begin

You need:

- A free port on the machine running Skipprd. The default is `8080` on all interfaces.
- Inbound access to that port from your senders: open it in the host firewall and security group, or put a reverse proxy in front.
- A TLS-terminating reverse proxy or load balancer if senders reach Skipprd over the internet. The listener speaks plain HTTP.

## Configure

Generate a token for senders and keep it in an environment variable. `skippr.yml` stores only the reference.

```bash
export INGEST_TOKEN="$(openssl rand -hex 32)"
```

::: code-group

```python [Python]
from skippr import Config, DataSourceHttpServer, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "webhooks",
    DataSourceHttpServer(
        listen_address="0.0.0.0:8080",
        path="/events",
        auth_token=EnvRef("INGEST_TOKEN"),
    ),
)
cfg.pipeline("webhooks", Pipeline(data_source=src))
cfg.save()
```

```bash [CLI]
skipprd connect data-source http-server \
  --pipeline webhooks \
  --name webhooks \
  --listen-address 0.0.0.0:8080 \
  --path /events \
  --auth-token '${INGEST_TOKEN}'
```

```yaml [YAML]
pipelines:
  webhooks:
    data_source: data_sources.webhooks

data_sources:
  webhooks:
    HttpServer:
      listen_address: "0.0.0.0:8080"
      path: /events
      auth_token: "${INGEST_TOKEN}"
```

:::

Run the pipeline and send a test request:

1. Start the sync. The listener runs until you stop Skipprd.

   ```bash
   skipprd sync --pipeline webhooks --log
   ```

2. In another terminal, post an event. A `200` response means Skipprd accepted it.

   ```bash
   curl -i -X POST http://localhost:8080/events \
     -H "Authorization: Bearer $INGEST_TOKEN" \
     -H "Content-Type: application/json" \
     -d '{"event": "signup", "user_id": 42}'
   ```

   Send several records in one request as a JSON array or as newline-delimited JSON:

   ```bash
   printf '{"event":"click","user_id":42}\n{"event":"click","user_id":7}\n' |
     curl -i -X POST http://localhost:8080/events \
       -H "Authorization: Bearer $INGEST_TOKEN" \
       --data-binary @-
   ```

3. Read what landed:

   ```bash
   skipprd df --pipeline webhooks
   ```

To load the data into a warehouse, add a destination to the pipeline. See [Destinations](/configuration/output).

## Options

| Key | Type | Default | Description |
|---|---|---|---|
| `listen_address` | string | `0.0.0.0:8080` | Address and port to listen on. Use `127.0.0.1:8080` to accept only local senders, for example behind a reverse proxy on the same host. |
| `path` | string | `/` | URL path that accepts `POST` requests. |
| `auth_token` | secret | — | Bearer token as a `${NAME}` reference. When set, requests must send `Authorization: Bearer <token>`. When unset, the endpoint accepts any request. |
| `format` | string | `json` | How each request body is parsed: `json`, `csv`, or `xml`. |
| `batch_size_bytes` | integer | — | Not used by this connector. Each request is written individually; tune landing batches with the pipeline [buffer thresholds](/configuration/buffering). |
| `batch_size_seconds` | integer | — | Not used by this connector. |

### Environment variables

| Key | Type | Default | Description |
|---|---|---|---|
| `SKIPPR_RUNTIME_ONCE_IDLE_TIMEOUT_SECONDS` | integer | `60` | With `skipprd sync --once`, seconds without a new request before the listener stops. |

## What gets synced

**Records.** Each request body is parsed with `format`. With the default `format: json`, a JSON object becomes one record, a JSON array becomes one record per element, and newline-delimited JSON becomes one record per line. Headers, query strings, and the sender's address are not captured. For `csv`, include the header row in every request.

**Namespace.** All records land in the namespace `http_server`.

**Responses.**

| Status | When |
|---|---|
| `200` | The body was accepted. |
| `400` | The body is empty. |
| `401` | `auth_token` is set and the `Authorization` header doesn't match. |
| `405` | The method isn't `POST`. |
| `404` | The path doesn't match `path`. |

**Delivery.** Skipprd replies `200` as soon as it accepts a body, before the body is written to the write-ahead log (WAL). Requests accepted in the moments before a crash can be lost, and nothing is accepted while Skipprd is stopped. If senders need guaranteed delivery, have them retry on connection errors, or publish to a queue such as [SQS](/connectors/inputs/sqs) instead.

**Ordering.** Requests are written in the order Skipprd accepts them.

**Schema discovery.** The first time a pipeline runs, Skipprd discovers the schema from incoming requests. `skipprd sync` does this automatically if you haven't run `skipprd discover`. Requests received during discover aren't landed, so send test requests at that point.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `Failed to bind 0.0.0.0:8080` | Another process uses the port. | Stop that process or change `listen_address`. |
| `curl: (7) Failed to connect` | Skipprd isn't running, or a firewall blocks the port. | Start `skipprd sync`, then open the port to your senders. |
| `401 Unauthorized` | The token doesn't match. | Send exactly `Authorization: Bearer <token>`, using the value of `INGEST_TOKEN` that Skipprd started with. |
| `404 Not Found` | The request path differs from `path`. | Post to the configured path, for example `/events`. |
| `200` responses, but records missing after a restart | Requests were accepted but not yet written when Skipprd stopped. | Stop Skipprd only after senders pause, and have senders tolerate gaps or use a queue. |

## Next steps

- [Destinations](/configuration/output): land webhook events in a warehouse.
- [OTLP source](/connectors/inputs/otlp): receive OpenTelemetry traces, logs, and metrics.
- [WAL and buffering](/configuration/buffering): control how often batches land.
