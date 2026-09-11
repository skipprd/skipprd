# Google Analytics (GA4) Input

Curated **daily fact grains** from the GA4 Data API (`runReport`). One bronze namespace per stable dimension set; warehouse SQL builds rollups and reports.

Not the GA4 BigQuery event export. See [GA4 bronze & modeling](../../concepts/ga4-bronze-and-modeling.md).

## Configuration

```yaml
data_sources:
  ga4:
    GoogleAnalytics:
      property_id: "123456789"
      start_date: "2024-01-01"
      stream_profile: full
      lookback_days: 7
      processing_lag_days: 1
      window_in_days: 1
      keep_empty_rows: true
      access_token: ${GA4_ACCESS_TOKEN}
```

| Variable | Default | Description |
| --- | --- | --- |
| `property_id` | *(required)* | GA4 property ID |
| `start_date` | *(required)* | First date (`YYYY-MM-DD`) |
| `end_date` | | Last date; omit for through yesterday minus lag |
| `stream_profile` | `full` | `minimal` (4), `standard` (16), `full` (23) |
| `streams` | profile set | Explicit namespace list; overrides profile |
| `lookback_days` | `3` | Mature days to re-fetch |
| `processing_lag_days` | `1` | Skip last N calendar days |
| `window_in_days` | `1` | Days per API date range; >1 risks sampling |
| `keep_empty_rows` | `true` | Zero-metric dimension rows in API response |
| Auth fields | | `access_token`, OAuth refresh, or service account |

### Accuracy knobs

| Setting | Purpose |
| --- | --- |
| `replace_partition` + `lookback_days` | Correct revised mature days |
| `processing_lag_days` | Avoid immature trailing days |
| `window_in_days: 1` | Minimize sampling (default) |

### Stream profiles

- **full** — 23 namespaces (production default)
- **standard** — 16 (no demographics, ecommerce, publisher ads)
- **minimal** — 4 acquisition/event tables (dev/CI)

### Namespace catalog

See the full table in the [public connector docs](https://docs.skippr.io/connectors/sources/google-analytics) or [GA4 bronze & modeling](../../concepts/ga4-bronze-and-modeling.md).

Ecommerce and publisher-ads namespaces are **optional**: invalid dimension/metric errors skip that stream for the run.

### Landing semantics

[Source landing semantics](../../concepts/source-landing-semantics.md). Pair with [Athena](../outputs/athena.md).

## Authentication

Full step-by-step guides (service account, OAuth refresh, bearer token) are on the public docs:

- [Service account](https://docs.skippr.io/connectors/sources/google-analytics#service-account)
- [OAuth refresh](https://docs.skippr.io/connectors/sources/google-analytics#oauth-refresh)

## CLI

```bash
sde connect source google-analytics \
  --property-id 123456789 \
  --start-date 2024-01-01 \
  --stream-profile full \
  --lookback-days 7 \
  --access-token ${GA4_ACCESS_TOKEN}
```

## Development fixtures

`SKIPPR_GA4_FIXTURE_DIR` — JSON files `{namespace_with_dots_as_underscores}_{YYYYMMDD}.json` (example: `google_analytics_events_daily_20240101.json`). See `plugins/data_source/google_analytics/tests/fixtures/`.
