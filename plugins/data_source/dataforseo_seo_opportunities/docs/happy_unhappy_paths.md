# DataForSEO SEO Opportunities — happy / unhappy paths

## Happy paths

### Configuration and credentials

- Valid `site` (domain or URL) normalized via `normalize_site`
- At least one `seed_keyword` in `mvp` / `full` mode (or `discover_only` with empty seeds allowed)
- Credentials from `login`/`password`, `DATAFORSEO_API_USER`/`DATAFORSEO_API_PASS`, or `DATAFORSEO_LOGIN`/`DATAFORSEO_PASSWORD`
- Fixture dir `SKIPPR_DATAFORSEO_SEO_OPPORTUNITIES_FIXTURE_DIR` bypasses live auth for CI
- `run_mode: mvp` enables six output streams; `full` enables all thirteen scored/raw streams plus `site_run_daily` and `seed_keyword_daily` (15 namespace contracts)
- Optional `competitors` with unique names and valid domains (≤ `limits.max_competitors`)
- `streams` override list when non-empty

### MVP sync (fixtures / live)

1. Seed row emitted to `seed_keyword_daily`
2. Labs keyword suggestions → `keyword_suggestion_daily` + `keyword_metric_daily`
3. SERP organic advanced (with AI overview flag) → `serp_result_daily`, `serp_feature_daily`
4. Weak-spot heuristics → `weak_spot_daily`
5. Deterministic opportunity scoring → `opportunity_score_daily` with explainable factor lists
6. Run rollup → `site_run_daily` (`seed_count`, `keyword_count`, `serp_count`, `api_cost_estimate`, `error_count`, `rows_by_stream`)

### Full sync

- All MVP streams plus:
  - SERP overlap clustering → `keyword_cluster_daily`
  - Competitor ranked keywords (DataForSEO Labs) → `competitor_keyword_daily`
  - Competitor sitemap HTTP fetch → `competitor_sitemap_url_daily` (skipped in discover)
  - Allintitle SERP query → `allintitle_daily` with KGR when volume known
  - Rank tracking rows for configured keywords / stream → `rank_tracking_daily`
  - AI citation heuristics from SERP features → `ai_citation_opportunity_daily`
  - Content briefs for keywords with `opportunity_score >= 40` → `content_brief_daily` (deterministic template; OpenAI optional when key present)

### Discover mode

- `SKIPPR_RUNTIME_EXECUTION_MODE=discover` bounds work:
  - One seed (`DISCOVER_MAX_SEEDS`)
  - Up to five suggestions (`DISCOVER_SUGGESTION_LIMIT`)
  - SERP depth 10 (`DISCOVER_SERP_DEPTH`)
  - One keyword SERP pass
  - Competitor ranked keywords limited; sitemap fetch skipped
- All enabled namespace contracts validate; no checkpoint I/O required

### Parsing and scoring (unit)

- Keyword suggestion / metric parsing from fixture JSON
- SERP organic + PAA + featured snippet + AI overview feature detection
- Weak-spot counts (forums, UGC, low authority)
- Opportunity score positive/negative factor arrays
- Allintitle count + KGR
- Competitor ranked keyword overlap flags
- AI citation opportunity detection from SERP features

## Unhappy paths

### Configuration validation

- Empty `seed_keywords` in `mvp` / `full` → validation error
- Invalid `site` or competitor domain → validation error
- Duplicate competitor `name` → validation error
- More competitors than `limits.max_competitors` → validation error
- Empty competitor name → validation error
- `limits.max_seed_keywords == 0` or `serp_depth == 0` → validation error
- Missing credentials without fixture dir → plugin init error with login/password message

### API / task failures (non-fatal per keyword)

- DataForSEO task `status_code != 20000` → logged; `tasks_error` incremented; empty items for that call; sync continues
- Empty `items[]` with successful task → zero rows for that stream; no panic
- SERP / allintitle / ranked-keywords task errors → same pattern; other keywords/seeds still processed

### Live-only failures

- HTTP 401 → `PermissionDenied` with auth message (not exercised in fixture CI)
- HTTP non-success after retries → error propagated to host
- Competitor sitemap URLs unreachable → empty sitemap stream for that competitor; no fatal error

### Deferred / out of scope (v1)

- `seed_queries_from_gsc` / `seed_urls_from_crawl` flags present but no cross-source seed pull at runtime (silver/dbt joins)
- OpenAI content brief generation requires `OPENAI_API_KEY`; fixture dir treats OpenAI as available for config checks only
- Related keywords / autocomplete endpoints implemented on client but not wired into sync yet

## Test coverage map

| Path | Test |
|------|------|
| MVP namespace contracts | `namespace_contracts_for_mvp` |
| Full namespace contracts (15) | `namespace_contracts_for_full` |
| MVP fixture sync | `mvp_fixture_sync_emits_core_namespaces` |
| Full + competitors | `full_fixture_sync_includes_competitor_keywords` |
| Discover bounds | `discover_sync_bounded_seeds_and_skips_sitemaps` |
| Task error continues | `sync_continues_after_keyword_suggestion_task_error` |
| Empty suggestions | `sync_handles_empty_keyword_suggestions` |
| Missing credentials | `new_fails_without_credentials_or_fixture` |
| Config validation | `config::tests::*` |
| Client parse / probe | `client::tests::*` |
| Parsers / scoring | `parse_*`, `scoring`, `cluster`, `ai_citation`, `content_brief`, `streams`, `target` |

Run:

```bash
./scripts/cargo-with-local-react.sh test -p skippr-plugin-data-source-dataforseo-seo-opportunities --lib
```
