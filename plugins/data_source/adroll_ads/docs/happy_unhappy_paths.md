# Meta Instagram Ads — happy / unhappy paths

## Happy paths

- Valid config: `ad_account_id`, `start_date`, static `access_token` or full OAuth refresh quartet
- `ad_account_id` normalized (`act_` prefix stripped for API path)
- Profile selection: `minimal` (1), `standard` (3), `full` (5) streams; optional `streams` override
- Namespace contracts: `ReplacePartition`, `date` partition/cursor, level-appropriate primary keys, `MutableReport`
- Fixture sync (`SKIPPR_ADROLL_ADS_FIXTURE_DIR`): per-namespace JSON → parse rows with `ad_account_id`, `date`, grain IDs
- Insights request: `time_increment=1`, day `time_range`, `level`, curated `fields`, Instagram `filtering` when enabled
- Placement stream: `breakdowns=platform_position` + extra fields in PK
- Pagination: follow `paging.next` until exhausted (live API)
- Normal sync: day loop, submit batches, advance `last_completed_date` checkpoint per namespace
- Discover: 3-day window, `minimal` streams only, `lookback_days=0`, contracts still reflect configured profile
- Discover: no checkpoint load/store
- Zero-row API day still advances checkpoint (sync path)
- OAuth refresh path accepted when all four fields non-empty

## Unhappy paths

- Missing auth: no token, env, OAuth, or fixture dir → clear `InvalidInput`
- OAuth configured with any empty field → error
- Empty / whitespace `ad_account_id` → validation error
- Invalid `start_date` / `end_date` format → parse error
- `end_date` before effective start (incl. discover window) → sync error
- Malformed insights body / non-array `data` → no rows (no panic)
- Empty `data` array → no ingest rows; checkpoint still advances on sync
- `publisher_platform` not `instagram` with filter on → row dropped
- Corrupt checkpoint payload → ignored (warn), full window from planner
- HTTP 429 → retry/backoff; 400/403 → give up (no retry)
- Unknown fixture namespace → falls through to live HTTP (CI uses fixtures only)
- Discover must not call `store_checkpoint` (regression)
