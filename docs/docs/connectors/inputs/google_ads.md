# Google Ads Input

Daily Google Ads reporting grains from the Google Ads API. One bronze namespace per report stream.

## Configuration

```yaml
data_sources:
  google_ads:
    GoogleAds:
      customer_id: "1234567890"
      developer_token: ${GOOGLE_ADS_DEVELOPER_TOKEN}
      start_date: "2024-01-01"
      stream_profile: full
      lookback_days: 3
      processing_lag_days: 1
      oauth_client_id: ${GOOGLE_ADS_CLIENT_ID}
      oauth_client_secret: ${GOOGLE_ADS_CLIENT_SECRET}
      oauth_refresh_token: ${GOOGLE_ADS_REFRESH_TOKEN}
```

| Field | Default | Description |
| --- | --- | --- |
| `customer_id` | *(required)* | Google Ads customer ID (no dashes) |
| `developer_token` | *(required)* | API developer token |
| `login_customer_id` | | Manager account ID when querying client accounts |
| `start_date` | *(required)* | First report date (`YYYY-MM-DD`) |
| `end_date` | | Last date; omit for through yesterday minus lag |
| `lookback_days` | `3` | Mature days to re-fetch |
| `processing_lag_days` | `1` | Skip trailing immature days |
| `stream_profile` | `full` | `minimal`, `standard`, or `full` |
| `streams` | profile set | Explicit namespace list; overrides profile |
| `api_version` | | Google Ads API version override |
| `access_token` | | Static bearer token (alternative to OAuth refresh) |
| `oauth_*` | | OAuth refresh credentials |

## Pipeline wiring

```yaml
pipelines:
  ads:
    data_source: data_sources.google_ads
    data_sink: data_sinks.landing
```

See [Source landing semantics](../../concepts/source-landing-semantics.md) for mutable report re-sync behavior.

## Authentication

Use a Google Ads developer token plus OAuth refresh (or a short-lived `access_token`). Put secrets in the environment:

- `GOOGLE_ADS_DEVELOPER_TOKEN`
- `GOOGLE_ADS_CLIENT_ID`
- `GOOGLE_ADS_CLIENT_SECRET`
- `GOOGLE_ADS_REFRESH_TOKEN`

The Google account must have access to the Ads customer (and manager account when `login_customer_id` is set).
