# Apple Search Ads plugin (maintainer)

Crate: `skippr-plugin-data-source-apple-search-ads` (`plugins/data_source/apple_search_ads/`).

## Bronze namespaces

| Namespace | Grain |
| --- | --- |
| `apple_search_ads.campaign_daily` | org × campaign × date |
| `apple_search_ads.ad_group_daily` | org × campaign × ad group × date |
| `apple_search_ads.keyword_daily` | org × campaign × ad group × keyword × date |
| `apple_search_ads.search_term_daily` | org × campaign × ad group × search term × date |

All streams use `replace_partition` on `date`. Checkpoint key: `asa:{org_id}:{namespace}`.

## Auth

Live runs require one of:

- `access_token` / `APPLE_SEARCH_ADS_ACCESS_TOKEN`
- Apple client credentials: `client_id`, `team_id`, `key_id`, plus `private_key_pem`, `private_key_path`, or `APPLE_SEARCH_ADS_PRIVATE_KEY_PATH`

The client-credentials path signs a JWT and exchanges it with Apple for `scope=searchadsorg`. Fixture mode (`SKIPPR_APPLE_SEARCH_ADS_FIXTURE_DIR`) skips live auth.

## Sync loop

- Discover mode runs the minimal profile for the last three days and does not persist checkpoints.
- Campaign and ad group references are enumerated before fan-out streams run.
- `search_term_daily` requests Apple’s `ORTZ` timezone, matching Search Ads API requirements.
- Reports paginate through Apple’s offset/limit selector and merge all returned rows before normalization.

## Local / CI testing

```bash
cargo test -p skippr-plugin-data-source-apple-search-ads --lib
```

Fixture mode:

```bash
SKIPPR_APPLE_SEARCH_ADS_FIXTURE_DIR=plugins/data_source/apple_search_ads/tests/fixtures \
  cargo test -p skippr-plugin-data-source-apple-search-ads --lib
```

## Release checklist

1. Run the focused plugin tests.
2. Bump the plugin crate version when runtime behavior or contracts change.
3. Tag a skipprd release and confirm `install.skippr.io` publishes `apple-search-ads-source` at the expected version.
4. In console-web, refresh bundled metadata after a successful prod sync.
