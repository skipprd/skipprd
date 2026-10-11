---
title: X Ads
description: Sync daily X (Twitter) Ads line item analytics plus your accounts, campaigns, line items, and promoted posts into your warehouse.
---

# X Ads

The X Ads source reads daily line item analytics (engagement, billing, and video metrics) from the X Ads API, plus the current list of ad accounts, campaigns, line items, and promoted posts for one ad account. Use it to report X spend and engagement next to your other paid channels in your own warehouse.

## Before you begin

You need a developer app approved for the X Ads API, OAuth 1.0a keys for a user who can read the ad account, the ad account ID, and a destination that can rewrite daily partitions.

1. **Get Ads API access.** In the [X Developer Portal](https://developer.x.com/), create a project and app, then [apply for X Ads API access](https://developer.x.com/en/docs/x-ads-api/getting-started) for that app and wait for approval.
2. **Collect the app keys.** On the app's **Keys and tokens** page, copy the **API key** and **API key secret** (the OAuth 1.0a consumer key and secret).
3. **Collect user tokens.** On the same page, generate an **Access token** and **Access token secret**. Generate them while signed in as a user who has access to the ad account in [X Ads](https://ads.x.com/); add that user under the ad account's **Edit access to account** if needed.
4. **Find the ad account ID.** In X Ads, the account ID is the short alphanumeric ID (for example `18ce54d4x5t`) in the URL and the account menu.
5. **Set up a destination.** The analytics stream is rewritten one day at a time, so the destination must support partition replacement: [Athena](/connectors/outputs/athena), [Athena Iceberg](/connectors/outputs/athenaiceberg), or [SkipprLake](/connectors/outputs/skipprlake). The examples below assume a data sink named `lake` already exists in `skippr.yml`.

```bash
export X_ADS_CONSUMER_KEY="your-api-key"
export X_ADS_CONSUMER_SECRET="your-api-key-secret"
export X_ADS_OAUTH_TOKEN="your-access-token"
export X_ADS_OAUTH_TOKEN_SECRET="your-access-token-secret"
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSourceXAds, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "x_ads",
    DataSourceXAds(
        account_id="18ce54d4x5t",
        start_date="2026-01-01",
        oauth_consumer_key=EnvRef("X_ADS_CONSUMER_KEY"),
        oauth_consumer_secret=EnvRef("X_ADS_CONSUMER_SECRET"),
        oauth_token=EnvRef("X_ADS_OAUTH_TOKEN"),
        oauth_token_secret=EnvRef("X_ADS_OAUTH_TOKEN_SECRET"),
    ),
)
cfg.pipeline("x_ads", Pipeline(data_source=src, data_sink=cfg.get_data_sink("lake")))
cfg.save()
```

```bash [CLI]
skipprd connect data-source x-ads \
  --pipeline x_ads \
  --name x_ads \
  --account-id 18ce54d4x5t \
  --start-date 2026-01-01 \
  --oauth-consumer-key '${X_ADS_CONSUMER_KEY}' \
  --oauth-consumer-secret '${X_ADS_CONSUMER_SECRET}' \
  --oauth-token '${X_ADS_OAUTH_TOKEN}' \
  --oauth-token-secret '${X_ADS_OAUTH_TOKEN_SECRET}'
```

```yaml [YAML]
pipelines:
  x_ads:
    data_source: data_sources.x_ads
    data_sink: data_sinks.lake

data_sources:
  x_ads:
    XAds:
      account_id: "18ce54d4x5t"
      start_date: "2026-01-01"
      oauth_consumer_key: ${X_ADS_CONSUMER_KEY}
      oauth_consumer_secret: ${X_ADS_CONSUMER_SECRET}
      oauth_token: ${X_ADS_OAUTH_TOKEN}
      oauth_token_secret: ${X_ADS_OAUTH_TOKEN_SECRET}
```

:::

With the CLI, wire the `lake` destination into the same pipeline with `skipprd connect data-sink … --pipeline x_ads` if it is not there already.

### Check it worked

```bash
skipprd discover --pipeline x_ads
skipprd sync --pipeline x_ads --once
```

Then query `x_ads_analytics_daily` in your destination. You should see one row per line item per day from `start_date` through yesterday.

## Options

| Key | Type | Default | Description |
|---|---|---|---|
| `account_id` | string | Required | X Ads account ID. |
| `start_date` | string | Required | First report day, `YYYY-MM-DD`. |
| `oauth_consumer_key` | secret | — | App API key. |
| `oauth_consumer_secret` | secret | — | App API key secret. |
| `oauth_token` | secret | — | User access token. |
| `oauth_token_secret` | secret | — | User access token secret. |
| `bearer_token` | secret | — | A bearer token to send instead of OAuth 1.0a signing. Used before the OAuth keys when set. |
| `access_token` | secret | — | Same as `bearer_token`. Used only when `bearer_token` is not set. |
| `ad_account_id` | string | — | Alternative name for `account_id`. Used only when `account_id` is empty. |
| `end_date` | string | — | Last report day, `YYYY-MM-DD`. Without it, Skipprd syncs through yesterday. |
| `lookback_days` | integer | `3` | Days before the last completed day to fetch again on every sync, so late numbers land. |
| `processing_lag_days` | integer | `1` | Never sync days more recent than this many days ago. |
| `stream_profile` | string | `full` | Which streams to sync: `minimal`, `standard`, or `full`. |
| `streams` | list of strings | — | Exact stream names to sync. Overrides `stream_profile`. |

Set either all four `oauth_*` keys or a bearer token. Secret fields only accept `${NAME}` environment references (`EnvRef("NAME")` in Python).

## What gets synced

Each stream lands as its own table named after the stream, with the dot replaced by an underscore.

| Stream | Table | One row per | Profiles |
|---|---|---|---|
| `x_ads.analytics_daily` | `x_ads_analytics_daily` | line item per day | minimal, standard, full |
| `x_ads.accounts` | `x_ads_accounts` | ad account the user can see | standard, full |
| `x_ads.campaigns` | `x_ads_campaigns` | campaign in the ad account | standard, full |
| `x_ads.line_items` | `x_ads_line_items` | line item (ad group) in the ad account | standard, full |
| `x_ads.promoted_posts` | `x_ads_promoted_posts` | promoted post in the ad account | full |

`analytics_daily` rows have `account_id`, `date`, `entity_type` (always `LINE_ITEM`), `entity_id` (the line item ID), and one column per X metric in the engagement, billing, and video groups, such as `impressions`, `engagements`, `clicks`, `billed_charge_local_micro`, and `video_total_views`. Skipprd also writes `spend`, which is `billed_charge_local_micro` divided by 1,000,000, in the account currency. Listing rows keep X's fields and add `account_id` and the entity's ID column (`campaign_id`, `line_item_id`, `promoted_post_id`).

**Report window.** The first sync requests each day from `start_date` to yesterday, one request per day, using UTC days. Each later sync starts again at the last completed day minus `lookback_days` and rewrites those days in full, so updated billing numbers replace the old ones.

**Listings.** The account, campaign, line item, and promoted post streams read the current list on every sync and are written append-only, with no date column. Deduplicate on the ID column in your warehouse if you need one row per entity.

**Discover.** `skipprd discover` reads only `analytics_daily` and does not save progress. It still starts at `start_date`, so keep `start_date` recent if you want a fast first discover.

**Rate limits.** X limits Ads API analytics requests per account in 15-minute windows. Skipprd retries rate-limit (429) and server (5xx) responses with exponential backoff, up to 8 attempts per request. It honours `Retry-After`, waits 30 seconds on a 429 without one, and never waits more than 2 minutes between attempts. A long backfill makes one analytics request per day, so expect it to pause when you hit the window.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `X Ads requires OAuth 1.0a credentials or bearer_token` | Neither all four `oauth_*` keys nor a bearer token are set, or their environment variables are empty. | Export all four keys in the shell or service that runs Skipprd. |
| `account_id is required` | Both `account_id` and `ad_account_id` are empty. | Set `account_id`. |
| HTTP 401 | The keys or tokens are wrong or were regenerated, or the app lacks Ads API access. | Copy fresh keys and tokens from **Keys and tokens**, and check the app's Ads API approval. |
| HTTP 403 or 404 for the account | The user behind the access token has no access to the ad account. | Grant the user access in X Ads, then regenerate the access token and secret. |
| Duplicate campaigns or line items in listing tables | Listings are appended on every sync. | Expected. Deduplicate on the ID column. |
| `sink '…' does not support write policy ReplacePartition` | The destination cannot rewrite daily partitions. | Use Athena, Athena Iceberg, or SkipprLake as the destination. |

## Next steps

- [How sources land](/concepts/source-landing-semantics) explains partition replacement and append-only listings.
- [LinkedIn Ads](/connectors/inputs/linkedin_ads) and [Meta Ads](/connectors/inputs/meta_ads) add your other paid channels.
- [skipprd sync](/cli/sync) covers scheduling and continuous mode.
