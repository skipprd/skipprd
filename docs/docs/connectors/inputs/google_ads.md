---
title: Google Ads
description: Sync daily Google Ads performance by account, campaign, ad group, keyword, search term, and landing page into your warehouse.
---

# Google Ads

The Google Ads source reads daily performance reports from the Google Ads API: impressions, clicks, cost, and conversions at account, campaign, ad group, keyword, search term, and landing page grain. Use it when you want paid search spend and results in your own warehouse next to your other data, refreshed on a schedule and corrected automatically as Google revises recent days.

## Before you begin

You need a Google Ads developer token, an OAuth client with a refresh token, the customer ID to read, and a destination that can rewrite daily partitions.

1. **Get a developer token.** Sign in to your Google Ads **manager** account and open **Admin → API Center**. Copy the developer token. A new token has test access and only works with test accounts until Google approves at least Basic access.
2. **Create an OAuth client.** In the [Google Cloud Console](https://console.cloud.google.com/), select a project, open **APIs & Services → Library**, and enable the **Google Ads API**. Configure the **OAuth consent screen**, then create an **OAuth client ID** under **APIs & Services → Credentials**. Copy the client ID and client secret.
3. **Get a refresh token.** Open the [OAuth 2.0 Playground](https://developers.google.com/oauthplayground/), click the gear icon, choose **Use your own OAuth credentials**, and paste your client ID and secret. (For a **Web application** client, first add `https://developers.google.com/oauthplayground` as an authorized redirect URI.) Authorize the scope `https://www.googleapis.com/auth/adwords` with a Google user that can see the Ads account, then exchange the code and copy the **refresh token**.
4. **Collect the customer ID.** It is the 10-digit number at the top of the Google Ads UI. Dashes are fine; Skipprd removes them. If you reach the account through a manager account, also note the manager's customer ID for `login_customer_id`.
5. **Set up a destination.** Report streams are rewritten one day at a time, so the destination must support partition replacement: [Athena](/connectors/outputs/athena), [Athena Iceberg](/connectors/outputs/athenaiceberg), or [SkipprLake](/connectors/outputs/skipprlake). The examples below assume a data sink named `lake` already exists in `skippr.yml`.

Export the secrets in the shell that runs Skipprd:

```bash
export GOOGLE_ADS_DEVELOPER_TOKEN="your-developer-token"
export GOOGLE_ADS_CLIENT_ID="1234567890-abc.apps.googleusercontent.com"
export GOOGLE_ADS_CLIENT_SECRET="your-client-secret"
export GOOGLE_ADS_REFRESH_TOKEN="1//your-refresh-token"
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSourceGoogleAds, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "google_ads",
    DataSourceGoogleAds(
        customer_id="1234567890",
        developer_token=EnvRef("GOOGLE_ADS_DEVELOPER_TOKEN"),
        start_date="2026-01-01",
        oauth_token_url="https://oauth2.googleapis.com/token",
        oauth_client_id="${GOOGLE_ADS_CLIENT_ID}",
        oauth_client_secret=EnvRef("GOOGLE_ADS_CLIENT_SECRET"),
        oauth_refresh_token=EnvRef("GOOGLE_ADS_REFRESH_TOKEN"),
    ),
)
cfg.pipeline("google_ads", Pipeline(data_source=src, data_sink=cfg.get_data_sink("lake")))
cfg.save()
```

```bash [CLI]
skipprd connect data-source google-ads \
  --pipeline google_ads \
  --name google_ads \
  --customer-id 1234567890 \
  --developer-token '${GOOGLE_ADS_DEVELOPER_TOKEN}' \
  --start-date 2026-01-01 \
  --oauth-token-url https://oauth2.googleapis.com/token \
  --oauth-client-id '${GOOGLE_ADS_CLIENT_ID}' \
  --oauth-client-secret '${GOOGLE_ADS_CLIENT_SECRET}' \
  --oauth-refresh-token '${GOOGLE_ADS_REFRESH_TOKEN}'
```

```yaml [YAML]
pipelines:
  google_ads:
    data_source: data_sources.google_ads
    data_sink: data_sinks.lake

data_sources:
  google_ads:
    GoogleAds:
      customer_id: "1234567890"
      developer_token: ${GOOGLE_ADS_DEVELOPER_TOKEN}
      start_date: "2026-01-01"
      oauth_token_url: https://oauth2.googleapis.com/token
      oauth_client_id: ${GOOGLE_ADS_CLIENT_ID}
      oauth_client_secret: ${GOOGLE_ADS_CLIENT_SECRET}
      oauth_refresh_token: ${GOOGLE_ADS_REFRESH_TOKEN}
```

:::

With the CLI, wire the `lake` destination into the same pipeline with `skipprd connect data-sink … --pipeline google_ads` if it is not there already.

All four `oauth_*` fields are required for Skipprd to refresh tokens itself. If you leave out `oauth_token_url`, Skipprd ignores the other three and fails with a missing-credentials error.

### Check it worked

```bash
skipprd discover --pipeline google_ads
skipprd sync --pipeline google_ads --once
```

Then query `google_ads_campaign_daily` in your destination. You should see one row per campaign per day from `start_date` through yesterday.

## Options

| Key | Type | Default | Description |
|---|---|---|---|
| `customer_id` | string | Required | Google Ads customer ID to read. Dashes are removed. |
| `developer_token` | secret | Required | Developer token from the manager account's API Center. |
| `start_date` | string | Required | First report day, `YYYY-MM-DD`. |
| `login_customer_id` | string | — | Manager account ID to authenticate through, when the OAuth user reaches `customer_id` via a manager account. |
| `oauth_token_url` | string | — | Token endpoint for refresh. Use `https://oauth2.googleapis.com/token`. |
| `oauth_client_id` | string | — | OAuth client ID. |
| `oauth_client_secret` | secret | — | OAuth client secret. |
| `oauth_refresh_token` | secret | — | Refresh token with the `adwords` scope. |
| `access_token` | secret | — | A short-lived bearer token. When set, it is used instead of OAuth refresh. It expires after about an hour, so use it only for quick tests. |
| `end_date` | string | — | Last report day, `YYYY-MM-DD`. Without it, Skipprd syncs through yesterday. |
| `lookback_days` | integer | `3` | Days before the last completed day to fetch again on every sync, so late conversions and corrections land. |
| `processing_lag_days` | integer | `1` | Never sync days more recent than this many days ago. |
| `stream_profile` | string | `full` | Which streams to sync: `minimal`, `standard`, or `full`. See [What gets synced](#what-gets-synced). |
| `streams` | list of strings | — | Exact stream names to sync, for example `[google_ads.campaign_daily]`. Overrides `stream_profile`. |
| `api_version` | string | `v20` | Google Ads API version in the request path. |

Secret fields only accept `${NAME}` environment references (`EnvRef("NAME")` in Python), so credentials never get written into `skippr.yml`.

## What gets synced

Each stream lands as its own table. The table name is the stream name with the dot replaced by an underscore.

| Stream | Table | Profiles | One row per |
|---|---|---|---|
| `google_ads.account_daily` | `google_ads_account_daily` | minimal, standard, full | customer per day |
| `google_ads.campaign_daily` | `google_ads_campaign_daily` | standard, full | campaign per day |
| `google_ads.ad_group_daily` | `google_ads_ad_group_daily` | standard, full | ad group per day |
| `google_ads.keyword_daily` | `google_ads_keyword_daily` | full | keyword (criterion) per ad group per day |
| `google_ads.search_term_daily` | `google_ads_search_term_daily` | full | search term per ad group per day |
| `google_ads.landing_page_daily` | `google_ads_landing_page_daily` | full | landing page URL per day |

Every row has `date`, `customer_id`, the IDs and names for its grain (`campaign_id`, `campaign_name`, `ad_group_id`, `keyword_text`, `search_term`, `landing_page_url`, and so on), and the metrics `impressions`, `clicks`, `cost_micros`, and `conversions`. `spend` is `cost_micros` divided by 1,000,000, in the account currency.

**Incremental window.** The first sync reads every day from `start_date` to yesterday, one API request per stream per day. Each later sync starts again at the last completed day minus `lookback_days` and rewrites those days in full, so a re-fetched day replaces the old numbers rather than adding to them.

**Attribution.** Google attributes conversions to the day of the click, so conversion counts for recent days keep rising until your conversion window closes. If your conversion window is 30 days and you need final numbers, set `lookback_days: 30`.

**Discover.** `skipprd discover` reads only `google_ads.account_daily` and does not save progress, so it never shortens your first real sync.

**Rate limits.** Skipprd retries rate-limit (429) and server (5xx) responses with exponential backoff, up to 8 attempts per request. It honours `Retry-After`, waits 30 seconds on a 429 without one, and never waits more than 2 minutes between attempts. Google's daily operation quota depends on your developer token's access level, so a long backfill on the `full` profile (six requests per day of history) can hit it. Backfill with `stream_profile: standard` first if needed.

For how report streams replace their daily partitions, see [How sources land](/concepts/source-landing-semantics).

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `Google Ads requires access_token, GOOGLE_ADS_ACCESS_TOKEN, or OAuth refresh credentials` | One of the four `oauth_*` fields is missing, usually `oauth_token_url`. | Set all four fields, with `oauth_token_url: https://oauth2.googleapis.com/token`. |
| `developer_token is required` | `developer_token` is empty or its environment variable is not set. | Export `GOOGLE_ADS_DEVELOPER_TOKEN` in the shell or service that runs Skipprd. |
| HTTP 401 on every request | The refresh token was revoked, or it belongs to a different OAuth client. Refresh tokens from a consent screen in **Testing** status expire after 7 days. | Generate a new refresh token with the same client ID and secret. For long-running pipelines, publish the consent screen or use an **Internal** app. |
| HTTP 403 `DEVELOPER_TOKEN_NOT_APPROVED` | The developer token only has test access. | Apply for Basic access in the API Center, or point the pipeline at a test account. |
| HTTP 403 `USER_PERMISSION_DENIED` | The OAuth user reaches the account through a manager account. | Set `login_customer_id` to the manager account ID. |
| HTTP 404 or an unsupported-version error | Google has retired the API version Skipprd is calling. | Set `api_version` to a version Google currently supports. |
| `sink '…' does not support write policy ReplacePartition` | The destination cannot rewrite daily partitions. | Use Athena, Athena Iceberg, or SkipprLake as the destination. |
| Recent conversions look low | Conversions are still arriving for those days. | Increase `lookback_days` to cover your conversion window. |

## Next steps

- [How sources land](/concepts/source-landing-semantics) explains partition replacement for report streams.
- [skipprd sync](/cli/sync) covers scheduling and continuous mode.
- [Google Analytics (GA4)](/connectors/inputs/google_analytics) lands site analytics to join with ad spend.
