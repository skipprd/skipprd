# MSSQL Warehouse

Query and model data in Microsoft SQL Server.

## Configuration

```yaml
warehouses:
  primary:
    kind: mssql
    database: analytics
    schema: dbo
    max_concurrency: 4
    discovery_cache_ttl_secs: 300
```

| Field | Description |
| --- | --- |
| `kind` | `mssql` |
| `database` | Database name |
| `schema` | Default schema |
| `max_concurrency` | Parallel query cap |
| `discovery_cache_ttl_secs` | Catalog discovery cache TTL |

## Connection secrets

Configure host, port, user, and password through environment variables supported by your deployment (for example `MSSQL_HOST`, `MSSQL_USER`, `MSSQL_PASSWORD`).

## Related

- [MSSQL input](../inputs/mssql.md) — CDC / ingest from SQL Server
- [Warehouses overview](../../configuration/warehouses.md)
