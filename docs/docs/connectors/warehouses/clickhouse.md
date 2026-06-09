# ClickHouse Warehouse

Query and model data in ClickHouse.

Ingest may use the [ClickHouse data sink](../outputs/clickhouse.md).

## Configuration

```yaml
warehouses:
  primary:
    kind: clickhouse
    url: https://clickhouse.example.com:8443
    database: default
    user: ${CLICKHOUSE_USER}
    password: ${CLICKHOUSE_PASSWORD}
```

| Field | Description |
| --- | --- |
| `kind` | `clickhouse` |
| `url` | ClickHouse HTTP(S) endpoint |
| `database` | Default database (default `default`) |
| `user` | Username |
| `password` | Password |

## Related

- [ClickHouse ingest](../outputs/clickhouse.md)
- [Warehouses overview](../../configuration/warehouses.md)
