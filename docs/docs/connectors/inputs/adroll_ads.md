# AdRoll Ads Input

Daily AdRoll advertising reports at advertiser and campaign grains.

## Configuration

```yaml
data_sources:
  adroll:
    AdrollAds:
      advertiser_id: "abc123"
      start_date: "2024-01-01"
      stream_profile: full
      lookback_days: 3
      processing_lag_days: 1
      personal_access_token: ${ADROLL_PAT}
```

| Field | Default | Description |
| --- | --- | --- |
| `advertiser_id` | *(required)* | AdRoll advertiser EID |
| `start_date` | *(required)* | First report date (`YYYY-MM-DD`) |
| `end_date` | | Last report date |
| `lookback_days` | `3` | Mature days to re-fetch |
| `processing_lag_days` | `1` | Skip trailing immature days |
| `stream_profile` | `full` | `minimal`, `standard`, or `full` |
| `streams` | profile set | Explicit namespace list |
| `personal_access_token` | | AdRoll personal access token |
| `access_token` | | Bearer token |
| `oauth_*` | | OAuth refresh credentials |
| `api_base_url` | | API base URL override |
| `reporting_base_url` | | Reporting API base URL override |

## Pipeline wiring

```yaml
pipelines:
  adroll:
    data_source: data_sources.adroll
    data_sink: data_sinks.landing
```
