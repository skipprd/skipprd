# DataForSEO keyword research — happy / unhappy paths

Keyword expansion, MSV, KD, KGR, and composite opportunity scores. **SERP rank tracking** is owned by `GoogleSerpRanks` (Bright Data), not this plugin.

## Happy paths

### Configuration and credentials

- Valid `site` (domain or URL) normalized via `normalize_site`
- At least one `seed_keyword` in `mvp` / `full` mode (or `discover_only` with empty seeds allowed)
- Credentials from `login`/`password`, `DATAFORSEO_API_USER`/`DATAFORSEO_API_PASS`, or `DATAFORSEO_LOGIN`/`DATAFORSEO_PASSWORD`
- Fixture dir `SKIPPR_DATAFORSEO_SEO_OPPORTUNITIES_FIXTURE_DIR` bypasses live auth for CI
- `run_mode: mvp` enables keyword research streams (suggestions, metrics, allintitle, opportunity scores)
- `run_mode: full` enables all streams when listed in `streams` (including opt-in SERP tracking streams)
- Optional `competitors` with unique names and valid domains (≤ `limits.max_competitors`)
- Explicit `streams` override when non-empty

### MVP sync (fixtures / live)

1. Seed row emitted to `seed_keyword_daily`
2. Labs keyword suggestions → `keyword_suggestion_daily` + `keyword_metric_daily`
3. Allintitle query → `allintitle_daily` with KGR when volume known
4. Opportunity scoring from volume/KD/KGR (no live SERP fetch in default MVP)
5. Run rollup → `site_run_daily` (`seed_count`, `keyword_count`, `serp_count`, `api_cost_estimate`, `error_count`, `rows_by_stream`)

### Opt-in SERP tracking streams (not default)

When `streams` includes `serp_results`, `serp_features`, `weak_spots`, `rank_tracking`, or `keyword_clusters`, the plugin may call DataForSEO SERP APIs. Up Foundry console uses **Bright Data** (`google_serp_ranks`) as the SERP rank source of truth instead.

### Discover mode

- `SKIPPR_RUNTIME_EXECUTION_MODE=discover` bounds work:
  - One seed (`DISCOVER_MAX_SEEDS`)
  - Up to five suggestions (`DISCOVER_SUGGESTION_LIMIT`)
  - One keyword for allintitle/KGR when enabled
- Competitor ranked keywords limited; sitemap fetch skipped

## Unhappy paths

### Configuration validation

- Empty `seed_keywords` in `mvp` / `full` → validation error
- Invalid `site` or competitor domain → validation error
- Duplicate competitor `name` → validation error
- More competitors than `limits.max_competitors` → validation error
- Missing credentials without fixture dir → plugin init error

### API / task failures (non-fatal per keyword)

- DataForSEO task `status_code != 20000` → logged; sync continues
- Empty `items[]` with successful task → zero rows for that stream
