---
title: Meta Instagram Ads
description: Sync daily Instagram-only Meta ad insights by account, campaign, ad set, ad, and placement into their own warehouse tables.
---

# Meta Instagram Ads

The Meta Instagram Ads source reads the same daily Meta Marketing API insights as [Meta Ads](/connectors/inputs/meta_ads), restricted to delivery on Instagram, and lands them in their own `meta_instagram_ads_*` tables. Use it when you report Instagram as a separate channel, or alongside Meta Ads when you want both the all-placement totals and the Instagram slice.

## Before you begin

Setup is the same as for Meta Ads: a Meta app, a token with `ads_read` for the ad account, the ad account ID, and a destination that can rewrite daily partitions.

1. **Create an app.** In [Meta for Developers](https://developers.facebook.com/apps/), create a **Business** app and add the **Marketing API** product.
2. **Create a system user token.** In [Business Settings](https://business.facebook.com/settings/), open **Users → System users**, add a system user, and under **Assign assets** give it access to the ad account. Then choose **Generate new token**, pick your app, select the `ads_read` permission, and set the token to never expire. You can reuse the Meta Ads token.
3. **Find the ad account ID.** In [Ads Manager](https://adsmanager.facebook.com/), the account dropdown shows the ID. Both `1234567890` and `act_1234567890` work.
4. **Set up a destination.** Every stream is rewritten one day at a time, so the destination must support partition replacement: [Athena](/connectors/outputs/athena), [Athena Iceberg](/connectors/outputs/athenaiceberg), or [SkipprLake](/connectors/outputs/skipprlake). The examples below assume a data sink named `lake` already exists in `skippr.yml`.

```bash
export META_INSTAGRAM_ADS_ACCESS_TOKEN="your-system-user-token"
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSourceMetaInstagramAds, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "meta_instagram_ads",
    DataSourceMetaInstagramAds(
        ad_account_id="act_1234567890",
        start_date="2026-01-01",
        access_token=EnvRef("META_INSTAGRAM_ADS_ACCESS_TOKEN"),
    ),
)
cfg.pipeline(
    "meta_instagram_ads",
    Pipeline(data_source=src, data_sink=cfg.get_data_sink("lake")),
)
cfg.save()
```

```bash [CLI]
skipprd connect data-source meta-instagram-ads \
  --pipeline meta_instagram_ads \
  --name meta_instagram_ads \
  --ad-account-id act_1234567890 \
  --start-date 2026-01-01 \
  --access-token '${META_INSTAGRAM_ADS_ACCESS_TOKEN}'
```

```yaml [YAML]
pipelines:
  meta_instagram_ads:
    data_source: data_sources.meta_instagram_ads
    data_sink: data_sinks.lake

data_sources:
  meta_instagram_ads:
    MetaInstagramAds:
      ad_account_id: "act_1234567890"
      start_date: "2026-01-01"
      access_token: ${META_INSTAGRAM_ADS_ACCESS_TOKEN}
```

:::

With the CLI, wire the `lake` destination into the same pipeline with `skipprd connect data-sink … --pipeline meta_instagram_ads` if it is not there already.

### Check it worked

```bash
skipprd discover --pipeline meta_instagram_ads
skipprd sync --pipeline meta_instagram_ads --once
```

Then query `meta_instagram_ads_account_daily` in your destination. You should see one row per day from `start_date` through yesterday on which the account delivered on Instagram.

## Options

| Key | Type | Default | Description |
|---|---|---|---|
| `ad_account_id` | string | Required | Ad account ID, with or without the `act_` prefix. |
| `start_date` | string | Required | First report day, `YYYY-MM-DD`. |
| `access_token` | secret | — | Token with `ads_read`. Used before OAuth refresh. |
| `oauth_token_url` | string | — | Token endpoint for your own OAuth refresh flow. |
| `oauth_client_id` | string | — | App ID. |
| `oauth_client_secret` | secret | — | App secret. |
| `oauth_refresh_token` | secret | — | Refresh token. Used only when `access_token` is not set, and only when all four `oauth_*` fields are set. |
| `api_version` | string | `v21.0` | Graph API version, such as `v23.0`. |
| `end_date` | string | — | Last report day, `YYYY-MM-DD`. Without it, Skipprd syncs through yesterday. |
| `lookback_days` | integer | `3` | Days before the last completed day to fetch again on every sync, so late conversions land. |
| `processing_lag_days` | integer | `1` | Never sync days more recent than this many days ago. |
| `instagram_filter` | boolean | `true` | Only report Instagram delivery. Set to `false` to get all placements in these tables. |
| `stream_profile` | string | `full` | Which streams to sync: `minimal`, `standard`, or `full`. |
| `streams` | list of strings | — | Exact stream names to sync. Overrides `stream_profile`. |

Secret fields only accept `${NAME}` environment references (`EnvRef("NAME")` in Python).

## What gets synced

Each stream lands as its own table named after the stream, with the dot replaced by an underscore.

| Stream | Table | One row per | Profiles |
|---|---|---|---|
| `meta_instagram_ads.account_daily` | `meta_instagram_ads_account_daily` | day | minimal, standard, full |
| `meta_instagram_ads.campaign_daily` | `meta_instagram_ads_campaign_daily` | campaign per day | standard, full |
| `meta_instagram_ads.adset_daily` | `meta_instagram_ads_adset_daily` | ad set per day | standard, full |
| `meta_instagram_ads.ad_daily` | `meta_instagram_ads_ad_daily` | ad per day | full |
| `meta_instagram_ads.campaign_placement_daily` | `meta_instagram_ads_campaign_placement_daily` | campaign per Instagram placement (feed, stories, reels, …) per day | full |

The columns match [Meta Ads](/connectors/inputs/meta_ads): `ad_account_id`, `date`, grain IDs and names, `impressions`, `reach`, `frequency`, `clicks`, `inline_link_clicks`, `spend`, `cpm`, `cpc`, `ctr`, and the `actions` conversion list. The placement stream adds `publisher_platform` and `platform_position`.

**Instagram filter.** With `instagram_filter: true`, every request asks Meta for Instagram delivery only, and any placement row whose `publisher_platform` is not `instagram` is dropped. Campaigns that only ran on Facebook do not appear at all.

**Incremental window.** The first sync requests each day from `start_date` to yesterday, one request per day per stream. Each later sync starts again at the last completed day minus `lookback_days` and rewrites those days in full. Conversions follow the ad set's attribution setting (by default, 7-day click and 1-day view), so set `lookback_days` to at least 7 if you need them to settle.

**Discover.** `skipprd discover` reads only `account_daily` for the last 3 days and does not save progress.

**Rate limits.** This source shares Meta's per-ad-account rate limit with any Meta Ads pipeline on the same account. Skipprd retries rate-limit (429) and server (5xx) responses with exponential backoff, up to 8 attempts per request. It honours `Retry-After`, waits 30 seconds on a 429 without one, and never waits more than 2 minutes between attempts.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `Meta Instagram Ads requires access_token, META_INSTAGRAM_ADS_ACCESS_TOKEN, or oauth_token_url + …` | No token is set, its environment variable is empty, or only some `oauth_*` fields are set. | Export the token, or set all four OAuth fields. |
| `ad_account_id is required` | `ad_account_id` is empty. | Set the ad account ID. |
| Tables are empty but Meta Ads has data | The account had no Instagram delivery in the window. | Check placements in Ads Manager, or set `instagram_filter: false`. |
| Error code 190 (invalid or expired token) | The token expired, was revoked, or its user lost access. | Generate a new system user token. |
| Error code 200 or 10 (permission) | The token lacks `ads_read`, or the system user was not assigned the ad account. | Regenerate the token with `ads_read` and assign the ad account. |
| Error about an unsupported or deprecated version | Meta retires each Graph API version about two years after release. | Set `api_version` to a current version. |
| `sink '…' does not support write policy ReplacePartition` | The destination cannot rewrite daily partitions. | Use Athena, Athena Iceberg, or SkipprLake as the destination. |

## Next steps

- [Meta Ads](/connectors/inputs/meta_ads) lands all-placement totals.
- [How sources land](/concepts/source-landing-semantics) explains partition replacement and lookback.
- [skipprd sync](/cli/sync) covers scheduling and continuous mode.
