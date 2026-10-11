---
title: AdRoll Ads
description: Sync daily AdRoll ad performance and your campaigns, ad groups, and ads into your warehouse with Skipprd.
---

# AdRoll Ads

The AdRoll Ads source reads daily ad-level performance (impressions, clicks, spend, conversions, and revenue) from the AdRoll reporting API, plus the current list of advertisers, campaigns, ad groups, and ads. Use it to report AdRoll retargeting spend and results next to your other channels in your own warehouse.

## Before you begin

You need an AdRoll token that can read the advertiser, the advertiser's ID, and a destination that can rewrite daily partitions.

1. **Get a token.** In the [NextRoll developer portal](https://developers.nextroll.com/), sign in with your AdRoll account, create an app, and generate a **personal access token**. Use a token created by a user who can see the advertiser you want to sync. If you run your own OAuth app instead, collect its token URL, client ID, client secret, and a refresh token.
2. **Find the advertiser ID.** AdRoll calls this the advertiser (advertisable) **EID**, a string such as `ABCDEFGHIJKLMNOP12345`. You can find it in the AdRoll app URL or account settings for the advertiser.
3. **Set up a destination.** The daily report is rewritten one day at a time, so the destination must support partition replacement: [Athena](/connectors/outputs/athena), [Athena Iceberg](/connectors/outputs/athenaiceberg), or [SkipprLake](/connectors/outputs/skipprlake). The examples below assume a data sink named `lake` already exists in `skippr.yml`.

```bash
export ADROLL_ADS_ACCESS_TOKEN="your-personal-access-token"
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSourceAdRollAds, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "adroll",
    DataSourceAdRollAds(
        advertiser_id="ABCDEFGHIJKLMNOP12345",
        start_date="2026-01-01",
        personal_access_token=EnvRef("ADROLL_ADS_ACCESS_TOKEN"),
    ),
)
cfg.pipeline("adroll", Pipeline(data_source=src, data_sink=cfg.get_data_sink("lake")))
cfg.save()
```

```bash [CLI]
skipprd connect data-source ad-roll-ads \
  --pipeline adroll \
  --name adroll \
  --advertiser-id ABCDEFGHIJKLMNOP12345 \
  --start-date 2026-01-01 \
  --personal-access-token '${ADROLL_ADS_ACCESS_TOKEN}'
```

```yaml [YAML]
pipelines:
  adroll:
    data_source: data_sources.adroll
    data_sink: data_sinks.lake

data_sources:
  adroll:
    AdRollAds:
      advertiser_id: "ABCDEFGHIJKLMNOP12345"
      start_date: "2026-01-01"
      personal_access_token: ${ADROLL_ADS_ACCESS_TOKEN}
```

:::

The CLI subcommand is `ad-roll-ads` and the YAML kind is `AdRollAds`. With the CLI, wire the `lake` destination into the same pipeline with `skipprd connect data-sink … --pipeline adroll` if it is not there already.

### Check it worked

```bash
skipprd discover --pipeline adroll
skipprd sync --pipeline adroll --once
```

Then query `adroll_ads_reporting_daily` in your destination. You should see one row per ad per day from `start_date` through yesterday.

## Options

| Key | Type | Default | Description |
|---|---|---|---|
| `advertiser_id` | string | Required | AdRoll advertiser EID. |
| `start_date` | string | Required | First report day, `YYYY-MM-DD`. |
| `personal_access_token` | secret | — | AdRoll personal access token, sent as a bearer token. |
| `access_token` | secret | — | Any other AdRoll bearer token. Used before `personal_access_token` when both are set. |
| `oauth_token_url` | string | — | Token endpoint of your AdRoll OAuth app. |
| `oauth_client_id` | string | — | OAuth client ID. |
| `oauth_client_secret` | secret | — | OAuth client secret. |
| `oauth_refresh_token` | secret | — | OAuth refresh token. Used only when no access token is set. |
| `end_date` | string | — | Last report day, `YYYY-MM-DD`. Without it, Skipprd syncs through yesterday. |
| `lookback_days` | integer | `3` | Days before the last completed day to fetch again on every sync, so late conversions land. |
| `processing_lag_days` | integer | `1` | Never sync days more recent than this many days ago. |
| `stream_profile` | string | `full` | Which streams to sync: `minimal`, `standard`, or `full`. |
| `streams` | list of strings | — | Exact stream names to sync. Overrides `stream_profile`. |
| `api_base_url` | string | `https://services.adroll.com/api/v1` | Base URL for the advertiser, campaign, ad group, and ad listings. Change only if AdRoll gives you another endpoint. |
| `reporting_base_url` | string | `https://services.adroll.com/reporting/api/v1/query` | Reporting API endpoint. Change only if AdRoll gives you another endpoint. |

If you set any `oauth_*` field you must set all four. Secret fields only accept `${NAME}` environment references (`EnvRef("NAME")` in Python).

## What gets synced

Each stream lands as its own table named after the stream, with the dot replaced by an underscore.

| Stream | Table | One row per | Profiles |
|---|---|---|---|
| `adroll_ads.reporting_daily` | `adroll_ads_reporting_daily` | ad per day | minimal, standard, full |
| `adroll_ads.advertisers` | `adroll_ads_advertisers` | advertiser the token can see | standard, full |
| `adroll_ads.campaigns` | `adroll_ads_campaigns` | campaign | standard, full |
| `adroll_ads.ad_groups` | `adroll_ads_ad_groups` | ad group | full |
| `adroll_ads.ads` | `adroll_ads_ads` | ad | full |

`reporting_daily` rows have `advertiser_id`, `date`, `campaign_id`, `campaign_name`, `ad_group_id`, `ad_group_name`, `ad_id`, `ad_name`, `impressions`, `clicks`, `spend`, `conversions`, and `revenue`. The listing streams keep the fields AdRoll returns and add `advertiser_id` and the entity's ID column.

**Report window.** The first sync requests each day from `start_date` to yesterday, one request per day. Each later sync starts again at the last completed day minus `lookback_days` and rewrites those days in full, so updated conversion and revenue numbers replace the old ones. Raise `lookback_days` to match your attribution window if you need final numbers.

**Listings.** The advertiser, campaign, ad group, and ad streams read the full current list on every sync and are written append-only, with no date column. Deduplicate on the ID column in your warehouse if you need one row per entity.

**Discover.** `skipprd discover` reads only `reporting_daily` and does not save progress. It still starts at `start_date`, so keep `start_date` recent if you want a fast first discover.

**Rate limits.** Skipprd retries rate-limit (429) and server (5xx) responses with exponential backoff, up to 8 attempts per request. It honours `Retry-After`, waits 30 seconds on a 429 without one, and never waits more than 2 minutes between attempts.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `AdRoll Ads requires access_token, personal_access_token, ADROLL_ADS_ACCESS_TOKEN, or OAuth refresh credentials` | No token is set, or its environment variable is empty. | Export the token in the shell or service that runs Skipprd. |
| `AdRoll Ads OAuth refresh requires oauth_token_url, oauth_client_id, oauth_client_secret, and oauth_refresh_token` | Only some OAuth fields are set. | Set all four, or remove them and use a token. |
| `advertiser_id is required` | `advertiser_id` is empty. | Set the advertiser EID. |
| HTTP 401 or 403 | The token expired or its user cannot see the advertiser. | Generate a new token as a user with access to the advertiser. |
| `AdRoll GraphQL errors: …` | The reporting API rejected the request, usually because the EID is wrong or not visible to the token. | Check `advertiser_id` against the advertiser in AdRoll. |
| Duplicate campaigns or ads in listing tables | Listings are appended on every sync. | Expected. Deduplicate on the ID column. |
| `sink '…' does not support write policy ReplacePartition` | The destination cannot rewrite daily partitions. | Use Athena, Athena Iceberg, or SkipprLake as the destination. |

## Next steps

- [How sources land](/concepts/source-landing-semantics) explains partition replacement and append-only listings.
- [Meta Ads](/connectors/inputs/meta_ads) and [Google Ads](/connectors/inputs/google_ads) add your other paid channels.
- [skipprd sync](/cli/sync) covers scheduling and continuous mode.
