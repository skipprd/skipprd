# GA4 offline fixtures

Used when `SKIPPR_GA4_FIXTURE_DIR` points at this directory.

Naming: `{namespace_with_underscores}_{YYYYMMDD}.json`  
Example: `google_analytics_events_daily_20240101.json`

Each file is a GA4 Data API `runReport` response body with a `rows` array.
