---
title: OTLP source
description: Receive OpenTelemetry traces, logs, and metrics in Skipprd over OTLP gRPC and HTTP, and land them as typed tables.
---

# OTLP source

The OTLP source runs OpenTelemetry Protocol (OTLP) receivers inside Skipprd and lands traces, logs, and metrics as typed tables such as `spans` and `log_records`. Point an OpenTelemetry Collector, or an SDK's OTLP exporter, at Skipprd to keep your telemetry in your own lake or warehouse. Pair it with [SkipprLake](/connectors/outputs/skipprlake) for partitioned Iceberg tables.

Do sampling and PII redaction in the Collector before data reaches Skipprd. Skipprd can additionally drop attributes with `attribute_allowlist`.

## Before you begin

You need:

- Free ports for the receivers: `4317` (gRPC) and `4318` (HTTP) by default.
- Inbound access to those ports from your Collectors or applications: open them in the host firewall and security group. Both receivers speak plain, unencrypted OTLP, so terminate TLS in front of Skipprd if traffic crosses an untrusted network.
- Optionally, a bearer token that senders must present.

## Configure

Generate a token and keep it in an environment variable. `skippr.yml` stores only the reference.

```bash
export OTLP_TOKEN="$(openssl rand -hex 32)"
```

::: code-group

```python [Python]
from skippr import Config, DataSourceOtlp, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "otlp_logs",
    DataSourceOtlp(
        listen_address_grpc="0.0.0.0:4317",
        listen_address_http="0.0.0.0:4318",
        signals=["logs"],
        auth_token=EnvRef("OTLP_TOKEN"),
    ),
)
cfg.pipeline("otel-logs", Pipeline(data_source=src))
cfg.save()
```

```bash [CLI]
skipprd connect data-source otlp \
  --pipeline otel-logs \
  --name otlp_logs \
  --listen-address-grpc 0.0.0.0:4317 \
  --listen-address-http 0.0.0.0:4318 \
  --signals '[logs]' \
  --auth-token '${OTLP_TOKEN}'
```

```yaml [YAML]
pipelines:
  otel-logs:
    data_source: data_sources.otlp_logs

data_sources:
  otlp_logs:
    Otlp:
      listen_address_grpc: "0.0.0.0:4317"
      listen_address_http: "0.0.0.0:4318"
      signals: [logs]
      auth_token: "${OTLP_TOKEN}"
```

:::

Run the pipeline and send a test log:

1. Start the sync. The receivers run until you stop Skipprd.

   ```bash
   skipprd sync --pipeline otel-logs --log
   ```

2. In another terminal, send one log record as OTLP/JSON over HTTP. A `200` response means Skipprd accepted it.

   ```bash
   curl -i http://localhost:4318/v1/logs \
     -H "Content-Type: application/json" \
     -H "Authorization: Bearer $OTLP_TOKEN" \
     -d '{
       "resourceLogs": [{
         "resource": {"attributes": [{"key": "service.name", "value": {"stringValue": "checkout"}}]},
         "scopeLogs": [{"logRecords": [{
           "timeUnixNano": "1760000000000000000",
           "severityText": "ERROR",
           "body": {"stringValue": "payment timeout"}
         }]}]
       }]
     }'
   ```

3. Read what landed:

   ```bash
   skipprd df --pipeline otel-logs
   ```

### Send from an OpenTelemetry Collector

Add an OTLP exporter that points at Skipprd and sends the token:

```yaml
exporters:
  otlp/skippr:
    endpoint: skippr.internal:4317
    tls:
      insecure: true
    headers:
      authorization: "Bearer ${env:OTLP_TOKEN}"

service:
  pipelines:
    logs:
      receivers: [otlp]
      exporters: [otlp/skippr]
```

Use `otlphttp` with `endpoint: http://skippr.internal:4318` if you prefer HTTP.

### One pipeline per signal

You can accept all three signals in one pipeline, but the usual layout is one pipeline per signal, each run as its own `skipprd sync` process with its own data directory and its own ports. That lets each signal use its own time field for partitioning.

```bash
DATA_DIR=/data/traces  skipprd sync --pipeline otel-traces
DATA_DIR=/data/logs    skipprd sync --pipeline otel-logs
DATA_DIR=/data/metrics skipprd sync --pipeline otel-metrics
```

Give each pipeline's source different `listen_address_grpc` and `listen_address_http` ports, for example `4317`/`4318`, `4319`/`4320`, and `4321`/`4322`, and point each Collector exporter at the matching port.

To load the data into a lake or warehouse, add a destination to the pipeline. See [SkipprLake](/connectors/outputs/skipprlake) and [Destinations](/configuration/output).

## Options

| Key | Type | Default | Description |
|---|---|---|---|
| `listen_address_grpc` | string | `0.0.0.0:4317` | Address and port for the OTLP gRPC receiver. |
| `listen_address_http` | string | `0.0.0.0:4318` | Address and port for the OTLP HTTP receiver (`/v1/traces`, `/v1/logs`, `/v1/metrics`). |
| `signals` | list of strings | `[traces, logs, metrics]` | Signals to accept: any of `traces`, `logs`, `metrics`. Must not be empty. Requests for other signals are rejected. |
| `auth_token` | secret | — | Bearer token as a `${NAME}` reference. When set, every request must send `Authorization: Bearer <token>`. |
| `attribute_allowlist` | list of strings | keep all | Attribute keys to keep in the attribute columns. Keys not in the list are dropped and counted in `dropped_attributes_count`. `[]` drops every attribute. Promoted columns such as `service_name` are filled either way. |

The OTLP source has no `format` or batch options. Any key not in this table is rejected.

## What gets synced

**Tables.** Each signal lands in its own namespaces:

| Signal | Namespaces |
|---|---|
| `traces` | `spans`, `span_events`, `span_links` |
| `logs` | `log_records` |
| `metrics` | `gauge`, `sum`, `histogram`, `exponential_histogram` |

Query them with the pipeline name as the schema, quoting names with hyphens, for example `"otel-logs".log_records`.

**Columns.** Common OpenTelemetry fields become typed columns: for example `trace_id`, `span_id`, `name`, `kind`, `start_time_unix_nano`, `duration_nano`, and `status_code` on `spans`, and `time_unix_nano`, `severity_text`, and `body` on `log_records`. Every table also has:

- `service_name`, from the `service.name` resource attribute;
- `tenant_id`, from the `tenant.id` resource attribute, or from a pipeline transform `inject_fields: {tenant_id: ...}` when the attribute is missing;
- `hour`, the event hour, used for partitioning;
- attribute maps such as `resource_attributes` and `span_attributes`, stored as JSON text.

**Partitioning.** New Iceberg tables are partitioned on `hour`, `service_name`, and `tenant_id`. Existing unpartitioned tables stay unpartitioned.

**Encodings.** The HTTP receiver accepts protobuf (`application/x-protobuf`) and JSON (any `Content-Type` containing `json`). Requests over 16 MiB are rejected with `413` (HTTP) or a size error (gRPC); this limit is fixed.

**Delivery.** Skipprd replies with success once it has decoded a request, before the data is written to the write-ahead log (WAL). Telemetry accepted in the moments before a crash can be lost, and nothing is accepted while Skipprd is stopped. Use the Collector's sending queue and retries to ride out restarts.

**Retention.** Skipprd doesn't expire telemetry. Manage retention in the destination, for example with object-store lifecycle rules.

**Schema.** Skipprd knows the OTLP schema up front, so `skipprd sync` doesn't need a discover pass first.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `Failed to bind HTTP 0.0.0.0:4318` | Another process, often a local Collector, uses the port. | Change `listen_address_http` and `listen_address_grpc`, or stop the other process. |
| `401 unauthorized` or gRPC `Unauthenticated` | The token is missing or wrong. | Send `Authorization: Bearer <token>`. In the Collector, set it under `headers`. |
| `400 signal not enabled` or gRPC `FailedPrecondition` | The signal isn't in `signals`. | Add it to `signals`, or send that signal to the pipeline that accepts it. |
| `400` with a decode error | The body isn't valid OTLP for its `Content-Type`. | Send protobuf with `application/x-protobuf`, or OTLP/JSON with `application/json`. |
| `413 payload too large` | A request exceeds 16 MiB. | Lower the Collector's batch size (`send_batch_max_size`). |
| `tenant_id` is empty | Senders don't set `tenant.id`. | Add the resource attribute in the Collector, or set `inject_fields` on the pipeline transform. |
| Startup fails with `unknown field` | The source has a key that isn't an OTLP option, such as `format`. | Remove it. |

## Next steps

- [SkipprLake destination](/connectors/outputs/skipprlake): partitioned Iceberg tables for telemetry.
- [Transforms](/configuration/transforms): set `inject_fields` and time partitioning.
- [HTTP server source](/connectors/inputs/http_server): accept other JSON over HTTP.
