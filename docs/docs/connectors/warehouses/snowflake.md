# Snowflake Warehouse

Query and model data in Snowflake.

Ingest may use the [Snowflake data sink](../outputs/snowflake.md). This page covers `warehouses:` for `skippr query` and `skippr model`.

## Configuration

```yaml
warehouses:
  primary:
    kind: snowflake
    account: my-org-my-account
    user: ${SNOWFLAKE_USER}
    private_key_path: ${SNOWFLAKE_PRIVATE_KEY_PATH}
    database: ANALYTICS
    schema: RAW
    warehouse: COMPUTE_WH
    role: TRANSFORMER
```

| Field | Description |
| --- | --- |
| `kind` | `snowflake` |
| `account` | Account identifier (`org-account`) |
| `user` | Login user |
| `password` | Password auth (when not using key pair) |
| `private_key_path` | PKCS8 PEM private key for key-pair auth |
| `database` | Default database |
| `schema` | Default schema |
| `warehouse` | Compute warehouse |
| `role` | Role to assume |
| `stage` | Default stage for bulk loads |
| `staging_uri` | External staging URI override |
| `max_concurrency` | Parallel query cap |
| `discovery_cache_ttl_secs` | Catalog discovery cache TTL |

## Related

- [Snowflake ingest](../outputs/snowflake.md)
- [Warehouses overview](../../configuration/warehouses.md)
