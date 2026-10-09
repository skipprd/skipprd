---
title: AdRoll Ads
description: Land daily AdRoll reports in your warehouse with an access token or OAuth refresh.
---

# AdRoll Ads

Reads daily AdRoll reports for one advertiser. Pair with Athena, Athena Iceberg, or SkipprLake so revised days can be rewritten.

## Before you begin

1. Copy the **advertiser id** from AdRoll.
2. Create an API token or an OAuth app and refresh token.

```bash
export ADROLL_ACCESS_TOKEN="..."
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSourceAdRollAds, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "adroll",
    DataSourceAdRollAds(
        advertiser_id="ADV123",
        start_date="2024-01-01",
        access_token=EnvRef("ADROLL_ACCESS_TOKEN"),
        stream_profile="full",
    ),
)
cfg.pipeline("adroll", Pipeline(data_source=src, data_sink=cfg.get_data_sink("lake")))
cfg.save()
```

```bash [CLI]
skipprd connect data-source adroll-ads \
  --pipeline adroll \
  --name adroll \
  --advertiser-id ADV123 \
  --start-date 2024-01-01 \
  --access-token '${ADROLL_ACCESS_TOKEN}' \
  --stream-profile full
```

```yaml [YAML]
data_sources:
  adroll:
    AdrollAds:
      advertiser_id: ADV123
      start_date: "2024-01-01"
      stream_profile: full
      access_token: ${ADROLL_ACCESS_TOKEN}
```

:::

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `advertiser_id` | string | Required | AdRoll advertiser id |
| `start_date` | string | Required | First report date |
| `access_token` | secret | Not set | API token |
| `personal_access_token` | secret | Not set | Personal token (if your account uses one) |
| `oauth_token_url` | URL | Not set | Token URL when you refresh |
| `oauth_client_id` | string | Not set | OAuth client id |
| `oauth_client_secret` | secret | Not set | OAuth secret |
| `oauth_refresh_token` | secret | Not set | Refresh token |
| `api_base_url` | URL | Not set | API host override |
| `reporting_base_url` | URL | Not set | Reporting host override |
| `end_date` | string | Not set | Last report date |
| `lookback_days` | integer | Not set | Days to re-fetch |
| `stream_profile` | string | Not set | `minimal`, `standard`, or `full` |
| `processing_lag_days` | integer | Not set | Skip immature days |
| `streams` | list | Profile set | Exact stream list |

## What gets synced

Daily advertiser reports in the selected profile. Each run rewrites recent days.

## Troubleshooting

| Symptom | Fix |
|---|---|
| 401 | Rotate the token or refresh credentials |
| Empty reports | Check `advertiser_id` and the date window |

## Next steps

- [Google Ads](/connectors/inputs/google_ads)
- [How sources land](/concepts/source-landing-semantics)
