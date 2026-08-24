# Observability SQL UDFs

Generic traces, logs, and metrics helpers registered on every user-SQL `SessionContext` (`skippr query`, Flight SQL, Ballista). Charts, log explorers, waterfalls, and alarms are **CloudQuery Execute** of this SQL — there is no `skippr observe` command.

Empty or null UDF results are **insufficient data** on the Cloud side. skipprd does not store alarm state.

Lookback ceiling is 24 hours (`ScanBudget`). Query collect timeout is 30 seconds. Series/row caps (`MAX_SERIES=1000`, `MAX_TRACES=100`, `MAX_LOG_ROWS=1000`, `MAX_SERVICES=500`, `MAX_EDGES=200`) set `truncated` rather than returning unbounded results.

## Cookbook

Checked-in examples: [`examples/otel/cookbook.sql`](../../../examples/otel/cookbook.sql).

### Traces

```sql
-- Search roots, then fetch / waterfall with the hit's time window.
SELECT * FROM otel_trace_search('checkout', from_ns, to_ns, true, NULL);
SELECT * FROM otel_trace('4bf92f3577b34da6a3ce929d0e0e4736', from_ns, to_ns);
SELECT * FROM otel_waterfall('4bf92f3577b34da6a3ce929d0e0e4736', from_ns, to_ns);
```

### Logs

```sql
SELECT * FROM otel_logs_tail('checkout', from_ns, to_ns, 'timeout');
SELECT * FROM otel_logs_for_trace(trace_id, from_ns, to_ns);
```

### Metrics / charts / alarm probes

```sql
SELECT service_name, metric_attributes, otel_rate(value, time_unix_nano, window_ns) AS rate
FROM "otel-metrics".sum
WHERE tenant_id = ? AND metric_name = ? AND hour BETWEEN ? AND ?;

SELECT count(*) FROM "otel-logs".log_records
WHERE tenant_id = ? AND service_name = ? AND severity_number >= 17
  AND time_unix_nano BETWEEN ? AND ?;

SELECT duration_nano FROM otel_trace(...) WHERE parent_span_id IS NULL;
```

When a window would exceed the 24h ScanBudget (`RangeTooWide`), query the 1m/5m rollup grain (`sum_1m` / `sum_5m`) instead of raw `sum`. Do not scan unbounded raw data. Top-N series: `ORDER BY abs(rate) DESC` then name, capped at `MAX_SERIES=1000` with `truncated`.

`otel_histogram_quantile` interpolates explicit-bucket histograms. Exponential histograms are stored but quantile is a typed error until a later grain.

### Services

```sql
SELECT * FROM otel_services(from_ns, to_ns);
SELECT * FROM otel_service_map('checkout', from_ns, to_ns);
```

Optional `tenant_id` is a TVF argument (shared pipelines) or a `WHERE` on the scan (OSS tables already isolated by pipeline).
