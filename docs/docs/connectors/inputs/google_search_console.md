# Google Search Console Input

Curated **daily Search Analytics** facts from the [Search Console API](https://developers.google.com/webmaster-tools/v1/api_reference_index). One bronze namespace per dimension grain; warehouse SQL builds rollups.

## Configuration

```yaml
data_sources:
  gsc:
    GoogleSearchConsole:
      site_url: "https://example.com/"
      start_date: "2024-01-01"
      stream_profile: full
      lookback_days: 3
      processing_lag_days: 3
      access_token: ${GSC_ACCESS_TOKEN}
```

| Variable | Default | Description |
| --- | --- | --- |
| `site_url` | *(required)* | URL-prefix (`https://example.com/`) or domain (`sc-domain:example.com`) |
| `start_date` | *(required)* | First date (`YYYY-MM-DD`) |
| `stream_profile` | `full` | `minimal`, `standard`, or `full` |
| `lookback_days` | `3` | Mature days to re-fetch |
| `processing_lag_days` | `3` | Skip last N days (`final` data lag ~2–3 days) |
| `search_type` | `web` | `web`, `image`, `video`, `news`, `discover`, `googleNews` |
| `data_state` | `final` | `final` or `all` |

Public connector docs: [Google Search Console](https://docs.skippr.io/connectors/sources/google-search-console).

## CLI

```bash
skipprd connect data-source google-search-console \
  --pipeline gsc \
  --name gsc \
  --site-url "https://example.com/" \
  --start-date 2024-01-01 \
  --access-token '${GSC_ACCESS_TOKEN}'
```

## Fixtures

`SKIPPR_GOOGLE_SEARCH_CONSOLE_FIXTURE_DIR` — see `plugins/data_source/google_search_console/tests/fixtures/`.
