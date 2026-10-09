---
title: SumUp
description: Sync SumUp merchant transactions and catalog snapshots into your warehouse.
---

# SumUp

Reads a SumUp merchant account: transactions and related catalog objects. Use an OAuth app or an access token. Pair with a destination that matches the write policy you set (Athena / Iceberg / SkipprLake for daily replace).

## Before you begin

1. Copy the **merchant code** from SumUp.
2. Create an OAuth app and a refresh token, or a personal access token.

```bash
export SUMUP_CLIENT_ID="..."
export SUMUP_CLIENT_SECRET="..."
export SUMUP_REFRESH_TOKEN="..."
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSourceSumUp, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "sumup",
    DataSourceSumUp(
        merchant_code="MC123",
        start_date="2024-01-01",
        oauth_token_url="https://api.sumup.com/token",
        oauth_client_id="${SUMUP_CLIENT_ID}",
        oauth_client_secret=EnvRef("SUMUP_CLIENT_SECRET"),
        oauth_refresh_token=EnvRef("SUMUP_REFRESH_TOKEN"),
        stream_profile="full",
    ),
)
cfg.pipeline("sumup", Pipeline(data_source=src, data_sink=cfg.get_data_sink("lake")))
cfg.save()
```

```bash [CLI]
skipprd connect data-source sumup \
  --pipeline sumup \
  --name sumup \
  --merchant-code MC123 \
  --start-date 2024-01-01 \
  --oauth-token-url https://api.sumup.com/token \
  --oauth-client-id '${SUMUP_CLIENT_ID}' \
  --oauth-client-secret '${SUMUP_CLIENT_SECRET}' \
  --oauth-refresh-token '${SUMUP_REFRESH_TOKEN}' \
  --stream-profile full
```

```yaml [YAML]
data_sources:
  sumup:
    SumUp:
      merchant_code: MC123
      start_date: "2024-01-01"
      stream_profile: full
      lookback_days: 7
      oauth_token_url: https://api.sumup.com/token
      oauth_client_id: ${SUMUP_CLIENT_ID}
      oauth_client_secret: ${SUMUP_CLIENT_SECRET}
      oauth_refresh_token: ${SUMUP_REFRESH_TOKEN}
```

:::

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `merchant_code` | string | Required | SumUp merchant code |
| `start_date` | string | Required | First date (`YYYY-MM-DD`) |
| `lookback_days` | integer | Not set | Days to re-read |
| `stream_profile` | string | Not set | `minimal`, `standard`, or `full` |
| `streams` | list | Profile set | Exact stream list |
| `min_query_interval_ms` | integer | Not set | Delay between API calls |
| `oauth_token_url` | URL | Not set | Token endpoint |
| `oauth_client_id` | string | Not set | OAuth client id |
| `oauth_client_secret` | secret | Not set | OAuth secret |
| `oauth_refresh_token` | secret | Not set | Refresh token |
| `access_token` | secret | Not set | Static token (skips refresh) |
| `privacy` | object | Not set | Field redaction (see Stripe's privacy settings for the shape) |

## What gets synced

Merchant transactions and catalog objects in the selected profile. Each run re-reads `lookback_days`.

## Troubleshooting

| Symptom | Fix |
|---|---|
| 401 | Rotate the refresh token or access token |
| Empty tables | Check `merchant_code` and `start_date` |

## Next steps

- [Stripe](/connectors/inputs/stripe)
- [How sources land](/concepts/source-landing-semantics)
