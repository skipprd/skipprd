# X Ads Input

Daily X (Twitter) Ads analytics at campaign and line-item grains.

## Configuration

```yaml
data_sources:
  x_ads:
    XAds:
      account_id: "abc123"
      start_date: "2024-01-01"
      stream_profile: full
      lookback_days: 3
      processing_lag_days: 1
      oauth_consumer_key: ${X_ADS_CONSUMER_KEY}
      oauth_consumer_secret: ${X_ADS_CONSUMER_SECRET}
      oauth_token: ${X_ADS_ACCESS_TOKEN}
      oauth_token_secret: ${X_ADS_ACCESS_TOKEN_SECRET}
```

| Field | Default | Description |
| --- | --- | --- |
| `account_id` | *(required)* | X Ads account ID |
| `ad_account_id` | | Alias for `account_id` when only the ads account is known |
| `start_date` | *(required)* | First report date (`YYYY-MM-DD`) |
| `end_date` | | Last report date |
| `lookback_days` | `3` | Mature days to re-fetch |
| `processing_lag_days` | `1` | Skip trailing immature days |
| `stream_profile` | `full` | `minimal`, `standard`, or `full` |
| `streams` | profile set | Explicit namespace list |
| `bearer_token` / `access_token` | | Bearer token (alternative to OAuth 1.0a) |
| `oauth_consumer_key` | | OAuth 1.0a consumer key |
| `oauth_consumer_secret` | | OAuth 1.0a consumer secret |
| `oauth_token` | | OAuth 1.0a access token |
| `oauth_token_secret` | | OAuth 1.0a access token secret |

## Pipeline wiring

```yaml
pipelines:
  x_ads:
    data_source: data_sources.x_ads
    data_sink: data_sinks.landing
```

## Authentication

X Ads accepts OAuth 1.0a (`oauth_consumer_key`, `oauth_consumer_secret`, `oauth_token`, `oauth_token_secret`) or a bearer `access_token`. Store those values in the environment.
