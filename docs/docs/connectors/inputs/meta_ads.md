---
title: Meta Ads
description: Land daily Meta (Facebook) Ads insights at campaign, ad set, and ad grain.
---

# Meta Ads

Reads daily insights from the Meta Marketing API. Use a long-lived user or system-user token with `ads_read` on the ad account. Pair with Athena, Athena Iceberg, or SkipprLake so revised days can be rewritten.

## Before you begin

1. Copy the ad account id (`act_123456789`) from Meta Events Manager or Ads Manager.
2. Generate a long-lived token (system user recommended for production).
3. Choose `start_date`.

```bash
export META_ADS_ACCESS_TOKEN="..."
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSourceMetaAds, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "meta_ads",
    DataSourceMetaAds(
        ad_account_id="act_123456789",
        start_date="2024-01-01",
        access_token=EnvRef("META_ADS_ACCESS_TOKEN"),
        stream_profile="full",
    ),
)
cfg.pipeline("meta", Pipeline(data_source=src, data_sink=cfg.get_data_sink("lake")))
cfg.save()
```

```bash [CLI]
skipprd connect data-source meta-ads \
  --pipeline meta \
  --name meta_ads \
  --ad-account-id act_123456789 \
  --start-date 2024-01-01 \
  --access-token '${META_ADS_ACCESS_TOKEN}' \
  --stream-profile full
```

```yaml [YAML]
data_sources:
  meta_ads:
    MetaAds:
      ad_account_id: act_123456789
      start_date: "2024-01-01"
      stream_profile: full
      lookback_days: 3
      processing_lag_days: 1
      access_token: ${META_ADS_ACCESS_TOKEN}
```

:::

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `ad_account_id` | string | Required | `act_…` account id |
| `start_date` | string | Required | First report date |
| `access_token` | secret | Not set | Long-lived token as `${ENV}` |
| `oauth_token_url` | URL | Not set | Token URL when you refresh |
| `oauth_client_id` | string | Not set | OAuth client id |
| `oauth_client_secret` | secret | Not set | OAuth secret |
| `oauth_refresh_token` | secret | Not set | Refresh token |
| `api_version` | string | Not set | Graph API version |
| `end_date` | string | Yesterday minus lag | Last report date |
| `lookback_days` | integer | `3` | Mature days to re-fetch |
| `stream_profile` | string | `full` | `minimal`, `standard`, or `full` |
| `processing_lag_days` | integer | `1` | Skip immature trailing days |
| `instagram_filter` | bool | `false` | Restrict to Instagram placements |
| `streams` | list | Profile set | Exact stream list |

## What gets synced

Daily campaign, ad-set, and ad insights in the selected profile. Each run rewrites recent days.

## Troubleshooting

| Symptom | Fix |
|---|---|
| 190 / invalid token | Generate a new long-lived token |
| 200 / permission | Grant `ads_read` on this `ad_account_id` |
| Destination rejects replace | Use Athena, Athena Iceberg, or SkipprLake |

## Next steps

- [Meta Instagram Ads](/connectors/inputs/meta_instagram_ads)
- [How sources land](/concepts/source-landing-semantics)
