# DataForSEO Backlinks — happy / unhappy paths

## Happy paths

- Valid config: `backlink_jobs` and/or `intersection_jobs`
- Credentials: `login`/`password`, `DATAFORSEO_LOGIN`/`DATAFORSEO_PASSWORD`, or fixture dir
- `normalize_target` strips scheme/`www.` for API `target`
- POST live endpoints with Basic auth; fixture JSON in CI (`SKIPPR_DATAFORSEO_BACKLINKS_FIXTURE_DIR`)
- Parse `tasks[].result[].items[]` → `backlink_daily` / `page_intersection_daily`
- `site_run_daily` rows with endpoint + cost
- Discover: first job per type only (bounded sample)
- Three namespace contracts (`ReplacePartition`, `run_date`)

## Unhappy paths

- No jobs configured → validation error
- Missing credentials without fixture → init error
- HTTP non-success from API → error with status body (live only)
- Malformed / empty `items` → zero rows (no panic)
- Empty backlink item (no URLs) → skipped
- Pagination `search_after_token` reserved for v2 deep pages (parser helper tested)
- Discover does not require checkpoint I/O in v1 scaffold
