# Bing Webmaster Tools Input

Curated **daily search and crawl reports** from the [Bing Webmaster API](https://learn.microsoft.com/en-us/bingwebmaster/). One bronze namespace per report grain; warehouse SQL builds rollups.

## Configuration

```yaml
data_sources:
  bing:
    BingWebmasterTools:
      site_url: "https://example.com/"
      start_date: "2026-03-01"
      stream_profile: full
      lookback_days: 3
      processing_lag_days: 3
      api_key: ${BING_WEBMASTER_TOOLS_API_KEY}
```

| Variable | Default | Description |
| --- | --- | --- |
| `site_url` | *(required)* | Verified site URL in Bing Webmaster Tools |
| `api_key` | — | API key from Settings → API Access (preferred for CLI) |
| `start_date` | *(required)* | First date (`YYYY-MM-DD`) |
| `stream_profile` | `full` | `minimal`, `standard`, or `full` |
| `lookback_days` | `3` | Mature days to re-fetch |
| `processing_lag_days` | `3` | Skip last N calendar days while Bing finalizes data |
| `access_token` | — | OAuth bearer token (alternative to API key) |
| `oauth_*` | — | OAuth refresh credentials for delegated access |

## CLI

```bash
skippr connect source bing-webmaster-tools \
  --site-url "https://example.com/" \
  --start-date 2026-03-01 \
  --api-key ${BING_WEBMASTER_TOOLS_API_KEY}
```

## Fixtures

`SKIPPR_BING_WEBMASTER_TOOLS_FIXTURE_DIR` — see `plugins/data_source/bing_webmaster_tools/tests/fixtures/`.
