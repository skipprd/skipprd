# Meta Instagram Ads Input

Daily Instagram placement insights from the Meta Marketing API.

## Connect

Set `ad_account_id` and `access_token` (or OAuth refresh fields) in `skippr.yml`. Credentials belong in the environment, not in git.

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

## Authentication

Provide either:

- **`access_token`** / `META_INSTAGRAM_ADS_ACCESS_TOKEN` — long-lived Marketing API token with `ads_read` (and related) permissions, or
- **OAuth refresh** — `oauth_token_url`, `oauth_client_id`, `oauth_client_secret`, and `oauth_refresh_token` (defaults to Meta’s token endpoint when configured in `skippr.yml`).

Required env vars (typical):

- `META_AD_ACCOUNT_ID`
- `META_INSTAGRAM_ADS_ACCESS_TOKEN`

## Troubleshooting

| Symptom | Fix |
| --- | --- |
| Token / 401 errors | Regenerate a long-lived token; confirm `ads_read` on the ad account |
| Empty streams | Verify campaigns ran on Instagram; check `instagram_filter` |
| Rate limits (429) | Plugin retries with backoff; reduce parallel jobs if needed |
| Slow discover | Expected — discover only samples 3 days of `account_daily`; use `skipprd sync` for full history |
| Stale metrics | Confirm `replace_partition`; increase `lookback_days` |

Offline dev: set `SKIPPR_META_INSTAGRAM_ADS_FIXTURE_DIR` to JSON fixtures (`account_insights.json`, `campaign_insights.json`, etc.).
