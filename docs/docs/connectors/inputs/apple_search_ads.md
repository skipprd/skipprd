# Apple Search Ads

Ingest daily Apple Search Ads reporting from the Apple Search Ads API.

## Connect

Apple Search Ads uses Apple OAuth2 client credentials, not a browser redirect. Provide the organization ID and Apple API credentials:

```yaml
data_sources:
  apple_search_ads:
    AppleSearchAds:
      org_id: "123456"
      client_id: "${APPLE_SEARCH_ADS_CLIENT_ID}"
      team_id: "${APPLE_SEARCH_ADS_TEAM_ID}"
      key_id: "${APPLE_SEARCH_ADS_KEY_ID}"
      private_key_path: "${APPLE_SEARCH_ADS_PRIVATE_KEY_PATH}"
      start_date: "2024-01-01"
      stream_profile: full
```

For short-lived smoke tests you can provide `access_token` instead of client credentials.

## Options

| Field | Description |
| --- | --- |
| `org_id` | Apple Search Ads organization ID used in `X-AP-Context`. |
| `client_id` | Apple API client ID for JWT client-credentials auth. |
| `team_id` | Apple developer team ID used to sign the client secret JWT. |
| `key_id` | Apple private key ID used to sign the client secret JWT. |
| `private_key_pem` / `private_key_path` | `.p8` private key material or local path. |
| `access_token` | Optional static bearer token for smoke tests. |
| `start_date` / `end_date` | Inclusive report window. `end_date` defaults to yesterday minus processing lag. |
| `lookback_days` | Number of recent days to reprocess after a checkpoint. Default `3`. |
| `stream_profile` | `minimal`, `standard`, or `full`. Default `full`. |
| `time_zone` | Default report timezone. Search term reports use `ORTZ`. |
| `return_records_with_no_metrics` | Whether Apple should include zero-metric rows. Default `true`. |
| `max_concurrent_requests` | Fan-out report request concurrency. Default `8`. |
| `streams` | Optional explicit namespace list. |

## Streams

| Namespace | Profile |
| --- | --- |
| `apple_search_ads.campaign_daily` | minimal |
| `apple_search_ads.ad_group_daily` | standard+ |
| `apple_search_ads.keyword_daily` | full |
| `apple_search_ads.search_term_daily` | full |

## Authentication

Skipprd exchanges an **ES256 JWT** (signed with your Apple API private key) for a short-lived access token at `https://appleid.apple.com/auth/oauth2/token` (`grant_type=client_credentials`, `scope=searchadsorg`).

Required env vars (typical):

- `APPLE_SEARCH_ADS_ORG_ID`
- `APPLE_SEARCH_ADS_CLIENT_ID`
- `APPLE_SEARCH_ADS_TEAM_ID`
- `APPLE_SEARCH_ADS_KEY_ID`
- `APPLE_SEARCH_ADS_PRIVATE_KEY_PATH`

Optional: `APPLE_SEARCH_ADS_ACCESS_TOKEN` to skip JWT exchange in local debugging.

## Troubleshooting

| Symptom | Fix |
| --- | --- |
| Token / 401 errors | Verify `client_id`, `team_id`, `key_id`, and PEM path; regenerate key in Apple Search Ads UI |
| Empty keyword stream | Confirm campaigns and ad groups exist; check `max_concurrent_requests` and API rate limits |
| Search term errors | Ensure the plugin sends `ORTZ` (built-in for `search_term_daily`) |
| Slow discover | Expected — discover only samples 3 days of `campaign_daily`; use `skipprd sync` for full history |
| Stale metrics | Confirm `replace_partition`; increase `lookback_days` |

Offline dev: set `SKIPPR_APPLE_SEARCH_ADS_FIXTURE_DIR` to JSON fixtures (`campaign_report.json`, `ad_group_report.json`, etc.).
