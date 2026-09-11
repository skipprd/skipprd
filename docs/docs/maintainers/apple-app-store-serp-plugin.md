# Apple App Store SERP plugin (maintainer)

Crate: `skippr-plugin-data-source-apple-app-store-serp` (`plugins/data_source/apple_app_store_serp/`).

## Bronze namespaces

| Namespace | Grain |
| --- | --- |
| `apple_app_store_serp.run_daily` | keyword × storefront × entity × run_date |
| `apple_app_store_serp.target_rank_daily` | keyword × storefront × entity × target_app_id × run_date |
| `apple_app_store_serp.result_daily` | keyword × storefront × entity × position × run_date (optional) |

All use `replace_partition` on `run_date`. Checkpoint key: `apple_app_store_serp:query:{keyword}:{storefront}:{entity}`.

## Sync loop

- Work queue: cartesian product `keywords × storefronts` (keywords outer), truncated by `max_queries_per_run`.
- Discover mode: one keyword, one storefront, capped depth, no checkpoints.
- Fetch backend: `itunes` (`GET https://itunes.apple.com/search`).
- Target match: `trackId` vs `app_id` / `aliases`, else `bundleId` vs `bundle_id`.

## Local / CI testing

Set `SKIPPR_APPLE_APP_STORE_SERP_FIXTURE_DIR` to the plugin `fixtures/` directory (no live iTunes calls).

```bash
cargo test -p skippr-plugin-data-source-apple-app-store-serp
```

Fixture matrix:

| Fixture | Expected |
| --- | --- |
| `search_ok.json` | `ok`, target found at position 3 |
| `not_found_query.json` | `ok`, `found: false` target row |
| `http_error.json` | `error`, `Error` checkpoint |

## CLI

```bash
sde connect source apple-app-store-serp --app-id 123 --keywords "term" --storefronts us
```

Pipeline name: `apple_app_store_serp`.
