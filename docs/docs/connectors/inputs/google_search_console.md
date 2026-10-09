---
title: Google Search Console
description: Sync daily Google Search Console clicks, impressions, CTR, and position by query, page, device, and country, plus sitemap status.
---

# Google Search Console

The Google Search Console source reads daily Search Analytics data (clicks, impressions, click-through rate, and average position) by query, page, device, country, and search appearance, plus a daily snapshot of your sitemaps. Use it to keep more than Search Console's 16 months of organic search history in your own warehouse and join it with analytics, ads, and revenue. You can optionally record URL Inspection results for a short list of important pages.

## Before you begin

You need a verified Search Console property, credentials that can read it, and a destination that can rewrite daily partitions.

1. **Find the property URL.** In [Search Console](https://search.google.com/search-console), open the property picker. A URL-prefix property looks like `https://example.com/`. A domain property looks like `sc-domain:example.com`. Use exactly that form for `site_url`. Skipprd adds a trailing slash to URL-prefix properties if you leave it out.
2. **Enable the API.** In the [Google Cloud Console](https://console.cloud.google.com/), select or create a project, open **APIs & Services → Library**, and enable the **Google Search Console API**.
3. **Choose credentials.** Skipprd checks them in this order and uses the first one it finds: `access_token`, then OAuth refresh (all four `oauth_*` fields), then `service_account_json_path`, then the `GOOGLE_APPLICATION_CREDENTIALS` environment variable. Skipprd requests the scope `https://www.googleapis.com/auth/webmasters.readonly`.
   - **Service account (recommended for schedules).** In **IAM & Admin → Service Accounts**, create an account and download a JSON key. Then, in Search Console, open **Settings → Users and permissions → Add user**, paste the service account's `client_email`, and give it **Restricted** or **Full** permission.
   - **OAuth refresh.** Create an OAuth client under **APIs & Services → Credentials**, then use the [OAuth 2.0 Playground](https://developers.google.com/oauthplayground/) with **Use your own OAuth credentials** to authorize `https://www.googleapis.com/auth/webmasters.readonly` as a user on the property and copy the refresh token. The [Google Analytics](/connectors/inputs/google_analytics) page walks through the same flow step by step.
4. **Set up a destination.** Every stream is rewritten one day at a time, so the destination must support partition replacement: [Athena](/connectors/outputs/athena), [Athena Iceberg](/connectors/outputs/athenaiceberg), or [SkipprLake](/connectors/outputs/skipprlake). The examples below assume a data sink named `lake` already exists in `skippr.yml`.

Before each sync, Skipprd lists the properties your credentials can see and stops with a permission error if `site_url` is not among them.

## Configure

This example uses a service account key:

```bash
export GSC_SERVICE_ACCOUNT_JSON="/secure/path/skippr-gsc-key.json"
```

::: code-group

```python [Python]
from skippr import Config, DataSourceGoogleSearchConsole, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "gsc",
    DataSourceGoogleSearchConsole(
        site_url="sc-domain:example.com",
        start_date="2026-01-01",
        service_account_json_path="${GSC_SERVICE_ACCOUNT_JSON}",
    ),
)
cfg.pipeline("gsc", Pipeline(data_source=src, data_sink=cfg.get_data_sink("lake")))
cfg.save()
```

```bash [CLI]
skipprd connect data-source google-search-console \
  --pipeline gsc \
  --name gsc \
  --site-url sc-domain:example.com \
  --start-date 2026-01-01 \
  --service-account-json-path '${GSC_SERVICE_ACCOUNT_JSON}'
```

```yaml [YAML]
pipelines:
  gsc:
    data_source: data_sources.gsc
    data_sink: data_sinks.lake

data_sources:
  gsc:
    GoogleSearchConsole:
      site_url: "sc-domain:example.com"
      start_date: "2026-01-01"
      service_account_json_path: ${GSC_SERVICE_ACCOUNT_JSON}
```

:::

For OAuth refresh, replace `service_account_json_path` with `oauth_token_url: https://oauth2.googleapis.com/token`, `oauth_client_id`, `oauth_client_secret`, and `oauth_refresh_token`.

`start_date` must fall within the last 16 months (Search Console's history limit), or Skipprd rejects the configuration. Once the pipeline has synced, you can move `start_date` forward at any time; progress is tracked separately.

With the CLI, wire the `lake` destination into the same pipeline with `skipprd connect data-sink … --pipeline gsc` if it is not there already.

### Track URL Inspection results (optional)

To record Google's index status for specific pages once per sync, add:

```yaml
      url_inspection_enabled: true
      url_list:
        - https://example.com/
        - https://example.com/pricing
```

Skipprd inspects at most the first 20 URLs in `url_list` per sync. Google also applies a daily URL Inspection quota per property.

### Check it worked

```bash
skipprd discover --pipeline gsc
skipprd sync --pipeline gsc --once
```

Then query `google_search_console_query_daily` in your destination. You should see clicks and impressions per query for each day from `start_date` to three days ago.

## Options

| Key | Type | Default | Description |
|---|---|---|---|
| `site_url` | string | Required | Property to read: `https://example.com/` (URL prefix) or `sc-domain:example.com` (domain). |
| `start_date` | string | Required | First day to read, `YYYY-MM-DD`. Must be within the last 16 months. |
| `service_account_json_path` | string | — | Path to a service account JSON key. |
| `oauth_token_url` | string | — | Token endpoint for refresh. Use `https://oauth2.googleapis.com/token`. |
| `oauth_client_id` | string | — | OAuth client ID. |
| `oauth_client_secret` | secret | — | OAuth client secret. |
| `oauth_refresh_token` | secret | — | Refresh token with the `webmasters.readonly` scope. |
| `access_token` | secret | — | Short-lived bearer token. Takes priority over every other method. Use for quick tests. |
| `end_date` | string | — | Last day to read, `YYYY-MM-DD`. Without it, Skipprd reads through yesterday, limited by `processing_lag_days`. |
| `lookback_days` | integer | `3` | Days before the last completed day to fetch again on every sync. |
| `processing_lag_days` | integer | `3` | Never read days more recent than this many days ago. Search Console usually finalizes data after 2–3 days. |
| `data_state` | string | `final` | `final` returns only finalized data. `all` includes fresher, still-changing data. |
| `search_type` | string | `web` | Search Console result type: `web`, `image`, `video`, `news`, `discover`, or `googleNews`. One type per source. |
| `stream_profile` | string | `full` | Which streams to sync: `minimal`, `standard`, or `full`. |
| `streams` | list of strings | — | Exact stream names to sync. Overrides `stream_profile`. |
| `window_in_days` | integer | `1` | Days per Search Analytics request, from 1 to 364. |
| `row_limit` | integer | `25000` | Rows per page, from 1 to 25,000. Skipprd keeps paging until a page comes back short. |
| `request_interval_ms` | integer | `300` | Pause after each successful request. |
| `max_api_retries` | integer | `12` | Attempts per request on rate-limit (429) and server (5xx) responses. |
| `url_inspection_enabled` | boolean | `false` | Record URL Inspection results for `url_list`. |
| `url_list` | list of strings | — | Pages to inspect when `url_inspection_enabled` is true. The first 20 are used. |

Secret fields only accept `${NAME}` environment references (`EnvRef("NAME")` in Python).

## What gets synced

Each stream lands as its own table named after the stream, with the dot replaced by an underscore (for example `google_search_console_page_daily`). Search Analytics rows have `site_url`, `search_type`, `date`, the stream's dimensions, and `clicks`, `impressions`, `ctr`, and `position`.

| Stream | One row per | Profiles |
|---|---|---|
| `google_search_console.site_daily` | day | minimal, standard, full |
| `google_search_console.query_daily` | `query` per day | standard, full |
| `google_search_console.page_daily` | `page` per day | standard, full |
| `google_search_console.device_daily` | `device` per day | standard, full |
| `google_search_console.country_daily` | `country` per day | standard, full |
| `google_search_console.page_query_daily` | `page` and `query` per day | full |
| `google_search_console.search_appearance_daily` | `searchAppearance` per day | full |
| `google_search_console.sitemap_daily` | sitemap `path` per sync day | full |
| `google_search_console.site_run_daily` | sync day: rows read and API errors for the run | full |
| `google_search_console.url_inspection_daily` | inspected URL per sync day | only with `url_inspection_enabled` |

`sitemap_daily` records each sitemap's type, last submitted and downloaded times, pending flag, and total warnings and errors. `url_inspection_daily` stores Google's full inspection result for each URL in `inspection_result`. Both are snapshots dated with the last day of the sync window, so you build a history by syncing regularly.

**Totals differ by grain.** Google drops anonymized queries and applies row limits, so `query_daily` and `page_query_daily` add up to less than `site_daily`. Use `site_daily` for totals.

**Optional stream.** If Google rejects the `searchAppearance` dimension for your property, Skipprd skips `search_appearance_daily` for that run and continues.

**Incremental window.** The first sync reads from `start_date` to three days ago (`processing_lag_days`). Each later sync starts again at the last completed day minus `lookback_days` and rewrites those days in full, so finalized numbers replace earlier ones.

**Discover.** `skipprd discover` reads only `site_daily` for the last 3 days and does not save progress.

**Rate limits.** Skipprd pauses `request_interval_ms` after each successful request and retries 429 and 5xx responses with exponential backoff (honouring `Retry-After`, waiting 30 seconds on a 429 without one, and never more than 2 minutes between attempts) up to `max_api_retries` attempts. `page_query_daily` is the largest stream; on big sites a long backfill makes many paged requests per day.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `Google Search Console requires access_token, OAuth refresh credentials, service_account_json_path, or GOOGLE_APPLICATION_CREDENTIALS` | No credential method is complete. OAuth refresh needs all four fields, including `oauth_token_url`. | Configure one method from [Before you begin](#before-you-begin). |
| `account does not have access to site_url` | The credentials can't see this property, or `site_url` doesn't match it exactly. | Add the service account or user under **Settings → Users and permissions**. Check `https://` versus `sc-domain:` and the `www.` prefix. |
| `GSC permission denied for site_url` (HTTP 403) | Same as above, or the Search Console API is not enabled in the key's Cloud project. | Grant access and enable the API. |
| `start_date … is older than ~16 months` | Search Console keeps 16 months of data. | Set a more recent `start_date`. |
| HTTP 400 on `query_daily` or `page_daily` with a non-web `search_type` | Some dimensions are not available for every search type. | List only the supported streams in `streams`. |
| Today's and yesterday's numbers are missing | `processing_lag_days` holds back the last 3 days until Google finalizes them. | Expected. Set `data_state: all` and lower `processing_lag_days` if you need fresher, provisional data. |
| `url_inspection_daily` table never appears | `url_inspection_enabled` is false, or `url_list` is empty. | Set both. |
| `sink '…' does not support write policy ReplacePartition` | The destination cannot rewrite daily partitions. | Use Athena, Athena Iceberg, or SkipprLake as the destination. |

## Next steps

- [How sources land](/concepts/source-landing-semantics) explains partition replacement and lookback.
- [Bing Webmaster Tools](/connectors/inputs/bing_webmaster_tools) adds Bing organic search to the same warehouse.
- [Google Analytics (GA4)](/connectors/inputs/google_analytics) adds on-site behaviour for the same pages.
