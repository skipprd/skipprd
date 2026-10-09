---
title: Google Analytics (GA4)
description: Sync daily GA4 reports on acquisition, engagement, content, geography, technology, and ecommerce into your warehouse with Skipprd.
---

# Google Analytics (GA4)

The Google Analytics source reads daily reports from the GA4 Data API: traffic and user acquisition, events and conversions, audience, content, geography, demographics, technology, ecommerce, and publisher ads. Each stream is one fixed set of dimensions per day, so you build rollups, pivots, and channel summaries in SQL rather than in GA4. Use it when you want GA4 numbers in your warehouse alongside ad spend and revenue. It does not read the event-level GA4 BigQuery export. For modeling ideas, see [GA4 bronze and modeling](/concepts/ga4-bronze-and-modeling).

## Before you begin

You need the GA4 property ID, a Google Cloud project with the Data API enabled, one set of credentials, and a destination that can rewrite daily partitions.

1. **Find the property ID.** In GA4, open **Admin → Property settings** and copy the numeric **Property ID**. Use the number only, without a `properties/` prefix.
2. **Enable the Data API.** In the [Google Cloud Console](https://console.cloud.google.com/), select or create a project, open **APIs & Services → Library**, and enable the **Google Analytics Data API**.
3. **Choose credentials.** Skipprd checks them in this order and uses the first one it finds: `access_token`, then OAuth refresh (all four `oauth_*` fields), then `service_account_json_path`, then the `GOOGLE_APPLICATION_CREDENTIALS` environment variable. Every method needs the scope `https://www.googleapis.com/auth/analytics.readonly` and at least **Viewer** on the property.

| Method | Use when | You provide |
|---|---|---|
| [Service account](#service-account) | Scheduled syncs on servers and CI | A JSON key file |
| [OAuth refresh](#oauth-refresh) | A person's Google account should own access, or GA4 will not accept a service account | Client ID, client secret, refresh token |
| [Access token](#access-token) | A first local test | A token that expires after about an hour |

4. **Set up a destination.** Every stream is rewritten one day at a time, so the destination must support partition replacement: [Athena](/connectors/outputs/athena), [Athena Iceberg](/connectors/outputs/athenaiceberg), or [SkipprLake](/connectors/outputs/skipprlake). The examples below assume a data sink named `lake` already exists in `skippr.yml`.

### Service account

Skipprd reads the JSON key and mints short-lived tokens on every sync.

1. In the Cloud Console, open **IAM & Admin → Service Accounts → Create service account**, name it (for example `skippr-ga4-read`), and click **Done**. It needs no project roles for Data API reads.
2. Open the account, then **Keys → Add key → Create new key → JSON**. Store the downloaded file like a password. Its `client_email` field is the account's email address.
3. In [Google Analytics](https://analytics.google.com/), open the property, then **Admin → Property access management → + → Add users**. Paste the `client_email`, choose **Viewer**, turn off **Notify new users by email**, and save.

Without step 3, syncs fail with HTTP 403 even though the key is valid.

**If GA4 says "This email doesn't match a Google Account".** The **Add users** dialog rejects `*.iam.gserviceaccount.com` addresses on many properties. Either switch to [OAuth refresh](#oauth-refresh), or grant access through the Google Analytics Admin API as a property **Administrator**:

```bash
gcloud auth application-default login \
  --scopes=https://www.googleapis.com/auth/analytics.manage.users,https://www.googleapis.com/auth/cloud-platform

export PROPERTY_ID="123456789"
export SA_EMAIL="skippr-ga4-read@my-project.iam.gserviceaccount.com"

curl -s -X POST \
  "https://analyticsadmin.googleapis.com/v1alpha/properties/${PROPERTY_ID}/accessBindings" \
  -H "Authorization: Bearer $(gcloud auth application-default print-access-token)" \
  -H "Content-Type: application/json" \
  -d "{\"user\": \"${SA_EMAIL}\", \"roles\": [\"predefinedRoles/viewer\"]}"
```

Enable the **Google Analytics Admin API** in the same Cloud project first. Afterwards, the service account appears under **Property access management**.

Then point Skipprd at the key with `service_account_json_path`, or set the standard Google variable instead:

```bash
export GOOGLE_APPLICATION_CREDENTIALS="/secure/path/skippr-ga4-key.json"
```

### OAuth refresh

Skipprd stores a refresh token and exchanges it for a new access token on every sync. Set all four fields: `oauth_token_url`, `oauth_client_id`, `oauth_client_secret`, and `oauth_refresh_token`. If any one is missing, Skipprd skips this method.

1. **Consent screen.** In **APIs & Services → OAuth consent screen**, choose **Internal** if you use Google Workspace and the property belongs to the same organization. Otherwise choose **External**, add your Google account under **Test users**, and add the scope `https://www.googleapis.com/auth/analytics.readonly`. You do not need Google's app verification for a private pipeline.
2. **Client.** In **APIs & Services → Credentials → Create credentials → OAuth client ID**, choose **Web application** and add the authorized redirect URI `https://developers.google.com/oauthplayground`. Copy the client ID and secret.
3. **Refresh token.** In the [OAuth 2.0 Playground](https://developers.google.com/oauthplayground/), click the gear icon, choose **Use your own OAuth credentials**, and paste the client ID and secret. Enter the scope `https://www.googleapis.com/auth/analytics.readonly`, click **Authorize APIs**, and sign in as a user with Viewer access on the property. Click **Exchange authorization code for tokens** and copy the **refresh token**.

If no refresh token appears, remove the app at [Google Account permissions](https://myaccount.google.com/permissions) and authorize again.

Use `https://oauth2.googleapis.com/token` as `oauth_token_url`. It is an API endpoint that only accepts POST requests, so opening it in a browser shows an error page. That is expected.

### Access token

An access token is the short-lived OAuth token (it starts with `ya29.`), not an API key from the GA4 UI. It expires after about an hour, so use it for a first test only. If you already use the Google Cloud CLI:

```bash
gcloud auth application-default login \
  --scopes=https://www.googleapis.com/auth/analytics.readonly,https://www.googleapis.com/auth/cloud-platform

export GA4_ACCESS_TOKEN="$(gcloud auth application-default print-access-token)"
```

You can also copy the **Access token** from step 3 of the OAuth Playground flow.

## Configure

Export the secrets for the method you chose. This example uses OAuth refresh:

```bash
export GA4_OAUTH_CLIENT_ID="1234567890-abc.apps.googleusercontent.com"
export GA4_OAUTH_CLIENT_SECRET="your-client-secret"
export GA4_OAUTH_REFRESH_TOKEN="1//your-refresh-token"
```

::: code-group

```python [Python]
from skippr import Config, DataSourceGoogleAnalytics, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "ga4",
    DataSourceGoogleAnalytics(
        property_id="123456789",
        start_date="2026-01-01",
        oauth_token_url="https://oauth2.googleapis.com/token",
        oauth_client_id="${GA4_OAUTH_CLIENT_ID}",
        oauth_client_secret=EnvRef("GA4_OAUTH_CLIENT_SECRET"),
        oauth_refresh_token=EnvRef("GA4_OAUTH_REFRESH_TOKEN"),
    ),
)
cfg.pipeline("ga4", Pipeline(data_source=src, data_sink=cfg.get_data_sink("lake")))
cfg.save()
```

```bash [CLI]
skipprd connect data-source google-analytics \
  --pipeline ga4 \
  --name ga4 \
  --property-id 123456789 \
  --start-date 2026-01-01 \
  --oauth-token-url https://oauth2.googleapis.com/token \
  --oauth-client-id '${GA4_OAUTH_CLIENT_ID}' \
  --oauth-client-secret '${GA4_OAUTH_CLIENT_SECRET}' \
  --oauth-refresh-token '${GA4_OAUTH_REFRESH_TOKEN}'
```

```yaml [YAML]
pipelines:
  ga4:
    data_source: data_sources.ga4
    data_sink: data_sinks.lake

data_sources:
  ga4:
    GoogleAnalytics:
      property_id: "123456789"
      start_date: "2026-01-01"
      oauth_token_url: https://oauth2.googleapis.com/token
      oauth_client_id: ${GA4_OAUTH_CLIENT_ID}
      oauth_client_secret: ${GA4_OAUTH_CLIENT_SECRET}
      oauth_refresh_token: ${GA4_OAUTH_REFRESH_TOKEN}
```

:::

For a service account, replace the four `oauth_*` fields with `service_account_json_path: /secure/path/skippr-ga4-key.json` (or omit credentials entirely when `GOOGLE_APPLICATION_CREDENTIALS` is set). For a test token, use `access_token: ${GA4_ACCESS_TOKEN}`. Set only one method.

With the CLI, wire the `lake` destination into the same pipeline with `skipprd connect data-sink … --pipeline ga4` if it is not there already.

### Check it worked

```bash
skipprd discover --pipeline ga4
skipprd sync --pipeline ga4 --once
```

Then query `google_analytics_traffic_acquisition_daily` in your destination. You should see sessions by channel, source, and medium for each day from `start_date` through yesterday.

## Options

| Key | Type | Default | Description |
|---|---|---|---|
| `property_id` | string | Required | Numeric GA4 property ID. |
| `start_date` | string | Required | First report day, `YYYY-MM-DD`. |
| `service_account_json_path` | string | — | Path to a service account JSON key. |
| `oauth_token_url` | string | — | Token endpoint for refresh. Use `https://oauth2.googleapis.com/token`. |
| `oauth_client_id` | string | — | OAuth client ID. |
| `oauth_client_secret` | secret | — | OAuth client secret. |
| `oauth_refresh_token` | secret | — | Refresh token with the `analytics.readonly` scope. |
| `access_token` | secret | — | Short-lived bearer token. Takes priority over every other method. |
| `end_date` | string | — | Last report day, `YYYY-MM-DD`. Without it, Skipprd syncs through yesterday. |
| `lookback_days` | integer | `3` | Days before the last completed day to fetch again on every sync, so GA4's revisions land. |
| `processing_lag_days` | integer | `1` | Never sync days more recent than this many days ago, while GA4 is still processing them. |
| `window_in_days` | integer | `1` | Days per Data API request, from 1 to 364. Keep `1`: wider windows can trigger GA4 sampling. |
| `keep_empty_rows` | boolean | `true` | Ask GA4 to return dimension combinations whose metrics are all zero. |
| `stream_profile` | string | `full` | Which streams to sync: `minimal`, `standard`, or `full`. |
| `streams` | list of strings | — | Exact stream names to sync, for example `[google_analytics.events_daily]`. Overrides `stream_profile`. |
| `request_interval_ms` | integer | `300` | Pause after each successful request, to stay under GA4's per-property quotas. |
| `max_api_retries` | integer | `12` | Attempts per request on rate-limit (429) and server (5xx) responses. |

Secret fields only accept `${NAME}` environment references (`EnvRef("NAME")` in Python).

## What gets synced

Each stream lands as its own table named after the stream, with the dot replaced by an underscore (for example `google_analytics_events_daily`). Every row has `property_id`, `date`, the stream's dimensions, and its metrics. A row is unique on `property_id`, `date`, and the dimensions.

| Stream | Dimensions (besides `date`) | Metrics | Profiles |
|---|---|---|---|
| `google_analytics.traffic_acquisition_daily` | `sessionDefaultChannelGroup`, `sessionSource`, `sessionMedium` | `sessions`, `totalUsers`, `conversions` | minimal, standard, full |
| `google_analytics.traffic_campaign_daily` | `sessionCampaignName`, `sessionSource`, `sessionMedium` | `sessions`, `totalUsers`, `conversions` | standard, full |
| `google_analytics.user_acquisition_daily` | `firstUserDefaultChannelGroup`, `firstUserSource`, `firstUserMedium` | `newUsers`, `totalUsers` | minimal, standard, full |
| `google_analytics.user_acquisition_campaign_daily` | `firstUserCampaignName`, `firstUserSource`, `firstUserMedium` | `newUsers`, `totalUsers` | standard, full |
| `google_analytics.events_daily` | `eventName` | `eventCount`, `totalUsers` | minimal, standard, full |
| `google_analytics.conversions_daily` | `eventName` | `conversions`, `totalRevenue` | minimal, standard, full |
| `google_analytics.audience_daily` | — | `activeUsers`, `newUsers`, `sessions`, `engagedSessions`, `averageSessionDuration` | standard, full |
| `google_analytics.audience_retention_daily` | — | `active1DayUsers`, `active7DayUsers`, `active28DayUsers` | standard, full |
| `google_analytics.content_pages_daily` | `pagePath` | `screenPageViews`, `sessions`, `totalUsers`, `engagementRate` | standard, full |
| `google_analytics.content_titles_daily` | `pageTitle` | same as pages | standard, full |
| `google_analytics.content_screens_daily` | `unifiedScreenClass` | same as pages | standard, full |
| `google_analytics.content_group_daily` | `contentGroup` | same as pages | standard, full |
| `google_analytics.geo_daily` | `country`, `region`, `city` | `sessions`, `totalUsers`, `newUsers` | standard, full |
| `google_analytics.demographics_age_daily` | `userAgeBracket` | `sessions`, `totalUsers`, `newUsers` | full |
| `google_analytics.demographics_gender_daily` | `userGender` | `sessions`, `totalUsers`, `newUsers` | full |
| `google_analytics.demographics_interest_daily` | `brandingInterest` | `sessions`, `totalUsers`, `newUsers` | full |
| `google_analytics.demographics_language_daily` | `language` | `sessions`, `totalUsers`, `newUsers` | full |
| `google_analytics.tech_daily` | `deviceCategory`, `operatingSystem`, `browser` | `sessions`, `totalUsers` | standard, full |
| `google_analytics.devices_daily` | `deviceCategory`, `mobileDeviceModel` | `sessions`, `totalUsers` | standard, full |
| `google_analytics.tech_platform_daily` | `platform`, `deviceCategory` | `sessions`, `totalUsers` | standard, full |
| `google_analytics.ecommerce_items_daily` | `itemName` | `itemsPurchased`, `itemRevenue`, `itemsAddedToCart` | full |
| `google_analytics.ecommerce_categories_daily` | `itemCategory` | `itemsPurchased`, `itemRevenue` | full |
| `google_analytics.publisher_ads_daily` | `adSourceName`, `adFormat`, `adUnitName` | `publisherAdClicks`, `publisherAdImpressions`, `adUnitExposure` | full |

`minimal` is 4 streams, `standard` is 16, and `full` is all 23.

**Optional streams.** The two ecommerce streams and `publisher_ads_daily` only work on properties with ecommerce or linked publisher ads. If GA4 rejects their dimensions or metrics, Skipprd skips that stream for the run and continues with the rest.

**Incremental window.** The first sync reads every day from `start_date`, one request per stream per day. Each later sync starts again at the last completed day minus `lookback_days` and rewrites those days in full, so revised GA4 numbers replace the old ones. Increase `lookback_days` if you rely on long attribution windows. Do not raise `window_in_days` for that.

**Row limits.** Each request returns one page, at most 10,000 rows (the Data API default). On large properties, high-cardinality streams such as `content_pages_daily`, `content_titles_daily`, `geo_daily`, and `devices_daily` can miss the long tail of a busy day. GA4 can also group rare values into `(other)` or hide them under its thresholding rules, so totals in these streams may differ slightly from the GA4 UI.

**Discover.** `skipprd discover` reads only the 4 `minimal` streams for the last 3 days and does not save progress, so it stays fast and never shortens your first real sync.

**Rate limits.** GA4 enforces per-property token quotas. Skipprd pauses `request_interval_ms` after each successful request and retries 429 and 5xx responses with exponential backoff (honouring `Retry-After`, waiting 30 seconds on a 429 without one, and never more than 2 minutes between attempts) up to `max_api_retries` attempts. A long backfill on the `full` profile makes 23 requests per day of history. If you keep hitting quota, backfill with `stream_profile: minimal` or `standard` first, or raise `request_interval_ms`.

For how each day's partition is replaced, see [How sources land](/concepts/source-landing-semantics).

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `GA4 requires access_token, OAuth refresh credentials, service_account_json_path, or GOOGLE_APPLICATION_CREDENTIALS` | No credential method is complete. OAuth refresh needs all four fields, including `oauth_token_url`. | Configure one method from [Before you begin](#before-you-begin). |
| HTTP 401 | The access token expired, or the refresh token was revoked. Refresh tokens from an **External** consent screen in **Testing** status expire after 7 days. | Generate a new token. For long-running pipelines, use a service account or an **Internal** consent screen. |
| HTTP 403 | The identity has no access to the property, the Data API is not enabled, or the token lacks the `analytics.readonly` scope. | Add the user or service account as **Viewer** under **Property access management** and enable the Data API in the same Cloud project. |
| **This email doesn't match a Google Account** when adding a service account | GA4's UI rejects service account addresses on many properties. | Grant access with the Admin API command in [Service account](#service-account), or use OAuth refresh. |
| `gcloud auth application-default login` says **This app is blocked** | Your Workspace admin blocks the Google Cloud SDK, or the wrong account is signed in. | You do not need `gcloud`. Use the OAuth Playground flow or a service account key. |
| OAuth Playground shows **Access blocked** | The consent screen is **External** and your account is not a test user. | Add your account under **Test users**, or use an **Internal** consent screen on Workspace. |
| Ecommerce or publisher ads tables are empty | The property has no ecommerce or linked ads data, so those optional streams are skipped. | Expected. Remove them with `streams` or use `stream_profile: standard`. |
| Recent days change between syncs | GA4 is still processing those days. | Expected. `lookback_days` re-fetches them; increase `processing_lag_days` to wait longer before the first read. |
| Numbers look sampled | `window_in_days` is greater than 1. | Set `window_in_days: 1`. |
| HTTP 429 after many retries | The property's quota is exhausted for the hour or day. | Reduce the profile for the backfill, raise `request_interval_ms`, or let the next scheduled sync continue from the checkpoint. |
| `sink '…' does not support write policy ReplacePartition` | The destination cannot rewrite daily partitions. | Use Athena, Athena Iceberg, or SkipprLake as the destination. |

## Next steps

- [GA4 bronze and modeling](/concepts/ga4-bronze-and-modeling) shows how to turn these tables into reports.
- [How sources land](/concepts/source-landing-semantics) explains partition replacement and lookback.
- [Google Search Console](/connectors/inputs/google_search_console) adds organic search queries to the same warehouse.
