# Google Search Console — happy / unhappy paths

## Happy paths

- Valid `site_url`, `start_date`, static token or OAuth / service account
- Site URL normalized (`https://…/` or `sc-domain:`)
- Stream profiles: minimal (discover), standard, full; optional `streams` override
- Search Analytics: day-chunked query, pagination via `startRow`, fixture dir for CI
- Namespace contracts: `ReplacePartition`, `date` partition, dimension-specific PKs
- Checkpoints: `last_completed_date` per namespace (sync only)
- Discover: 3-day window, minimal streams, `lookback_days=0`, no checkpoint I/O
- Optional streams: skip on 400 optional-dimension errors
- Sitemap snapshot + URL inspection (when enabled, non-discover)
- `site_run_daily` aggregate on full sync

## Unhappy paths

- Missing credentials (no token, env, OAuth, SA, fixture) → clear error
- Invalid dates, end before start → sync error
- HTTP 403 forbidden site → detected via `is_forbidden_site_error`
- Optional dimension 400 → stream skipped (not fatal)
- Malformed query response → empty/partial rows handled in parser tests
- Pagination: full page advances `startRow`; discover must not checkpoint
- URL inspection without URLs → no-op for that stream
