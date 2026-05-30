# Meta Instagram Ads source plugin

Runtime source plugin: `plugins/data_source/meta_instagram_ads/` (`MetaInstagramAds` / `meta_instagram_ads`).

## API

- [Ad Account Insights](https://developers.facebook.com/docs/marketing-api/reference/ad-account/insights/)
- Default Graph version: `v21.0` (config `api_version`)
- Instagram scope: `filtering` on `publisher_platform=instagram` when `instagram_filter: true`
- Auth: `access_token` query parameter on insights requests (from static token or OAuth refresh)

## Bronze catalog

| Namespace | Level | Notes |
| --- | --- | --- |
| `meta_instagram_ads.account_daily` | account | Discover / minimal profile |
| `meta_instagram_ads.campaign_daily` | campaign | |
| `meta_instagram_ads.adset_daily` | adset | |
| `meta_instagram_ads.ad_daily` | ad | |
| `meta_instagram_ads.campaign_placement_daily` | campaign + `breakdowns=platform_position` | |

All namespaces: `replace_partition` on `date`, `semantics: mutable_report`, `lookback_days` refresh window.

## Discover sampling (automatic)

When `SKIPPR_RUNTIME_EXECUTION_MODE=discover`:

- Last **3** calendar days only
- **`account_daily`** stream only (`StreamProfile::Minimal`)
- No checkpoint load/advance; `lookback_days` treated as **0** for the date planner

Contracts emitted from `source_namespace_contracts()` still reflect the configured profile (e.g. all five on `full`).

## Configuration (engine `skippr.yml`)

```yaml
data_sources:
  meta_ig:
    MetaInstagramAds:
      ad_account_id: "${META_INSTAGRAM_ADS_AD_ACCOUNT_ID}"
      access_token: "${META_INSTAGRAM_ADS_ACCESS_TOKEN}"
      start_date: "2024-01-01"
      stream_profile: full
      lookback_days: 3
      processing_lag_days: 1
      instagram_filter: true
```

OAuth alternative: `oauth_token_url`, `oauth_client_id`, `oauth_client_secret`, `oauth_refresh_token`.

## Local verification

```bash
cargo build -p skippr-plugin-data-source-meta-instagram-ads
cargo test -p skippr-plugin-data-source-meta-instagram-ads
cargo test -p skippr-cli translate_meta_instagram_ads
```

Fixture-only sync (no live Graph API):

```bash
export SKIPPR_META_INSTAGRAM_ADS_FIXTURE_DIR=plugins/data_source/meta_instagram_ads/tests/fixtures
export USE_LOCAL_PLUGIN_CODE=1
manifest_dir="$(python3 .github/scripts/local_runtime_plugins.py \
  --config path/to/skippr.yml --pipeline my_pipeline)"
export SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR="$manifest_dir"
skippr discover --pipeline my_pipeline
skippr sync --once --pipeline my_pipeline
```

Live E2E requires a Marketing API token with `ads_read` and access to the configured ad account.

## CLI

```bash
skippr connect source meta-instagram-ads \
  --ad-account-id 123456789 \
  --start-date 2024-01-01 \
  --access-token '${META_INSTAGRAM_ADS_ACCESS_TOKEN}'
```

Public connector: [skippr-web Meta Instagram Ads](https://github.com/skippr-io/skippr-web/blob/main/docs/connectors/sources/meta-instagram-ads.md).
