# Meta Instagram Ads Input

Daily Instagram placement insights from the Meta Marketing API.

## Connect

```bash
skippr connect source meta-instagram-ads \
  --ad-account-id act_123456789 \
  --start-date 2024-01-01 \
  --access-token ${META_ADS_ACCESS_TOKEN}
```

## Configuration

```yaml
data_sources:
  meta_instagram:
    MetaInstagramAds:
      ad_account_id: "act_123456789"
      start_date: "2024-01-01"
      stream_profile: full
      lookback_days: 3
      processing_lag_days: 1
      access_token: ${META_ADS_ACCESS_TOKEN}
```

| Field | Default | Description |
| --- | --- | --- |
| `ad_account_id` | *(required)* | Meta ad account ID |
| `start_date` | *(required)* | First report date (`YYYY-MM-DD`) |
| `end_date` | | Last report date |
| `lookback_days` | `3` | Mature days to re-fetch |
| `processing_lag_days` | `1` | Skip trailing immature days |
| `stream_profile` | `full` | `minimal`, `standard`, or `full` |
| `streams` | profile set | Explicit namespace list |
| `api_version` | | Graph API version override |
| `access_token` | | Long-lived access token |
| `oauth_*` | | OAuth refresh credentials |

## Pipeline wiring

```yaml
pipelines:
  instagram:
    data_source: data_sources.meta_instagram
    data_sink: data_sinks.landing
```
