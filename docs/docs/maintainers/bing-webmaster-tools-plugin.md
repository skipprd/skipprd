# Bing Webmaster Tools source plugin

Runtime data source plugin: `plugins/data_source/bing_webmaster_tools/`.

| Item | Value |
| --- | --- |
| Crate | `skippr-plugin-data-source-bing-webmaster-tools` |
| `plugin_name` | `BingWebmasterTools` |
| Config `kind` | `bing_webmaster_tools` |
| Namespace prefix | `bing_webmaster_tools.*` |

## API surface

Read-only JSON endpoints on `https://ssl.bing.com/webmaster/api.svc/json/`:

| Method | Namespace |
| --- | --- |
| `GetRankAndTrafficStats` | `site_daily` |
| `GetQueryStats` | `query_daily` |
| `GetPageStats` | `page_daily` (run-date snapshot) |
| `GetCrawlStats` | `crawl_daily` |

Auth: `apikey` query parameter (default), static `access_token`, or OAuth refresh (`https://www.bing.com/webmasters/oauth/token`).

Bing returns the full available history per call (~3 months); the plugin filters client-side by sync window and checkpoints per namespace.

## Streams

| Profile | Namespaces |
| --- | --- |
| `minimal` | `site_daily` |
| `standard` | `site_daily`, `query_daily`, `crawl_daily` |
| `full` | All five curated namespaces (includes `site_run_daily`) |

Discover mode: last 3 days × `minimal` only; does not advance checkpoints.

## Config (engine)

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

## Fixtures (CI)

Set `SKIPPR_BING_WEBMASTER_TOOLS_FIXTURE_DIR` to a directory containing:

- `getrankandtrafficstats_{site_slug}.json`
- `getquerystats_{site_slug}.json`
- `getpagestats_{site_slug}.json`
- `getcrawlstats_{site_slug}.json`

See `plugins/data_source/bing_webmaster_tools/tests/fixtures/`.

Happy / unhappy path matrix: `plugins/data_source/bing_webmaster_tools/docs/happy_unhappy_paths.md`.

## CLI

```bash
skippr connect source bing-webmaster-tools \
  --site-url "https://example.com/" \
  --start-date 2026-03-01 \
  --stream-profile standard \
  --api-key "${BING_WEBMASTER_TOOLS_API_KEY}"
```

## Checklist

- [x] `source_namespace_contracts()` + `replace_partition` on `date`
- [x] Per-namespace checkpoints (`last_completed_date`)
- [x] `skippr_plugin_name`: `bing_webmaster_tools` → `BingWebmasterTools`
- [x] `translate_bing_webmaster_tools_source` test in `skippr-cli`

See [API / SaaS source plugins](api-saas-source-plugins.md) and [Google Search Console plugin](google-search-console-plugin.md) for shared patterns.
