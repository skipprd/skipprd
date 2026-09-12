# Meta Ads Input

Daily Meta (Facebook) Ads insights at campaign, ad set, and ad levels.

## Configuration

```yaml
data_sources:
  meta_ads:
    MetaAds:
      ad_account_id: "act_123456789"
      start_date: "2024-01-01"
      stream_profile: full
      lookback_days: 3
      processing_lag_days: 1
      access_token: ${META_ADS_ACCESS_TOKEN}
```

| Field | Default | Description |
| --- | --- | --- |
| `ad_account_id` | *(required)* | Meta ad account ID (`act_…`) |
| `start_date` | *(required)* | First report date (`YYYY-MM-DD`) |
| `end_date` | | Last date; omit for through yesterday minus lag |
| `lookback_days` | `3` | Mature days to re-fetch |
| `processing_lag_days` | `1` | Skip trailing immature days |
| `stream_profile` | `full` | `minimal`, `standard`, or `full` |
| `instagram_filter` | `false` | When true, restrict to Instagram placement insights |
| `streams` | profile set | Explicit namespace list |
| `api_version` | | Graph API version override |
| `access_token` | | Long-lived user or system token |
| `oauth_*` | | OAuth refresh credentials |

## Pipeline wiring

```yaml
pipelines:
  meta:
    data_source: data_sources.meta_ads
    data_sink: data_sinks.landing
```

## Authentication

Provide a long-lived Marketing API token with ads read access (`META_ADS_ACCESS_TOKEN`) or OAuth refresh fields (`oauth_client_id`, `oauth_client_secret`, `oauth_refresh_token`). Do not commit tokens in git.
