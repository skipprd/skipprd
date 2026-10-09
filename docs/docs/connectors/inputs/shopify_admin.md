---
title: Shopify Admin
description: Sync Shopify orders, products, and store metadata into your warehouse with a custom-app Admin API token.
---

# Shopify Admin

Reads the Shopify Admin API: orders, products, and store metadata. Use a custom app token with **read** access to the streams you select.

## Before you begin

1. In Shopify admin, create a **custom app** and install it on the shop.
2. Copy the Admin API access token. Grant read scopes for orders and products (and any other stream you enable).
3. Note the shop hostname (`my-store.myshopify.com`).
4. Pick a destination. Daily order windows work best with [Athena](/connectors/outputs/athena), [Athena Iceberg](/connectors/outputs/athenaiceberg), or [SkipprLake](/connectors/outputs/skipprlake).

```bash
export SHOPIFY_ACCESS_TOKEN="shpat_..."
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSourceShopifyAdmin, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "shopify",
    DataSourceShopifyAdmin(
        shop_domain="my-store.myshopify.com",
        start_date="2024-01-01",
        stream_profile="full",
        oauth_access_token=EnvRef("SHOPIFY_ACCESS_TOKEN"),
    ),
)
cfg.pipeline("commerce", Pipeline(data_source=src, data_sink=cfg.get_data_sink("lake")))
cfg.save()
```

```bash [CLI]
skipprd connect data-source shopify-admin \
  --pipeline commerce \
  --name shopify \
  --shop-domain my-store.myshopify.com \
  --start-date 2024-01-01 \
  --stream-profile full \
  --oauth-access-token '${SHOPIFY_ACCESS_TOKEN}'
```

```yaml [YAML]
data_sources:
  shopify:
    ShopifyAdmin:
      shop_domain: my-store.myshopify.com
      start_date: "2024-01-01"
      stream_profile: full
      lookback_days: 7
      oauth_access_token: ${SHOPIFY_ACCESS_TOKEN}
```

:::

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `shop_domain` | string | Required | Shop hostname |
| `start_date` | string | Required | First order date (`YYYY-MM-DD`) |
| `api_version` | string | Not set | Admin API version override |
| `lookback_days` | integer | `7` | Order window to re-read |
| `stream_profile` | string | `full` | `minimal`, `standard`, or `full` |
| `streams` | list | Profile set | Exact stream list |
| `min_query_interval_ms` | integer | `500` | Minimum delay between GraphQL calls |
| `max_queries_per_run` | integer | `200` | Query budget per sync |
| `use_bulk_operations` | bool | `false` | Use bulk operations for large backfills |
| `oauth_client_id` | string | Not set | Custom app client ID |
| `oauth_client_secret` | secret | Not set | Custom app secret |
| `oauth_access_token` | secret | Required | Admin API token as `${ENV}` |

## What gets synced

Orders and catalog objects in the selected profile. Each run re-reads `lookback_days`. Deleted Shopify objects stop appearing in new snapshots.

## Troubleshooting

| Symptom | Fix |
|---|---|
| 401 | Recreate the custom-app token and export it |
| 403 | Add the missing Admin API read scopes |
| Rate limited | Raise `min_query_interval_ms` or enable `use_bulk_operations` |

## Next steps

- [Stripe](/connectors/inputs/stripe)
- [How sources land](/concepts/source-landing-semantics)
