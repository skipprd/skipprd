# HLA: Observability on Skippr (OTel Lakehouse + SQL/UDFs)

**Status:** design / implementation in progress  
**Implementation:** [`hla-observability-implementation-wbs.md`](./hla-observability-implementation-wbs.md)  
**Depends on (shipped in skipprd):** multi-node HA ingest, per-pipeline leases, sync WAL replication, Iceberg catalog, Iceberg∪WAL UNION (`IcebergWalUnionProvider`), Arrow Flight SQL, in-process Ballista — [`hla-flight-sql-ballista.md`](./hla-flight-sql-ballista.md).  
**Product front:** CloudQuery Execute of the same SQL/UDFs (Cloud later). skipprd has **no** `skippr observe`, no o11y HTTP API, no console, and no alarm evaluator.  
**Non-goals:** building another consensus/WAL stack; replacing customer paging products; platform-wide legal-hold/compliance product; shipping a full Datadog clone on day one

---

## 0. Verdict

With HA Skippr already in production, observability is primarily a **data model + UDF** problem on top of the existing ingest/query plane — not new distributed systems work. skipprd supplies OTLP ingest into Iceberg and generic SQL/UDFs. Charts, alarms, and consoles are CloudQuery Execute of those UDFs.

**In scope for skipprd:**

| Layer | Approach |
| --- | --- |
| Signals | OpenTelemetry metrics, logs, traces → Skippr pipelines → Iceberg |
| Hot path | Live WAL / Flight (already HA) |
| Historical | Iceberg / Parquet on object store |
| Signal UX shapes | SQL + **UDFs** (`otel_trace`, `otel_waterfall`, `otel_logs_tail`, `otel_rate`, …) |
| Alerting | Cloud schedules CloudQuery Execute of the cookbook SQL; skipprd does not store alarm state |
| Correlation | OTel identity model + SQL/API joins (trace ↔ span ↔ log ↔ metric) |
| AuthZ / tenancy | Existing platform ABAC |
| Compliance | Tenant/user-space responsibility |

**Explicitly out of Skippr’s “must build” list:** incident routing, on-call calendars, agent fleets as a product (customers run OTel Collector), exotic consensus for telemetry.

---

## 1. Product shape

```
OTel Collector / SDKs
        │
        ▼
Skippr ingest (HA, leased pipelines) ──► WAL (hot) ──► Iceberg (cold)
        │                                    │              │
        │                                    └──────┬───────┘
        │                                           ▼
        │                              Query engine (DF / Ballista)
        │                              + observability UDFs
        │                                           │
        │                                           ▼
        │                              skippr query / Flight / CloudQuery
        │                              + observability UDFs
        │                                           │
        └───────────────────────────────────────────┴──► Cloud UI / alarms (later)
```

Cloud UI is the sellable console; it is not where durability or multi-node correctness lives. skipprd does not ship a console.

---

## 2. Assumptions (already in prod)

- Per-pipeline exclusive leases; failover with fence + TTL + WAL/compaction catch-up.  
- Sync-replicated disk WAL (+ compaction hard-state) and/or S3 WAL as configured.  
- Query: lakehouse (Iceberg) ∪ live WAL via `IcebergWalUnionProvider`; clustered query is Arrow Flight SQL + in-process Ballista ([`hla-flight-sql-ballista.md`](./hla-flight-sql-ballista.md)).  
- Schema and Iceberg metadata on shared object store + catalog.  
- Platform already provides multi-tenant AuthZ (ABAC policy engine).

This HLA does **not** redesign those layers.

---

## 3. Data plane: OTel into Skippr

### 3.1 Pipelines / namespaces (illustrative)

| Signal | Example pipeline / namespaces | Notes |
| --- | --- | --- |
| Traces | `otel_traces` / `spans`, `span_events`, `span_links` | Partition by time + service; secondary access by `trace_id` |
| Logs | `otel_logs` / `log_records` | Time + severity + service; `trace_id` / `span_id` when present |
| Metrics | `otel_metrics` / `gauge`, `sum`, `histogram`, … | Prefer OTel metric identity; control rollup tables early |

Exact physical schemas follow OTel → Skippr discover/Arrow mapping; evolve via existing schema sinks.

### 3.2 Ingest path

- Prefer OTel Collector exporting to a Skippr-supported source (OTLP HTTP/gRPC source plugin, or bridge).  
- One logical pipeline (or signal-split pipelines) under normal lease/HA rules.  
- Redaction / attribute allowlists: Collector-side and/or Skippr transform inject — tenant policy, not a new subsystem.

### 3.3 Cardinality & cost

Object-store Parquet makes **retention** affordable (Influx-style “store everything” is plausible). It does **not** make unbounded explore queries free.

**Platform defaults (product engineering, not new storage):**

- Partition discipline (time, `service.name`, signal).  
- Optional continuous rollups (e.g. 1m/5m metric aggregates) as Skippr-derived pipelines or scheduled SQL.  
- API-level series limits / “top-N” for metric explore.  
- Trace sampling policy documented at Collector; backend accepts what it is given.

---

## 4. Query shapes via UDFs + glue API

Generic SQL is the engine; the console never depends on ad-hoc SQL strings alone.

### 4.1 UDF / table-function catalog (illustrative)

| Concern | Example surface | Implementation sketch |
| --- | --- | --- |
| Trace fetch | `otel_trace(trace_id)` → ordered spans | UDF/TVF over Iceberg + WAL union |
| Waterfall | `otel_waterfall(trace_id)` | Parent/child assembly in UDF or API layer |
| Log tail | `otel_logs_tail(service, since, filter)` | Time-bounded scan; WAL-first for recent |
| Metric step/rate | `otel_rate(metric, window)`, `otel_increase(...)` | DataFusion UDFs over gauge/sum/histogram |
| Histograms | `otel_histogram_quantile(p, …)` | UDF over explicit bucket columns |
| Correlation | helpers joining on `trace_id` / resource attrs | Thin SQL views + API |

### 4.2 Glue / proxy API

Stable HTTP/JSON (or Flight SQL) resources, e.g.:

- `GET /v1/traces/{trace_id}`  
- `GET /v1/logs?service=&from=&to=&q=`  
- `GET /v1/metrics/query` (range / instant)  
- `GET /v1/services`, `GET /v1/services/{name}/map` (dependency edges from span pairs)  
- ABAC enforced on every call using existing policy engine

Proxy translates to SessionContext SQL + UDFs against `lakehouse` ∪ `live_wal`. This is **application engineering**: routing, pagination, timeouts, cursor tokens — not distributed consensus.

### 4.3 Realtime

- Recent windows prefer **WAL/Flight**; older windows fall through to Iceberg.  
- Same dual-plane query model as the core HLA.  
- “Powerful UDFs and hooks” = known web/API patterns (websocket tail, polling, SSE) over that query plane.

---

## 5. Alerting & incident loop

**Skippr provides:**

- Alarm **definitions** (tenant-scoped): expression over metrics/logs/traces (SQL or named UDF presets), window, threshold, severity.  
- Continuous or interval **evaluation** against live+lake query.  
- Durable **alarm state** time series: `OK | ALARM | INSUFFICIENT_DATA`, transitions, reason payloads.  
- Webhook / SNS-like **notification hook** with state change events.

**Customers provide:**

- PagerDuty, Opsgenie, Slack, custom routers — same posture as CloudWatch alarms → customer paging.

**Not in v1 product scope:** full incident management, on-call schedules, timeline war rooms.

Evaluation workers are ordinary Skippr-adjacent services: they need read access to query + write alarm_state; HA is “run N evaluators with lease per alarm rule or shard,” reusing the **pipeline lease idea** if useful — still not a new WAL design.

---

## 6. Correlation UX

OTel’s resource + trace context is the join key:

- Log ↔ span via `trace_id` / `span_id`  
- Span ↔ metrics via `service.name`, route, exemplar trace ids when present  
- Service map from span client/server pairs over a time window  

Console flows are API compositions + visualization. Feasible without a proprietary telemetry model.

---

## 7. AuthZ / multi-tenant isolation

- Reuse existing platform **ABAC**.  
- Observability APIs and console attach the same policy decisions as other Skippr data planes (workspace/tenant/pipeline/table attributes).  
- No parallel auth stack for o11y.

---

## 8. Compliance

- **Tenant-managed:** retention knobs, Collector redaction, what attributes they emit.  
- Platform may expose retention TTLs / pipeline enable switches; immutability legal-hold is **not** assumed as a Skippr compliance product unless separately scoped.

---

## 9. Console (UI)

Minimum viable console on top of the API:

1. Service list / health overview  
2. Trace search + waterfall  
3. Log explorer (filter + deep link to trace)  
4. Metric explorer (bounded cardinality)  
5. Alarm list + state history + webhook config  

UI is a client of §4–§5 only.

---

## 10. What we are *not* claiming

| Claim | Reality |
| --- | --- |
| “Parquet on S3 solves cardinality” | Solves storage cost; query still needs rollups/limits |
| “UDFs make APM easy” | Makes shapes expressible; APIs + indexes/partitions still required |
| “We replace Datadog overnight” | We replace the **data + query + alarm-state** core; edge collectors and paging remain ecosystem |
| “No systems work left” | HA ingest/query already done; o11y is product/API/UDF/ops-default work |

---

## 11. Phased delivery

### Phase A — Ingest + schemas

- OTel source path; Iceberg tables for spans/logs/metrics.  
- Partition conventions documented.

### Phase B — Query UDFs + public API

- Core UDFs/TVFs; glue API with ABAC.  
- Dual-plane (WAL + Iceberg) wired for recent vs historical.

### Phase C — Console MVP

- Traces, logs, metrics explore; correlation links.

### Phase D — Alarm state

- Rule store, evaluator, alarm_state table, webhook export.

### Phase E — Hardening

- Rollup pipelines, series limits, SLO-ish dashboards optional; load tests on high-cardinality metric explore.

---

## 12. Success criteria

- Ingest OTel metrics, logs, and traces through HA Skippr into Iceberg with live WAL query.  
- Console can open a trace waterfall, jump to related logs, and chart a metric without raw SQL from the browser.  
- Alarm transitions are durable and webhook-deliverable to a customer pager.  
- All data access passes existing ABAC.  
- No new WAL/lease/catalog subsystem required beyond the core HA HLA already in prod.

---

## 13. Open decisions

1. OTLP native source plugin vs Collector → existing connector only.  
2. Single `otel_*` pipeline vs separate pipelines per signal.  
3. Metric rollup strategy: derived Skippr pipelines vs scheduled SQL materializations.  
4. Alarm evaluator colocated with Skippr nodes vs separate service.  
5. PromQL-subset compatibility layer vs SQL-only + UDFs for metrics API.
