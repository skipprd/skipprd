# OTLP Input

Receives OpenTelemetry Protocol (OTLP) traces, logs, and metrics over gRPC (`:4317`) and HTTP (`:4318`). Pair with the Iceberg sink. The OpenTelemetry Collector is the customer-run agent.

## How it works

1. Starts HTTP (`/v1/traces`, `/v1/logs`, `/v1/metrics`) and gRPC Trace/Logs/Metrics services.
2. Decodes protobuf or JSON Export* requests into bronze Arrow batches.
3. Emits namespaces `spans` / `span_events` / `span_links`, `log_records`, `gauge` / `sum` / `histogram` / `exponential_histogram`.
4. Optional Bearer token. Requests larger than 16 MiB are rejected (platform ceiling, not a tunable).

Sampling and PII redaction belong in the Collector. The plugin may apply an extra `attribute_allowlist`. Empty `tenant_id` can be filled with transform `inject_fields`.

New Iceberg tables are identity-partitioned on `hour`, `service_name`, and `tenant_id`. Existing unpartitioned tables stay unpartitioned.

Retention is tenant-managed: `ENABLE` / `DISABLE PIPELINE` plus object-store or Iceberg lifecycle. There is no Skippr TTL product.

## Process topology

OSS uses **three ingest processes** (one pipeline per signal). Query is on-demand (`sde query` or CloudQuery). There is no observe process.

```text
DATA_DIR=/data/traces skipprd sync --pipeline otel-traces
DATA_DIR=/data/logs   skipprd sync --pipeline otel-logs
DATA_DIR=/data/metrics skipprd sync --pipeline otel-metrics
sde query
```

Listen ports must not collide (see the example YAML).

## Configuration

```yaml
data_sources:
  otlp_traces:
    Otlp:
      listen_address_grpc: "0.0.0.0:4317"
      listen_address_http: "0.0.0.0:4318"
      signals: [traces]
      auth_token: "secret"
```

```bash
sde connect source otlp \
  --listen-address-grpc 0.0.0.0:4317 \
  --listen-address-http 0.0.0.0:4318 \
  --signals traces
```

Full three-pipeline example: [`examples/otel/skippr.yml`](../../../examples/otel/skippr.yml). Collector exporters: [`examples/otel/collector.yaml`](../../../examples/otel/collector.yaml).

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `listen_address_grpc` | `0.0.0.0:4317` | gRPC bind address |
| `listen_address_http` | `0.0.0.0:4318` | HTTP bind address |
| `signals` | `traces, logs, metrics` | Non-empty list. Unknown signals are rejected. |
| `auth_token` | | Optional Bearer token. Missing/wrong token → 401 / gRPC unauthenticated. |
| `attribute_allowlist` | keep all | `[]` drops every attribute map entry. Promotion columns still fill. |

## Namespaces

| Signal | Namespaces |
|---|---|
| traces | `spans`, `span_events`, `span_links` |
| logs | `log_records` |
| metrics | `gauge`, `sum`, `histogram`, `exponential_histogram` |

SQL catalog schema is the pipeline name (quote hyphens): `"otel-traces".spans`.

## Related

- [Observability UDFs](../../query/observability-udfs.md)
- [Iceberg output](../outputs/iceberg.md)
- [HTTP Server](http_server.md)
