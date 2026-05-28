# GA4 bronze catalog and warehouse modeling

Skippr’s GA4 source lands **daily fact grains** in bronze via the Data API `runReport`. Each namespace is one stable dimension set; rollups, pivots, and channel summaries belong in the warehouse (dbt, `skippr model`, or SQL).

This is **not** the GA4 BigQuery export (event-level raw). Use a separate future source for event-level hoovering.

## Bronze → warehouse examples

| Bronze namespace | Example warehouse outputs |
| --- | --- |
| `google_analytics.audience_daily` | Site-wide DAU, sessions, engagement KPIs |
| `google_analytics.audience_retention_daily` | WAU / 28-day active users (metrics already on `date` grain) |
| `google_analytics.traffic_acquisition_daily` | Channel summary, source/medium rollups |
| `google_analytics.traffic_campaign_daily` | Paid campaign performance |
| `google_analytics.user_acquisition_daily` | New user acquisition by channel |
| `google_analytics.events_daily` | Weekly event trends (group by week in SQL) |
| `google_analytics.content_pages_daily` | Top pages, landing page analysis |
| `google_analytics.geo_daily` | Country / region / city dashboards |
| `google_analytics.demographics_*_daily` | Age, gender, interest, language breakdowns |
| `google_analytics.ecommerce_items_daily` | Product-level revenue (requires ecommerce enabled) |
| `google_analytics.publisher_ads_daily` | Ad unit performance (requires linked Ads) |

## Accuracy knobs (do not conflate)

| Setting | Role |
| --- | --- |
| `replace_partition` on `date` | Rewrite each calendar day’s partition when metrics change |
| `lookback_days` | Re-pull recent **mature** days Google may revise |
| `processing_lag_days` | Skip syncing the trailing edge while GA4 is still incomplete |
| `window_in_days` | Days per API `dateRanges` chunk; default **1** to limit sampling |

**Recommended operations:** scheduled `skippr sync` with defaults; raise `lookback_days` for long attribution windows — not `window_in_days > 1`.

See [Google Analytics (GA4) input](../connectors/inputs/google_analytics.md) and [Source landing semantics](source-landing-semantics.md).
