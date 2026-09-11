# Observability soak notes

Operator notes for OTLP ingest volume and waterfall query latency. Not product knobs — ceilings stay in code.

## Ingest volume

OSS topology is three `skipprd sync` processes (see [OTLP input](../connectors/inputs/otlp.md)). Watch:

- WAL growth per `DATA_DIR` (`/data/traces`, `/data/logs`, `/data/metrics`)
- Iceberg commit lag vs OTLP HTTP/gRPC accept rate
- 16 MiB request rejects (`413` / gRPC resource exhausted) — increase Collector batch split, do not raise the ceiling
- Deadletter `_dl_*` Arrow batches (`failure_code=arrow_builder`) vs `400` on wire decode

A useful soak: Collector `memory_limiter` + `batch` processors feeding traces/logs/metrics endpoints concurrently for ≥30 minutes, then `sde query` cookbook statements.

## Waterfall p99

`otel_waterfall` / `otel_trace` are time-bounded (`ScanBudget`, default 24h) and capped (`MAX_TRACES=100`). p99 of `sde query` collect should stay under the 30s session timeout.

If p99 climbs:

- Confirm `hour` + `tenant_id` predicates so Iceberg identity partitions prune
- Confirm `trace_id` bloom is present on new Parquet files
- Narrow `from_ns`/`to_ns` from `otel_trace_search` hits instead of a 24h fetch

## High cardinality

`otel_rate` / `otel_increase` are per-row scalar UDFs. Cap series in SQL (`ORDER BY … LIMIT`) rather than scanning unbounded attribute combinations. MemTable harness covers 1000-series rate without HTTP.
