# Shopify Admin Input

Orders, products, and store metadata from the Shopify Admin GraphQL API.

## Configuration

```yaml
data_sources:
  shopify:
    ShopifyAdmin:
      shop_domain: my-store.myshopify.com
      start_date: "2024-01-01"
      stream_profile: full
      lookback_days: 7
      oauth_access_token: ${SHOPIFY_ACCESS_TOKEN}
```

| Field | Default | Description |
| --- | --- | --- |
| `shop_domain` | *(required)* | Shop hostname (with or without `https://`) |
| `start_date` | *(required)* | First order sync date (`YYYY-MM-DD`) |
| `lookback_days` | `7` | Order window lookback |
| `stream_profile` | `full` | `minimal`, `standard`, or `full` |
| `streams` | profile set | Explicit namespace list |
| `api_version` | | Admin API version override |
| `min_query_interval_ms` | `500` | Minimum delay between GraphQL calls |
| `max_queries_per_run` | `200` | GraphQL query budget per sync |
| `use_bulk_operations` | `false` | Use bulk operation API for large backfills |
| `oauth_client_id` | | Custom app client ID |
| `oauth_client_secret` | | Custom app client secret |
| `oauth_access_token` | | Admin API access token |

## Pipeline wiring

```yaml
pipelines:
  commerce:
    data_source: data_sources.shopify
    data_sink: data_sinks.landing
```
