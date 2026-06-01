# Google SERP ranks

Track organic Google positions for **your** domains on a small set of keywords. Designed for low-volume daily monitoring, not bulk SERP scraping.

## Requirements

- Node.js and Chromium (Playwright) on the sync host
- Install worker deps: `cd plugins/data_source/google_serp_ranks/worker && npm install && npx playwright install chromium`

## Connect

```bash
skippr connect source google-serp-ranks \
  --target-site example.com \
  --keywords "brand name,product category" \
  --country uk \
  --language en
```

## Options (engine `skippr.yml`)

| Field | Description |
| --- | --- |
| `targets` | Sites to find in results (`site`, optional `aliases`) |
| `keywords` | Search queries to run |
| `country` / `language` | Google `gl` / `hl` (default `uk` / `en`) |
| `device` | `desktop` or `mobile` |
| `max_depth` | Max organic positions to inspect (default 30, max 100) |
| `min_query_interval_ms` | Pause between queries (default 30000) |
| `max_queries_per_run` | Cap keywords per sync (default 10) |
| `capture_results` | Store full organic rows (default false) |

## Operational notes

- Google may show CAPTCHA or consent pages; the plugin records `status: blocked` and does not retry aggressively in the same run.
- Do not lower `min_query_interval_ms` below 5000 without a strong reason.
- Same keyword/locale/device is skipped on subsequent syncs the same UTC day unless `force_refresh_today: true`.

See [maintainer doc](../../maintainers/google-serp-ranks-plugin.md) for namespaces and worker details.
