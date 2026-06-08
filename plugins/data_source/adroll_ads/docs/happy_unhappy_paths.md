# AdRoll Ads — happy / unhappy paths

## Happy paths

- Valid config: `advertiser_id`, `start_date`, and one of static token, `ADROLL_ADS_ACCESS_TOKEN`, fixture mode, or a full OAuth refresh quartet
- Optional API overrides: `api_base_url`, `reporting_base_url`, `ADROLL_ADS_API_BASE`, or `ADROLL_ADS_REPORTING_URL`
- Profile selection: `minimal` (reporting only), `standard` (advertisers, campaigns, reporting), `full` (all five streams); optional `streams` override
- Entity streams: `advertisers`, `campaigns`, `ad_groups`, and `ads` use REST snapshot endpoints and `Append` contracts
- Reporting stream: `reporting_daily` uses GraphQL daily reports with `ReplacePartition` on `date`
- Fixture sync (`SKIPPR_ADROLL_ADS_FIXTURE_DIR`): per-namespace JSON parses rows with `advertiser_id`, entity IDs, and report dates
- Normal sync: day loop, submit batches, advance `last_completed_date` checkpoint for `reporting_daily`
- Discover: 3-day window, `minimal` streams only, and no checkpoint load/store
- Zero-row API day still advances checkpoint for reporting sync
- OAuth refresh path accepted when all four refresh fields are non-empty

## Unhappy paths

- Missing auth: no token, env, OAuth, or fixture dir → clear `InvalidInput`
- OAuth configured with any missing or empty field → error
- Empty / whitespace `advertiser_id` → validation error
- Invalid `start_date` / `end_date` format → parse error
- `end_date` before effective start (incl. discover window) → sync error
- Malformed reporting body / non-array reports → no rows (no panic)
- Empty report array → no ingest rows; checkpoint still advances on sync
- Corrupt checkpoint payload → ignored (warn), full window from planner
- HTTP 429 / transient statuses → retry/backoff; 400/403 → give up (no retry)
- Unknown fixture namespace → clear missing-fixture error
- Discover must not call `store_checkpoint` (regression)
