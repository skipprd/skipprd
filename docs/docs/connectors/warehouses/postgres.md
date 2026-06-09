# Postgres Warehouse

Query and model data in PostgreSQL.

Ingest may use the [Postgres data sink](../outputs/postgres.md). This page covers `warehouses:` for `skippr query` and `skippr model`.

## Configuration

```yaml
warehouses:
  primary:
    kind: postgres
    database: analytics
    schema: public
```

| Field | Description |
| --- | --- |
| `kind` | `postgres` |
| `database` | Database name |
| `schema` | Schema name (default `public`) |

## Connection secrets

Set via environment variables:

- `POSTGRES_HOST`
- `POSTGRES_PORT` (default `5432`)
- `POSTGRES_USER`
- `POSTGRES_PASSWORD`
- `POSTGRES_SSLMODE`

## Related

- [Postgres ingest](../outputs/postgres.md)
- [Warehouses overview](../../configuration/warehouses.md)
