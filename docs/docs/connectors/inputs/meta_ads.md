---
title: Meta Ads
description: Sync daily Meta (Facebook and Instagram) ad insights by account, campaign, ad set, ad, and placement into your warehouse.
---

# Meta Ads

The Meta Ads source reads daily insights from the Meta Marketing API for one ad account at account, campaign, ad set, ad, and campaign-by-placement grain: impressions, reach, clicks, spend, and conversion actions. It covers every Meta placement (Facebook, Instagram, Messenger, and Audience Network). If you only want Instagram delivery, use [Meta Instagram Ads](/connectors/inputs/meta_instagram_ads) instead.

## Before you begin

You need a Meta app, a token with `ads_read` for the ad account, the ad account ID, and a destination that can rewrite daily partitions.

1. **Create an app.** In [Meta for Developers](https://developers.facebook.com/apps/), create a **Business** app and add the **Marketing API** product.
2. **Create a system user token.** In [Business Settings](https://business.facebook.com/settings/), open **Users → System users**, add a system user, and under **Assign assets** give it access to the ad account. Then choose **Generate new token**, pick your app, select the `ads_read` permission, and set the token to never expire. A personal user token from the Graph API Explorer also works, but it expires after about 60 days.
3. **Find the ad account ID.** In [Ads Manager](https://adsmanager.facebook.com/), the account dropdown shows the ID; the URL contains it as `act=…`. Both `1234567890` and `act_1234567890` work.
4. **Set up a destination.** Every stream is rewritten one day at a time, so the destination must support partition replacement: [Athena](/connectors/outputs/athena), [Athena Iceberg](/connectors/outputs/athenaiceberg), or [SkipprLake](/connectors/outputs/skipprlake). The examples below assume a data sink named `lake` already exists in `skippr.yml`.

```bash
export META_ADS_ACCESS_TOKEN="your-system-user-token"
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSourceMetaAds, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "meta_ads",
    DataSourceMetaAds(
        ad_account_id="act_1234567890",
        start_date="2026-01-01",
        access_token=EnvRef("META_ADS_ACCESS_TOKEN"),
    ),
)
cfg.pipeline("meta_ads", Pipeline(data_source=src, data_sink=cfg.get_data_sink("lake")))
cfg.save()
```

```bash [CLI]
skipprd connect data-source meta-ads \
  --pipeline meta_ads \
  --name meta_ads \
  --ad-account-id act_1234567890 \
  --start-date 2026-01-01 \
  --access-token '${META_ADS_ACCESS_TOKEN}'
```

```yaml [YAML]
pipelines:
  meta_ads:
    data_source: data_sources.meta_ads
    data_sink: data_sinks.lake

data_sources:
  meta_ads:
    MetaAds:
      ad_account_id: "act_1234567890"
      start_date: "2026-01-01"
      access_token: ${META_ADS_ACCESS_TOKEN}
```

:::

With the CLI, wire the `lake` destination into the same pipeline with `skipprd connect data-sink … --pipeline meta_ads` if it is not there already.

### Check it worked

```bash
skipprd discover --pipeline meta_ads
skipprd sync --pipeline meta_ads --once
```

Then query `meta_ads_account_daily` in your destination. You should see one row per day from `start_date` through yesterday.

## Options

| Key | Type | Default | Description |
|---|---|---|---|
| `ad_account_id` | string | Required | Ad account ID, with or without the `act_` prefix. |
| `start_date` | string | Required | First report day, `YYYY-MM-DD`. |
| `access_token` | secret | — | Token with `ads_read`. Used before OAuth refresh. |
| `oauth_token_url` | string | — | Token endpoint for your own OAuth refresh flow. |
| `oauth_client_id` | string | — | App ID. |
| `oauth_client_secret` | secret | — | App secret. When set, Skipprd also signs every request with Meta's app secret proof. |
| `oauth_refresh_token` | secret | — | Refresh token. Used only when `access_token` is not set. |
| `api_version` | string | `v21.0` | Graph API version, such as `v23.0`. |
| `end_date` | string | — | Last report day, `YYYY-MM-DD`. Without it, Skipprd syncs through yesterday. |
| `lookback_days` | integer | `3` | Days before the last completed day to fetch again on every sync, so late conversions land. |
| `processing_lag_days` | integer | `1` | Never sync days more recent than this many days ago. |
| `instagram_filter` | boolean | `false` | Only report Instagram delivery. |
| `stream_profile` | string | `full` | Which streams to sync: `minimal`, `standard`, or `full`. |
| `streams` | list of strings | — | Exact stream names to sync. Overrides `stream_profile`. |

If you set any `oauth_*` field you must set all four. Secret fields only accept `${NAME}` environment references (`EnvRef("NAME")` in Python).

## What gets synced

Each stream lands as its own table named after the stream, with the dot replaced by an underscore.

| Stream | Table | One row per | Profiles |
|---|---|---|---|
| `meta_ads.account_daily` | `meta_ads_account_daily` | day | minimal, standard, full |
| `meta_ads.campaign_daily` | `meta_ads_campaign_daily` | campaign per day | standard, full |
| `meta_ads.adset_daily` | `meta_ads_adset_daily` | ad set per day | standard, full |
| `meta_ads.ad_daily` | `meta_ads_ad_daily` | ad per day | full |
| `meta_ads.campaign_placement_daily` | `meta_ads_campaign_placement_daily` | campaign per placement per day | full |

Every row has `ad_account_id`, `date`, and the IDs for its grain (`campaign_id`, `adset_id`, `ad_id`). Metrics are Meta's `impressions`, `reach`, `frequency`, `clicks`, `inline_link_clicks`, `spend`, `cpm`, `cpc`, and `ctr`, plus `account_name`, `campaign_name`, `adset_name`, and `ad_name`. Conversions arrive in `actions`, Meta's list of `action_type` and `value` pairs. The placement stream adds `publisher_platform` and `platform_position`. Meta returns most numbers as strings, so cast them in your models.

**Incremental window.** The first sync requests each day from `start_date` to yesterday, one request per day per stream. Each later sync starts again at the last completed day minus `lookback_days` and rewrites those days in full, so updated numbers replace the old ones.

**Attribution.** Conversions use the attribution setting of each ad set in Ads Manager (by default, 7-day click and 1-day view), and Meta keeps crediting conversions to the day of the ad interaction. Set `lookback_days` to at least 7 if you report conversions and need them to settle.

**Discover.** `skipprd discover` reads only `account_daily` for the last 3 days and does not save progress.

**Rate limits.** Meta throttles Marketing API calls per ad account, and the limit grows with your app's access tier. Skipprd retries rate-limit (429) and server (5xx) responses with exponential backoff, up to 8 attempts per request. It honours `Retry-After`, waits 30 seconds on a 429 without one, and never waits more than 2 minutes between attempts. Long backfills of the `full` profile make five requests per day, so start with `standard` if you hit limits.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `Meta Ads requires access_token, META_ADS_ACCESS_TOKEN, or oauth_token_url + …` | No token is set, or its environment variable is empty. | Export the token in the shell or service that runs Skipprd. |
| `Meta Ads OAuth refresh requires oauth_token_url, oauth_client_id, oauth_client_secret, and oauth_refresh_token` | Only some OAuth fields are set. | Set all four, or remove them and use `access_token`. |
| `ad_account_id is required` | `ad_account_id` is empty. | Set the ad account ID. |
| Error code 190 (invalid or expired token) | The token expired, was revoked, or its user lost access. | Generate a new system user token. |
| Error code 200 or 10 (permission) | The token lacks `ads_read`, or the system user was not assigned the ad account. | Regenerate the token with `ads_read` and assign the ad account under **Assign assets**. |
| Error code 100 mentioning `appsecret_proof` | Your app requires app secret proof on every call. | Turn off **Require App Secret** in the app's advanced settings, or configure OAuth with `oauth_client_secret`. |
| Error about an unsupported or deprecated version | Meta retires each Graph API version about two years after release. | Set `api_version` to a current version. |
| Error code 17 or 80004 (too many calls) | You hit Meta's rate limit and retries ran out. | Sync fewer streams, use a shorter backfill, or raise your app's Marketing API access tier. |
| `sink '…' does not support write policy ReplacePartition` | The destination cannot rewrite daily partitions. | Use Athena, Athena Iceberg, or SkipprLake as the destination. |

## Next steps

- [Meta Instagram Ads](/connectors/inputs/meta_instagram_ads) lands Instagram-only delivery in its own tables.
- [How sources land](/concepts/source-landing-semantics) explains partition replacement and lookback.
- [skipprd sync](/cli/sync) covers scheduling and continuous mode.
