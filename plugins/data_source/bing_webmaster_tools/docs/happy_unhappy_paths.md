# Bing Webmaster Tools — happy / unhappy paths

## Happy paths

- Valid `site_url`, `start_date`, and `api_key` (or `access_token` / OAuth refresh)
- Site URL normalized (`https://…/` trailing slash; `sc-domain:` preserved)
- Stream profiles: `minimal` (discover), `standard` (site/query/crawl dailies), `full` (+ page snapshot + run aggregate); optional `streams` override
- Read APIs: `GetRankAndTrafficStats`, `GetQueryStats`, `GetPageStats`, `GetCrawlStats` via fixture dir for CI
- Client-side date filter on full API payloads (~3 months returned per call)
- .NET `/Date(ms)/` dates parsed to `YYYY-MM-DD` bronze `date`
- Namespace contracts: `ReplacePartition`, `date` partition, dimension PKs (`Query`, `Url`)
- Checkpoints: `last_completed_date` per namespace (sync only; zero-row days still advance)
- Discover: 3-day window, minimal streams only, `lookback_days=0`, no checkpoint I/O
- `page_daily`: run-date snapshot (rows tagged with sync `end_date` when API has no `Date`)
- `site_run_daily` aggregate emitted on full-profile sync (non-discover)
- `api_key` preferred over `access_token` when both are set

## Unhappy paths

- Missing credentials (no key, token, OAuth, fixture) → clear `InvalidInput` error
- Invalid `start_date` format → validation error at `new()` / `sync()`
- `start_date` older than ~3 months → validation error
- `window_in_days` outside 1..364 → validation error
- `end_date` before `start_date` on sync → error
- HTTP 403 / forbidden site → `PermissionDenied` with site context
- Missing fixture file → `NotFound` with path in message
- Empty `{"d":[]}` response → no bronze rows; checkpoint still advances per day in window
- API rows without parseable `Date` (dated streams) → skipped unless `page_daily` partition date applies
- Unparseable `/Date(...)/` values → row skipped for dated streams
- Discover must not store checkpoints or emit `page_daily` / `site_run_daily`
- Malformed checkpoint payload → ignored on load (warn, treat as no checkpoint)
