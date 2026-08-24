# Implementation WBS: Observability (OTel lakehouse + SQL/UDFs)

**Architecture:** [`hla-observability-otel-console.md`](./hla-observability-otel-console.md)  
**Query substrate:** [`hla-flight-sql-ballista.md`](./hla-flight-sql-ballista.md) (shipped)  
**Style:** [`AGENTS.md`](../../../AGENTS.md)  
**Boundary:** [`repository-map.md`](./repository-map.md)

This WBS is the implementation contract for Skipprd OTel ingest and generic SQL/UDFs. Charts, alarms, and consoles are **CloudQuery Execute** of those UDFs (Cloud later). There is **no** `skippr observe`, no skipprd HTTP o11y API, no skipprd console, and no skipprd alarm evaluator.

skipprd stays JWT-ignorant. Fully functional on `skippr sync` + `skippr query` (and clustered Flight / CloudQuery).

## Completion rules for every work unit

- Compile-time invariants first: newtypes, enums, exhaustive matches, typed errors.
- Host MUST NOT depend on `plugins/data_source/otlp`.
- Do not add `plugins/data_sink/otel`. Iceberg is the dest.
- OTLP plugin MUST NOT call Iceberg `Catalog` / `DataSink` APIs. Lake writes are host WAL via existing Arrow IPC (`ingest_runtime_batches_into_core`).
- UDF code is keyed by `PipelineConfigView` / `register_namespace_view` — never `Config::get_pipeline_name()` or `PIPELINE_NAME`.
- No env/config knobs for payload ceilings, series caps, lookback, or timeouts (code constants).
- No `skippr observe`, `src/observe/`, `console/`, or skipprd alarm store.
- No one-implementor trait hierarchy for telemetry.
- No new WAL, lease protocol, catalog, or Ballista scheduler. Registering UDFs on the existing `SessionStateBuilder` is in scope (B.26).
- No work unit is complete without its named deterministic tests.

## Locked decisions

1. Native OTLP **DataSource** plugin (`plugin_name = "Otlp"`), HTTP `:4318` and gRPC `:4317`. Collector is the agent. Dest is existing Iceberg DataSink.
2. Three signal pipelines: `otel-traces`, `otel-logs`, `otel-metrics`. SQL catalog schema = pipeline name (quote hyphens).
3. **SQL + UDFs only.** No PromQL. No skipprd o11y HTTP.
4. Iceberg partition = **identity** on bronze `hour` + `service_name` + `tenant_id`. No Iceberg `hour()` transform.
5. `trace_id` lookup is time-bounded (ScanBudget, default 24h).
6. Clustered discover stays rejected. Schema from **host-seeded `OutputMetadata`**.
7. Dual-plane is shipped (`IcebergWalUnionProvider`). UDFs run on that provider. No listing fallback.
8. skipprd JWT-ignorant. Tenant prune is SQL `WHERE tenant_id = …`.
9. **Arrow IPC ingest:** plugin decodes OTLP binary (or OTLP JSON → proto) to bronze structs, then Arrow builders. Submit `RuntimeIngestPartitionBatch` via `submit_arrow_ipc_batches`. MUST NOT emit `IngestBatch` NDJSON. MUST NOT add a serde path in `ingest_work.rs`.
10. Dead letters: invalid wire → HTTP 400 / gRPC invalid argument. Builder/schema failure → Arrow batch on `_dl_{pipeline}` with existing deadletter columns.
11. `inject_fields` for empty `tenant_id` is applied **in the plugin** (Arrow path skips ingest_work JSON inject).

## Constants (code, not env)

- `OTLP_MAX_REQUEST_BYTES`: 16 MiB
- `OTEL_DEFAULT_LOOKBACK`: 24h
- `OTEL_QUERY_TIMEOUT`: 30s
- `MAX_SERIES`: 1000
- `MAX_TRACES`: 100
- `MAX_LOG_ROWS`: 1000
- `MAX_SERVICES`: 500
- `MAX_EDGES`: 200

## Planned layout

```text
plugins/data_source/otlp/          # DataSource only
  src/{lib,main,config,bronze,decode,arrow,http,grpc,sync}.rs
  proto/                           # OTLP protos for tonic-build
  tests/fixtures/
plugins/data_sink/iceberg/         # A.19/A.20 partition spec + write values
src/sqlrt/session.rs
src/sqlrt/schema_seed.rs
src/sqlrt/udfs/
examples/otel/
docs/docs/connectors/inputs/otlp.md
docs/docs/query/observability-udfs.md
```

## Phase 0 — Contract, session

| WU | Must | Tests |
| --- | --- | --- |
| 0.1 | This file + HLA header + maintainer nav | docs |
| 0.2 | Cancelled — no `Mode::Observe` | — |
| 0.3 | `build_query_context` used by `query.rs` / `engine.rs` | `SELECT 1`; no direct `SessionContext::new_with_config` in those files |
| 0.4 | `src/sqlrt/udfs` MUST NOT mention `PIPELINE_NAME` | grep guard; `PipelineConfigView::for_name` missing → error |
| 0.5 | Checked-in OTLP protobuf + JSON fixtures | parse in A.7–A.9 |

## Phase A — Ingest + schemas

| WU | Must | Tests |
| --- | --- | --- |
| A.1 | Crate `skippr-plugin-data-source-otlp`, `plugin_name = "Otlp"`, HostIdleBounded | crate builds; metadata name |
| A.2 | Typed `OtlpConfig`; empty signals rejected; deny unknown keys | decode defaults; reject empty/unknown |
| A.3–A.5 | Bronze types with newtypes; events/links separate records; histogram bounds required | hex round-trip; serde goldens; bounds mismatch |
| A.6 | Promote `service.name`, `deployment.environment`, `http.route`, `tenant.id`; plugin inject_fields for empty tenant_id | present/absent/wrong-type |
| A.7–A.9 | One decode for HTTP protobuf, HTTP JSON, gRPC | fixtures |
| A.10 | Attribute allowlist; promotion first | keep/drop keys |
| A.11–A.14 | HTTP `/v1/traces\|logs\|metrics`, JSON content-type, gRPC, bearer, 16 MiB | 401/413/signal filter |
| A.15 | Contracts + Arrow IPC emit (not IngestBatch JSON) | traces → three namespaces; logs-only no spans |
| A.16 | `skippr connect source otlp` | `--help`; plugin name `Otlp` |
| A.17 | Example three-pipeline YAML | Config parse |
| A.18 | Discover goldens from bronze/Arrow + `otel_columns.txt` | names match |
| A.19 | Iceberg identity PartitionSpec on hour/service_name/tenant_id when those fields exist | create table has spec |
| A.20 | Data-file partition values not `Struct::empty()` when spec exists | written files carry values |
| A.21 | Connector docs + Collector example | docs |
| A.22 | Capability catalog + host boundary | `by_name("Otlp")`; host does not depend on crate |
| A.23 | Seed OutputMetadata on sync when missing | clustered discover still rejected |
| A.24 | OSS three-process topology notes | docs |
| A.25 | Bronze → Arrow builders; schema equals seed | IPC round-trip; column names |
| A.26 | Deadletter Arrow on builder failure; 400 on wire decode | `_dl_*` failure_code |
| A.27 | SDK `submit_arrow_ipc_batches` | empty / one batch; no ingest_work.rs change |

## Phase B — SQL / UDFs

| WU | Must | Tests |
| --- | --- | --- |
| B.1 | `register_observability_udfs` on session factory | compiles |
| B.2 | ScanBudget: required from/to or 24h ceiling | unbounded fails closed |
| B.3 | `otel_trace` TVF | fixture spans |
| B.4 | `otel_waterfall` TVF | parent/child/depth |
| B.5 | `otel_logs_tail` / `otel_logs_for_trace` | filter + trace join |
| B.6–B.8 | `otel_rate` / `otel_increase` / `otel_histogram_quantile` | monotonic; empty; quantile |
| B.9 | UDFs on IcebergWalUnionProvider; no listing fallback | existing union tests |
| B.15 | Cookbook SQL | `examples/otel/cookbook.sql` |
| B.16–B.17 | `otel_services` / `otel_service_map` | caps |
| B.18 | Row caps + `truncated` column | overflow sets truncated |
| B.19 | Session timeout constant | factory / Ballista |
| B.21 | MemTable golden harness | waterfall/search/rate |
| B.22 | TUI UDF name list | names match registry |
| B.23 | `otel_trace_search` TVF | service/time/error |
| B.26 | Register UDFs on Ballista `SessionStateBuilder` **before** `BallistaFunctionRegistry::from` | names present |

B.10–B.14, B.20 cancelled (no skipprd HTTP/SSE).

## Phase C / D

Cancelled. Cloud UI and Cloud-scheduled CloudQuery own consoles and alarms.

## Phase E — Hardening

| WU | Must | Tests |
| --- | --- | --- |
| E.1–E.3 | 1m/5m rollup example pipelines + cookbook grain | YAML + docs |
| E.4 | Top-N in metrics TVF/cookbook | cap |
| E.5 | Parquet bloom on `trace_id` when the column exists | writer props |
| E.6 | High-cardinality UDF load harness | MemTable |
| E.7 | Maintainer soak notes | docs |
| E.8 | Cancelled (Cloud JWT/ABAC) | — |
| E.9 | Exponential histogram namespace | decode + store; quantile UDF typed error until later |

## Process topology (A.24)

```text
DATA_DIR=/data/traces skippr sync --pipeline otel-traces
DATA_DIR=/data/logs   skippr sync --pipeline otel-logs
DATA_DIR=/data/metrics skippr sync --pipeline otel-metrics
skippr query
```
