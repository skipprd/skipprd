# Shopify Admin plugin fixtures

Set `SKIPPR_SHOPIFY_FIXTURE_DIR` to this directory for offline tests:

```bash
SKIPPR_SHOPIFY_FIXTURE_DIR=plugins/data_source/shopify_admin/tests/fixtures \
  cargo test -p skippr-plugin-data-source-shopify-admin happy_sync
```

| File | GraphQL operation |
|------|-------------------|
| `shop.json` | `shop` |
| `products.json` | `products` |
| `orders.json` | `orders` |

`orders.json` includes nested customer PII to verify the plugin never maps those fields into bronze rows.
