---
title: Xero Accounting
description: Sync Xero invoices, payments, bank transactions, contacts, and the chart of accounts.
---

# Xero Accounting

Reads one Xero organisation: invoices, payments, bank transactions, contacts, and accounts. Use a Xero OAuth 2.0 app with a refresh token. The **tenant id** is the organisation Skipprd should read.

## Before you begin

1. Create an app at [Xero developer](https://developer.xero.com/) and complete OAuth.
2. Copy the **tenant id** (organisation) from the Xero connection list.
3. Export client id, secret, and refresh token.

```bash
export XERO_CLIENT_ID="..."
export XERO_CLIENT_SECRET="..."
export XERO_REFRESH_TOKEN="..."
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSourceXeroAccounting, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "xero",
    DataSourceXeroAccounting(
        tenant_id="00000000-0000-0000-0000-000000000000",
        start_date="2024-01-01",
        oauth_client_id="${XERO_CLIENT_ID}",
        oauth_client_secret=EnvRef("XERO_CLIENT_SECRET"),
        oauth_refresh_token=EnvRef("XERO_REFRESH_TOKEN"),
        stream_profile="full",
    ),
)
cfg.pipeline("accounting", Pipeline(data_source=src, data_sink=cfg.get_data_sink("lake")))
cfg.save()
```

```bash [CLI]
skipprd connect data-source xero-accounting \
  --pipeline accounting \
  --name xero \
  --tenant-id 00000000-0000-0000-0000-000000000000 \
  --start-date 2024-01-01 \
  --oauth-client-id '${XERO_CLIENT_ID}' \
  --oauth-client-secret '${XERO_CLIENT_SECRET}' \
  --oauth-refresh-token '${XERO_REFRESH_TOKEN}' \
  --stream-profile full
```

```yaml [YAML]
data_sources:
  xero:
    XeroAccounting:
      tenant_id: "00000000-0000-0000-0000-000000000000"
      start_date: "2024-01-01"
      stream_profile: full
      lookback_days: 90
      oauth_client_id: ${XERO_CLIENT_ID}
      oauth_client_secret: ${XERO_CLIENT_SECRET}
      oauth_refresh_token: ${XERO_REFRESH_TOKEN}
```

:::

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `tenant_id` | string | Required | Xero organisation tenant id |
| `start_date` | string | Required | First date (`YYYY-MM-DD`) |
| `lookback_days` | integer | `90` | Modified-since window |
| `page_size` | integer | `100` | API page size (1–100) |
| `stream_profile` | string | `full` | `minimal`, `standard`, or `full` |
| `streams` | list | Profile set | Exact stream list |
| `min_query_interval_ms` | integer | `350` | Delay between API calls |
| `oauth_token_url` | URL | Xero default | Token endpoint |
| `oauth_client_id` | string | Not set | App client id |
| `oauth_client_secret` | secret | Not set | App secret |
| `oauth_refresh_token` | secret | Not set | Refresh token |
| `access_token` | secret | Not set | Static bearer (skips refresh) |
| `privacy` | object | Not set | Field redaction |

## What gets synced

Accounting entities in the selected profile. Each run re-reads rows modified in `lookback_days`.

## Troubleshooting

| Symptom | Fix |
|---|---|
| 401 | Re-consent the Xero app; refresh tokens rotate |
| 403 | Confirm the tenant is connected to this app |
| Wrong organisation | `tenant_id` is per organisation, not the user |

## Next steps

- [Stripe](/connectors/inputs/stripe)
- [How sources land](/concepts/source-landing-semantics)
