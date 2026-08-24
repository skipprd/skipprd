-- Observability cookbook. Bind ? from CloudQuery / skippr query.
-- from_ns / to_ns must fit ScanBudget (default 24h). Empty/null UDF results = insufficient data.

-- Trace search → fetch / waterfall (pass the hit's window).
SELECT trace_id, root_name, start_time_unix_nano, end_time_unix_nano, duration_nano, error, truncated
FROM otel_trace_search('checkout', 0, 10000000000, true, NULL);

SELECT * FROM otel_trace('4bf92f3577b34da6a3ce929d0e0e4736', 0, 10000000000);
SELECT * FROM otel_waterfall('4bf92f3577b34da6a3ce929d0e0e4736', 0, 10000000000);

-- Logs
SELECT * FROM otel_logs_tail('checkout', 0, 10000000000, 'timeout');
SELECT * FROM otel_logs_for_trace('4bf92f3577b34da6a3ce929d0e0e4736', 0, 10000000000);

-- Alarm: error log count
SELECT count(*) AS error_logs
FROM log_records
WHERE service_name = 'checkout' AND severity_number >= 17
  AND time_unix_nano BETWEEN 0 AND 10000000000;

-- Alarm: root-span duration
SELECT duration_nano
FROM otel_trace('4bf92f3577b34da6a3ce929d0e0e4736', 0, 10000000000)
WHERE parent_span_id IS NULL;

-- Charts: rate / increase / quantile on raw sum/histogram
SELECT service_name, otel_rate(value, time_unix_nano, 60000000000) AS rate
FROM sum
WHERE metric_name = 'http.server.duration' AND hour BETWEEN 0 AND 1;

SELECT otel_increase(value, time_unix_nano) AS inc
FROM sum
WHERE metric_name = 'http.server.request.count';

SELECT otel_histogram_quantile(0.99, bucket_counts, explicit_bounds) AS p99
FROM histogram
WHERE metric_name = 'http.server.duration';

-- When the window would exceed ScanBudget, use 1m/5m rollup grain instead of raw sum.
-- SELECT service_name, value FROM "otel-metrics-1m".sum WHERE hour BETWEEN ? AND ?;
-- SELECT service_name, value FROM "otel-metrics-5m".sum WHERE hour BETWEEN ? AND ?;

-- Top-N series
SELECT service_name, otel_rate(value, time_unix_nano, 60000000000) AS rate
FROM sum
ORDER BY rate DESC NULLS LAST
LIMIT 20;

-- Services / map
SELECT service_name, last_seen_unix_nano, error_spans, truncated FROM otel_services(0, 10000000000);
SELECT from_service, to_service, truncated FROM otel_service_map('checkout', 0, 10000000000);
