---
title: X Ads
description: Land daily X (Twitter) Ads reports in your warehouse.
---

# X Ads

Reads daily Ads reports from the X Ads API. Provide the ads account id and either a bearer token or OAuth 1.0a user credentials. Pair with Athena, Athena Iceberg, or SkipprLake.

## Before you begin

1. Create an app in the X developer portal with Ads API access.
2. Copy `account_id` / `ad_account_id` from Ads Manager.
3. Export a bearer token **or** the four OAuth 1.0a values.

```bash
export X_ADS_BEARER_TOKEN="..."
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSourceXAds, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "x_ads",
    DataSourceXAds(
        account_id="18ce54...",
        start_date="2024-01-01",
        bearer_token=EnvRef("X_ADS_BEARER_TOKEN"),
        stream_profile="full",
    ),
)
cfg.pipeline("x", Pipeline(data_source=src, data_sink=cfg.get_data_sink("lake")))
cfg.save()
```

```bash [CLI]
skipprd connect data-source x-ads \
  --pipeline x \
  --name x_ads \
  --account-id 18ce54... \
  --start-date 2024-01-01 \
  --bearer-token '${X_ADS_BEARER_TOKEN}' \
  --stream-profile full
```

```yaml [YAML]
data_sources:
  x_ads:
    XAds:
      account_id: 18ce54...
      start_date: "2024-01-01"
      stream_profile: full
      bearer_token: ${X_ADS_BEARER_TOKEN}
```

:::

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `account_id` | string | Required | Ads account id |
| `start_date` | string | Required | First report date |
| `ad_account_id` | string | Not set | Alias some accounts show in the UI |
| `bearer_token` | secret | Not set | App bearer token |
| `access_token` | secret | Not set | User access token |
| `oauth_consumer_key` | string | Not set | OAuth 1.0a consumer key |
| `oauth_consumer_secret` | secret | Not set | Consumer secret |
| `oauth_token` | secret | Not set | User token |
| `oauth_token_secret` | secret | Not set | User token secret |
| `end_date` | string | Not set | Last report date |
| `lookback_days` | integer | Not set | Days to re-fetch |
| `stream_profile` | string | Not set | `minimal`, `standard`, or `full` |
| `processing_lag_days` | integer | Not set | Skip immature days |
| `streams` | list | Profile set | Exact stream list |

## What gets synced

Daily campaign reports in the selected profile. Each run rewrites recent days.

## Troubleshooting

| Symptom | Fix |
|---|---|
| 401 | Check bearer vs OAuth 1.0a — do not mix half-configured sets |
| 403 | Confirm the app has Ads API access |
| Empty reports | Check `account_id` and the date window |

## Next steps

- [Meta Ads](/connectors/inputs/meta_ads)
- [How sources land](/concepts/source-landing-semantics)
