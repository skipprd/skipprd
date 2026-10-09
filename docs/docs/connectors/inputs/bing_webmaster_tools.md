---
title: Bing Webmaster Tools
description: Sync daily Bing search traffic, query stats, page stats, and crawl stats from Bing Webmaster Tools into your warehouse.
---

# Bing Webmaster Tools

The Bing Webmaster Tools source reads search performance and crawl reports from the Bing Webmaster API: site-wide clicks and impressions per day, per-query stats, per-page stats, and crawl activity. Use it to keep Bing organic search history beyond what Bing retains and report on it next to Google Search Console and your analytics data.

## Before you begin

You need a site verified in Bing Webmaster Tools, an API key (or OAuth credentials), and a destination that can rewrite daily partitions.

1. **Verify the site.** In [Bing Webmaster Tools](https://www.bing.com/webmasters), add and verify your site. Note the site URL exactly as it appears in the site picker, for example `https://example.com/`. Skipprd adds a trailing slash if you leave it out.
2. **Create an API key.** Open **Settings → API access**, accept the terms, and click **Generate API key**. Copy the key. One key covers every site your account can manage.
3. **Or use OAuth.** If you have a Bing Webmaster OAuth app, you can provide `oauth_client_id`, `oauth_client_secret`, and `oauth_refresh_token` instead. `oauth_token_url` defaults to `https://www.bing.com/webmasters/oauth/token`. When both an API key and a token are configured, Skipprd uses the API key.
4. **Set up a destination.** The dated streams are rewritten one day at a time, so the destination must support partition replacement: [Athena](/connectors/outputs/athena), [Athena Iceberg](/connectors/outputs/athenaiceberg), or [SkipprLake](/connectors/outputs/skipprlake). The examples below assume a data sink named `lake` already exists in `skippr.yml`.

```bash
export BING_WEBMASTER_TOOLS_API_KEY="your-api-key"
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSourceBingWebmasterTools, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "bing",
    DataSourceBingWebmasterTools(
        site_url="https://example.com/",
        start_date="2026-09-01",
        api_key=EnvRef("BING_WEBMASTER_TOOLS_API_KEY"),
    ),
)
cfg.pipeline("bing", Pipeline(data_source=src, data_sink=cfg.get_data_sink("lake")))
cfg.save()
```

```bash [CLI]
skipprd connect data-source bing-webmaster-tools \
  --pipeline bing \
  --name bing \
  --site-url https://example.com/ \
  --start-date 2026-09-01 \
  --api-key '${BING_WEBMASTER_TOOLS_API_KEY}'
```

```yaml [YAML]
pipelines:
  bing:
    data_source: data_sources.bing
    data_sink: data_sinks.lake

data_sources:
  bing:
    BingWebmasterTools:
      site_url: "https://example.com/"
      start_date: "2026-09-01"
      api_key: ${BING_WEBMASTER_TOOLS_API_KEY}
```

:::

::: warning Keep `start_date` recent
Bing only serves about three months of history, and Skipprd rejects a `start_date` more than 90 days in the past. Because that limit moves with today's date, a fixed `start_date` eventually falls outside it. After the first sync, move `start_date` forward to a recent date. Progress is tracked separately, so this does not cause a resync.
:::

With the CLI, wire the `lake` destination into the same pipeline with `skipprd connect data-sink … --pipeline bing` if it is not there already.

### Check it worked

```bash
skipprd discover --pipeline bing
skipprd sync --pipeline bing --once
```

Then query `bing_webmaster_tools_site_daily` in your destination. You should see one row per day with Bing clicks and impressions.

## Options

| Key | Type | Default | Description |
|---|---|---|---|
| `site_url` | string | Required | Verified site URL, for example `https://example.com/`. |
| `start_date` | string | Required | First day to read, `YYYY-MM-DD`. Must be within the last 90 days. |
| `api_key` | secret | — | API key from **Settings → API access**. Used before any token. |
| `access_token` | secret | — | OAuth bearer token. Used when no `api_key` is set. |
| `oauth_token_url` | string | `https://www.bing.com/webmasters/oauth/token` | Token endpoint for OAuth refresh. |
| `oauth_client_id` | string | — | OAuth client ID. |
| `oauth_client_secret` | secret | — | OAuth client secret. |
| `oauth_refresh_token` | secret | — | OAuth refresh token. |
| `end_date` | string | — | Last day to keep, `YYYY-MM-DD`. Without it, Skipprd keeps data through yesterday, limited by `processing_lag_days`. |
| `lookback_days` | integer | `3` | Days before the last completed day to rewrite on every sync. |
| `processing_lag_days` | integer | `3` | Never keep days more recent than this many days ago, while Bing finalizes them. |
| `stream_profile` | string | `full` | Which streams to sync: `minimal`, `standard`, or `full`. |
| `streams` | list of strings | — | Exact stream names to sync. Overrides `stream_profile`. |
| `window_in_days` | integer | `1` | Accepted for consistency with other search sources (1 to 364). Bing returns its whole history in one response, so this setting does not change requests. |
| `request_interval_ms` | integer | `300` | Pause after each dated report request. |
| `max_api_retries` | integer | `12` | Attempts per request on rate-limit (429) and server (5xx) responses. |

Secret fields only accept `${NAME}` environment references (`EnvRef("NAME")` in Python).

## What gets synced

Each stream lands as its own table named after the stream, with the dot replaced by an underscore. Every row has `site_url` and `date`, plus the fields Bing returns for that report (for example `Clicks` and `Impressions`), with Bing's original field names.

| Stream | Bing report | One row per | Profiles |
|---|---|---|---|
| `bing_webmaster_tools.site_daily` | Rank and traffic stats | day | minimal, standard, full |
| `bing_webmaster_tools.query_daily` | Query stats | `Query` per Bing date | standard, full |
| `bing_webmaster_tools.crawl_daily` | Crawl stats | day | standard, full |
| `bing_webmaster_tools.page_daily` | Page stats | `Url` per sync day | full |
| `bing_webmaster_tools.site_run_daily` | — | sync day: rows read for the run | full |

**How dates work.** Each report call returns Bing's full retained history for the site. Skipprd keeps only the rows whose date falls in the sync window and writes them by day, using Bing's `Date` field. `query_daily` has rows only on the dates Bing returns query stats for. `page_daily` has no date from Bing, so each sync stores the current page stats under the last day of the sync window; sync regularly to build a history.

Query rows include `Query`, `Clicks`, `Impressions`, `AvgClickPosition`, and `AvgImpressionPosition`. Crawl rows include `CrawledPages`, HTTP status counts such as `Code2xx` and `Code4xx`, and `BlockedByRobotsTxt`.

**Incremental window.** The first sync covers `start_date` to three days ago (`processing_lag_days`). Each later sync starts again at the last completed day minus `lookback_days` and rewrites those days in full. Days with no rows still count as completed.

**Discover.** `skipprd discover` reads only `site_daily` for the last 3 days and does not save progress.

**Rate limits.** Skipprd makes one call per report per sync, however long the window, pauses `request_interval_ms` after each dated report, and retries 429 and 5xx responses with exponential backoff (honouring `Retry-After`, waiting 30 seconds on a 429 without one, and never more than 2 minutes between attempts) up to `max_api_retries` attempts.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `Bing Webmaster Tools requires api_key, access_token, or OAuth refresh credentials` | No credential is set, or its environment variable is empty. | Export `BING_WEBMASTER_TOOLS_API_KEY` in the shell or service that runs Skipprd. |
| `start_date … is older than ~3 months of Bing Webmaster history` | `start_date` is more than 90 days ago. This also happens to a working pipeline as time passes. | Move `start_date` to a date within the last 90 days. Progress is kept. |
| `Bing Webmaster permission denied for site_url` (HTTP 403) | The API key's account does not manage this site, or `site_url` differs from the verified URL. | Use the site URL exactly as shown in Bing Webmaster Tools, including `https://` and `www.` if present. |
| `window_in_days must be between 1 and 364` | `window_in_days` is out of range. | Remove it or set a value from 1 to 364. |
| Tables are empty for recent days | `processing_lag_days` holds back the last 3 days. | Expected. Lower it if you accept provisional numbers. |
| `query_daily` has gaps between dates | Bing does not return query stats for every date. | Expected. Aggregate by week or month in SQL. |
| `sink '…' does not support write policy ReplacePartition` | The destination cannot rewrite daily partitions. | Use Athena, Athena Iceberg, or SkipprLake as the destination. |

## Next steps

- [Google Search Console](/connectors/inputs/google_search_console) adds Google organic search for the same site.
- [How sources land](/concepts/source-landing-semantics) explains partition replacement and lookback.
- [skipprd sync](/cli/sync) covers scheduling, which keeps `page_daily` snapshots continuous.
