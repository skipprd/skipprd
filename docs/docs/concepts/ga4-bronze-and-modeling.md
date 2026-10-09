---
title: GA4 bronze and modeling
description: Land Google Analytics 4 as daily bronze tables in Skipprd, then build warehouse rollups in SQL or dbt — not in the source.
---

# GA4 bronze and modeling

The [Google Analytics (GA4) source](/connectors/inputs/google_analytics) lands **daily fact tables** from the Data API. Each table is one stable set of dimensions. Rollups, pivots, and channel summaries belong in the warehouse (SQL or dbt), not in Skipprd.

This is not the GA4 BigQuery export. You get daily grains Skipprd can rewrite when Google revises a day, not event-level hits.

## What to build on each bronze table

| Bronze table | Warehouse work you typically do |
|---|---|
| `google_analytics.audience_daily` | Site-wide DAU, sessions, engagement KPIs |
| `google_analytics.audience_retention_daily` | WAU / 28-day active users (`date` grain already) |
| `google_analytics.traffic_acquisition_daily` | Channel summary, source/medium rollups |
| `google_analytics.traffic_campaign_daily` | Paid campaign performance |
| `google_analytics.user_acquisition_daily` | New-user acquisition by channel |
| `google_analytics.events_daily` | Weekly event trends (`date_trunc` in SQL) |
| `google_analytics.content_pages_daily` | Top pages and landing pages |
| `google_analytics.geo_daily` | Country / region / city dashboards |
| `google_analytics.demographics_*_daily` | Age, gender, interest, language |
| `google_analytics.ecommerce_items_daily` | Product revenue (needs ecommerce in GA4) |
| `google_analytics.publisher_ads_daily` | Ad-unit performance (needs linked Ads) |

Join on `date` plus the dimension columns the table already has. Do not ask the source for a different grain.

## Settings that affect accuracy

These are easy to mix up. They do different jobs.

| Setting | Role |
|---|---|
| `replace_partition` on `date` | Rewrite that calendar day's partition when metrics change |
| `lookback_days` | Re-pull recent days Google may revise |
| `processing_lag_days` | Skip the newest days while GA4 is still incomplete |
| `window_in_days` | Days per API request. Leave at `1` to limit sampling |

Run a scheduled `skipprd sync`. Raise `lookback_days` when attribution windows are long. Do not raise `window_in_days` to speed up a backfill — that increases sampling.

See [How sources land](/concepts/source-landing-semantics) for why `replace_partition` is required here.

## Next steps

- [Google Analytics (GA4)](/connectors/inputs/google_analytics)
- [How sources land](/concepts/source-landing-semantics)
- [Athena](/connectors/outputs/athena)
