---
title: Meta Instagram Ads
description: Land daily Instagram placement insights from a Meta ad account.
---

# Meta Instagram Ads

Same Marketing API as [Meta Ads](/connectors/inputs/meta_ads), filtered to Instagram placements. Use it when Instagram spend should land in its own tables.

## Before you begin

- An `act_…` ad account and a long-lived token with `ads_read`.
- A destination that can rewrite a day (Athena, Athena Iceberg, or SkipprLake).

```bash
export META_ADS_ACCESS_TOKEN="..."
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSourceMetaInstagramAds, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "ig_ads",
    DataSourceMetaInstagramAds(
        ad_account_id="act_123456789",
        start_date="2024-01-01",
        access_token=EnvRef("META_ADS_ACCESS_TOKEN"),
        instagram_filter=True,
        stream_profile="full",
    ),
)
cfg.pipeline("ig", Pipeline(data_source=src, data_sink=cfg.get_data_sink("lake")))
cfg.save()
```

```bash [CLI]
skipprd connect data-source meta-instagram-ads \
  --pipeline ig \
  --name ig_ads \
  --ad-account-id act_123456789 \
  --start-date 2024-01-01 \
  --access-token '${META_ADS_ACCESS_TOKEN}' \
  --instagram-filter \
  --stream-profile full
```

```yaml [YAML]
data_sources:
  ig_ads:
    MetaInstagramAds:
      ad_account_id: act_123456789
      start_date: "2024-01-01"
      stream_profile: full
      instagram_filter: true
      access_token: ${META_ADS_ACCESS_TOKEN}
```

:::

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `ad_account_id` | string | Required | `act_…` account id |
| `start_date` | string | Required | First report date |
| `access_token` | secret | Not set | Long-lived token |
| `oauth_token_url` | URL | Not set | Token URL when you refresh |
| `oauth_client_id` | string | Not set | OAuth client id |
| `oauth_client_secret` | secret | Not set | OAuth secret |
| `oauth_refresh_token` | secret | Not set | Refresh token |
| `api_version` | string | Not set | Graph API version |
| `end_date` | string | Yesterday minus lag | Last report date |
| `lookback_days` | integer | Not set | Days to re-fetch |
| `stream_profile` | string | Not set | `minimal`, `standard`, or `full` |
| `processing_lag_days` | integer | Not set | Skip immature days |
| `instagram_filter` | bool | `true` here | Keep Instagram placements |
| `streams` | list | Profile set | Exact stream list |

## What gets synced

Daily Instagram placement insights. Each run rewrites recent days.

## Troubleshooting

| Symptom | Fix |
|---|---|
| Empty tables | Confirm the account has Instagram placements in the window |
| 190 / invalid token | Same token fix as [Meta Ads](/connectors/inputs/meta_ads) |

## Next steps

- [Meta Ads](/connectors/inputs/meta_ads)
- [How sources land](/concepts/source-landing-semantics)
