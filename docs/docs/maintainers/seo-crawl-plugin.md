# SEO Crawl source plugin

`SeoCrawl` (`seo_crawl`) is a Skipprd runtime **data source** plugin that crawls a configured site origin, discovers `robots.txt` and sitemaps, extracts on-page SEO / head metadata, builds an internal link graph, extracts content blocks for AEO analysis, and optionally scores blocks via OpenAI.

Crate: `plugins/data_source/seo_crawl/` (`skippr-plugin-data-source-seo-crawl`).

## Namespaces

| Namespace | Grain |
| --- | --- |
| `seo_crawl.site_run_daily` | One row per crawl run / `crawl_date` |
| `seo_crawl.page_daily` | One row per canonical URL / day |
| `seo_crawl.link_edge` | One row per discovered link / day |
| `seo_crawl.robots_txt` | Robots.txt snapshot / day |
| `seo_crawl.sitemap_url` | Sitemap URL entries / day |
| `seo_crawl.check_daily` | Per-check outcomes (pass / warn / fail) / day |
| `seo_crawl.content_block` | Content block + optional OpenAI scores / day |

All use `replace_partition` on `crawl_date`.

## Configuration (engine `skippr.yml`)

```yaml
data_sources:
  example_site:
    SeoCrawl:
      site: "https://example.com"
      max_urls: 5000
      max_depth: 8
      crawl_rate_per_second: 2
      respect_robots: true
      openai_enabled: true
      openai_model: "gpt-4.1-mini"
      openai_analyze_blocks: true
      openai_max_blocks_per_page: 24
      user_agent: "SkipprSeoCrawl/1.0"
```

Public config (`sde connect source seo-crawl`): `kind: seo_crawl` with the same fields (camelCase in JSON export).

## Environment

| Variable | Purpose |
| --- | --- |
| `OPENAI_API_KEY` | Block-level AEO scoring (when `openai_enabled`) |
| `OPENAI_BASE_URL` | Optional API base (default OpenAI) |
| `SKIPPR_SEO_CRAWL_FIXTURE_DIR` | Offline HTML/robots/sitemap fixtures for tests |
| `SKIPPR_OPENAI_FIXTURE_DIR` | Offline OpenAI JSON fixtures |
| `SKIPPR_RUNTIME_EXECUTION_MODE=discover` | Minimal crawl (~10 URLs), no OpenAI, no checkpoint writes |

## Checkpoints

Per-page checkpoints in the host offset store (`seo_crawl:page:{canonical_url}`) store `content_hash`, page scores, and per-block `text_hash` values. When `content_hash` is unchanged, OpenAI is skipped and scores are forwarded from the checkpoint.

## Boundaries

- **Static HTML only** — `render_js` is rejected; use the `site_quality` plugin for Playwright / JS rendering.
- No third-party rank, CrUX, or SERP APIs.
- Plugin emits bronze JSON only; sinks and SQL models live in Skipprd core / dbt.

## Tests

```bash
cargo test -p skippr-plugin-data-source-seo-crawl
cargo test -p skippr-plugin-shared-api-source
```

Required unit tests: `content_hash_unchanged_skips_openai_calls`, `block_hash_change_analyzes_only_delta_blocks`, `checkpoint_roundtrip_in_offset_store`.
