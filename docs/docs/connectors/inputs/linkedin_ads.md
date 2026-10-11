---
title: LinkedIn Ads
description: Sync daily LinkedIn Campaign Manager analytics by campaign, plus your ad accounts, campaign groups, campaigns, and creatives.
---

# LinkedIn Ads

The LinkedIn Ads source reads daily campaign analytics from the LinkedIn Marketing API, plus the current list of ad accounts, campaign groups, campaigns, and creatives for one ad account. Use it to report LinkedIn spend and results next to your other paid channels in your own warehouse.

## Before you begin

You need a LinkedIn developer app with Advertising API access, a token for a user who can read the ad account, the ad account ID, and a destination that can rewrite daily partitions.

1. **Create an app.** In the [LinkedIn Developer Portal](https://www.linkedin.com/developers/apps), create an app linked to your company page. Under **Products**, request **Advertising API** access and wait for approval.
2. **Get a token.** Use the portal's **OAuth 2.0 tools** (or your own OAuth flow) to generate an access token with the `r_ads` and `r_ads_reporting` scopes, signed in as a user with at least **Viewer** access to the ad account in Campaign Manager. Access tokens last about 60 days. If LinkedIn has enabled refresh tokens for your app, also keep the refresh token, client ID, and client secret so Skipprd can renew the token itself.
3. **Find the ad account ID.** In [Campaign Manager](https://www.linkedin.com/campaignmanager/), the account ID is the number in the account list and in the URL. Both `123456789` and `urn:li:sponsoredAccount:123456789` work.
4. **Set up a destination.** The analytics stream is rewritten one day at a time, so the destination must support partition replacement: [Athena](/connectors/outputs/athena), [Athena Iceberg](/connectors/outputs/athenaiceberg), or [SkipprLake](/connectors/outputs/skipprlake). The examples below assume a data sink named `lake` already exists in `skippr.yml`.

```bash
export LINKEDIN_ADS_ACCESS_TOKEN="your-access-token"
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSourceLinkedInAds, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "linkedin_ads",
    DataSourceLinkedInAds(
        ad_account_id="123456789",
        start_date="2026-01-01",
        access_token=EnvRef("LINKEDIN_ADS_ACCESS_TOKEN"),
    ),
)
cfg.pipeline("linkedin_ads", Pipeline(data_source=src, data_sink=cfg.get_data_sink("lake")))
cfg.save()
```

```bash [CLI]
skipprd connect data-source linked-in-ads \
  --pipeline linkedin_ads \
  --name linkedin_ads \
  --ad-account-id 123456789 \
  --start-date 2026-01-01 \
  --access-token '${LINKEDIN_ADS_ACCESS_TOKEN}'
```

```yaml [YAML]
pipelines:
  linkedin_ads:
    data_source: data_sources.linkedin_ads
    data_sink: data_sinks.lake

data_sources:
  linkedin_ads:
    LinkedInAds:
      ad_account_id: "123456789"
      start_date: "2026-01-01"
      access_token: ${LINKEDIN_ADS_ACCESS_TOKEN}
```

:::

The CLI subcommand is `linked-in-ads` and the YAML kind is `LinkedInAds`. To let Skipprd renew tokens, replace `access_token` with `oauth_token_url: https://www.linkedin.com/oauth/v2/accessToken`, `oauth_client_id`, `oauth_client_secret`, and `oauth_refresh_token`.

With the CLI, wire the `lake` destination into the same pipeline with `skipprd connect data-sink … --pipeline linkedin_ads` if it is not there already.

### Check it worked

```bash
skipprd discover --pipeline linkedin_ads
skipprd sync --pipeline linkedin_ads --once
```

Then query `linkedin_ads_ad_analytics_daily` in your destination. You should see one row per campaign per day from `start_date` through yesterday.

## Options

| Key | Type | Default | Description |
|---|---|---|---|
| `ad_account_id` | string | Required | Campaign Manager ad account ID, with or without the `urn:li:sponsoredAccount:` prefix. |
| `start_date` | string | Required | First report day, `YYYY-MM-DD`. |
| `access_token` | secret | — | Bearer token with `r_ads` and `r_ads_reporting`. Used before OAuth refresh. |
| `oauth_token_url` | string | — | Token endpoint, `https://www.linkedin.com/oauth/v2/accessToken`. |
| `oauth_client_id` | string | — | App client ID. |
| `oauth_client_secret` | secret | — | App client secret. |
| `oauth_refresh_token` | secret | — | Refresh token. |
| `rest_version` | string | `202411` | LinkedIn API version, sent as the `LinkedIn-Version` header (`YYYYMM`). |
| `api_version` | string | — | Older name for `rest_version`. Used only when `rest_version` is not set. |
| `end_date` | string | — | Last report day, `YYYY-MM-DD`. Without it, Skipprd syncs through yesterday. |
| `lookback_days` | integer | `3` | Days before the last completed day to fetch again on every sync, so late conversions land. |
| `processing_lag_days` | integer | `1` | Never sync days more recent than this many days ago. |
| `stream_profile` | string | `full` | Which streams to sync: `minimal`, `standard`, or `full`. |
| `streams` | list of strings | — | Exact stream names to sync. Overrides `stream_profile`. |

If you set any `oauth_*` field you must set all four. Secret fields only accept `${NAME}` environment references (`EnvRef("NAME")` in Python).

## What gets synced

Each stream lands as its own table named after the stream, with the dot replaced by an underscore.

| Stream | Table | One row per | Profiles |
|---|---|---|---|
| `linkedin_ads.ad_analytics_daily` | `linkedin_ads_ad_analytics_daily` | campaign per day | minimal, standard, full |
| `linkedin_ads.ad_accounts` | `linkedin_ads_ad_accounts` | active, draft, or paused ad account the token can see | standard, full |
| `linkedin_ads.campaign_groups` | `linkedin_ads_campaign_groups` | campaign group in the ad account | standard, full |
| `linkedin_ads.campaigns` | `linkedin_ads_campaigns` | campaign in the ad account | standard, full |
| `linkedin_ads.creatives` | `linkedin_ads_creatives` | creative in the ad account | full |

`ad_analytics_daily` rows have `account_urn`, `date`, `pivot_value` and `campaign_urn` (the campaign), and the metrics LinkedIn returns. When LinkedIn returns `costInUsd`, Skipprd also writes it as `spend`. Listing rows keep LinkedIn's fields and add `account_urn` and the entity's URN column (`campaign_urn`, `creative_urn`, and so on).

**Report window.** The first sync requests each day from `start_date` to yesterday, one request per day. Each later sync starts again at the last completed day minus `lookback_days` and rewrites those days in full. LinkedIn attributes conversions back to the click or view, so raise `lookback_days` to your conversion window if you need final numbers.

**Listings.** The account, campaign group, campaign, and creative streams read the current list on every sync and are written append-only, with no date column. Deduplicate on the URN column in your warehouse if you need one row per entity.

**Discover.** `skipprd discover` reads only `ad_analytics_daily` and does not save progress. It still starts at `start_date`, so keep `start_date` recent if you want a fast first discover.

**Rate limits.** LinkedIn sets daily request limits per app. Skipprd retries rate-limit (429) and server (5xx) responses with exponential backoff, up to 8 attempts per request. It honours `Retry-After`, waits 30 seconds on a 429 without one, and never waits more than 2 minutes between attempts.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `LinkedIn Ads requires access_token, LINKEDIN_ADS_ACCESS_TOKEN, or OAuth refresh credentials` | No token is set, or its environment variable is empty. | Export the token in the shell or service that runs Skipprd. |
| `LinkedIn Ads OAuth refresh requires oauth_token_url, oauth_client_id, oauth_client_secret, and oauth_refresh_token` | Only some OAuth fields are set. | Set all four, or remove them and use `access_token`. |
| HTTP 401 | The access token expired (after about 60 days) or was revoked. | Generate a new token, or configure OAuth refresh. |
| HTTP 403 | The app lacks Advertising API access, the token lacks `r_ads_reporting`, or the user cannot see the ad account. | Check the app's products, the token's scopes, and the user's role in Campaign Manager. |
| HTTP 426 or an error about the version | LinkedIn retires each monthly API version about a year after release. | Set `rest_version` to a current `YYYYMM` version. |
| Duplicate campaigns or creatives in listing tables | Listings are appended on every sync. | Expected. Deduplicate on the URN column. |
| `sink '…' does not support write policy ReplacePartition` | The destination cannot rewrite daily partitions. | Use Athena, Athena Iceberg, or SkipprLake as the destination. |

## Next steps

- [How sources land](/concepts/source-landing-semantics) explains partition replacement and append-only listings.
- [Google Ads](/connectors/inputs/google_ads) and [Meta Ads](/connectors/inputs/meta_ads) add your other paid channels.
- [skipprd sync](/cli/sync) covers scheduling and continuous mode.
