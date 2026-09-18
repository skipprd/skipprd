# Google Search Console source plugin

Runtime data source plugin: `plugins/data_source/google_search_console/`.

| Item | Value |
| --- | --- |
| Crate | `skippr-plugin-data-source-google-search-console` |
| `plugin_name` | `GoogleSearchConsole` |
| Config `kind` | `google_search_console` |
| Namespace prefix | `google_search_console.*` |

## API surface

- **Search Analytics:** `POST /webmasters/v3/sites/{siteUrl}/searchAnalytics/query` with percent-encoded `siteUrl`, `startRow` pagination, `rowLimit` ≤ 25 000.
- **Sitemaps:** `GET /webmasters/v3/sites/{siteUrl}/sitemaps` → `google_search_console.sitemap_daily`.
- **URL Inspection:** `POST /v1/urlInspection/index:inspect` when `url_inspection_enabled` and `url_list` (capped at 20 URLs/run).

Auth: OAuth refresh, static `access_token`, or service account (`webmasters.readonly`). Property id is `site_url` (URL-prefix with trailing slash, or `sc-domain:example.com`).

## Streams

| Profile | Namespaces |
| --- | --- |
| `minimal` | `site_daily` |
| `standard` | `site_daily`, `query_daily`, `page_daily`, `device_daily`, `country_daily` |
| `full` | All nine curated namespaces (see `streams.rs` `FULL_STREAM_COUNT`) |

`search_appearance_daily` is **optional** — HTTP 400 skips that stream only.

Discover mode: last 3 days × `minimal` only; does not advance checkpoints.

## Config (engine)

```yaml
data_sources:
  gsc:
    GoogleSearchConsole:
      site_url: "https://example.com/"
      start_date: "2024-01-01"
      stream_profile: full
      lookback_days: 3
      processing_lag_days: 3
      search_type: web
      data_state: final
      access_token: ${GSC_ACCESS_TOKEN}
```

## Fixtures (CI)

Set `SKIPPR_GOOGLE_SEARCH_CONSOLE_FIXTURE_DIR` to a directory containing:

- `search_analytics_{site_slug}_{dimensions}_{start}_{end}.json`
- `sitemaps_{site_slug}.json`
- `url_inspection_{url_slug}.json` (optional)

See `plugins/data_source/google_search_console/tests/fixtures/`.

## CLI

```bash
skipprd connect data-source google-search-console \
  --pipeline gsc \
  --name gsc \
  --site-url "https://example.com/" \
  --start-date 2024-01-01 \
  --stream-profile standard \
  --access-token '${GSC_ACCESS_TOKEN}'
```

## Checklist

- [x] `source_namespace_contracts()` + `replace_partition` on `date`
- [x] Per-namespace checkpoints (`last_completed_date`)
- [x] `skippr_plugin_name`: `google_search_console` → `GoogleSearchConsole`

See [API / SaaS source plugins](api-saas-source-plugins.md) and [GA4 plugin](../connectors/inputs/google_analytics.md) for shared patterns.
