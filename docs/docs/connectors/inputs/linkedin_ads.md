# LinkedIn Ads Input

Daily LinkedIn Campaign Manager analytics at account, campaign, and creative grains.

## Configuration

```yaml
data_sources:
  linkedin_ads:
    LinkedInAds:
      ad_account_id: "123456789"
      start_date: "2024-01-01"
      stream_profile: full
      lookback_days: 3
      processing_lag_days: 1
      access_token: ${LINKEDIN_ADS_ACCESS_TOKEN}
```

| Field | Default | Description |
| --- | --- | --- |
| `ad_account_id` | *(required)* | LinkedIn ad account ID |
| `start_date` | *(required)* | First report date (`YYYY-MM-DD`) |
| `end_date` | | Last report date |
| `lookback_days` | `3` | Mature days to re-fetch |
| `processing_lag_days` | `1` | Skip trailing immature days |
| `stream_profile` | `full` | `minimal`, `standard`, or `full` |
| `streams` | profile set | Explicit namespace list |
| `rest_version` | | LinkedIn REST API version |
| `api_version` | | Legacy API version override |
| `access_token` | | OAuth bearer token |
| `oauth_*` | | OAuth refresh credentials |

## Pipeline wiring

```yaml
pipelines:
  linkedin:
    data_source: data_sources.linkedin_ads
    data_sink: data_sinks.landing
```
