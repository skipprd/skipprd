# MotherDuck Warehouse

Query and model data in MotherDuck.

Ingest may use the [MotherDuck data sink](../outputs/motherduck.md).

## Configuration

```yaml
warehouses:
  primary:
    kind: motherduck
    motherduck_token: ${MOTHERDUCK_TOKEN}
    database: my_db
    schema: main
```

| Field | Description |
| --- | --- |
| `kind` | `motherduck` |
| `motherduck_token` | MotherDuck access token |
| `database` | Database name |
| `schema` | Schema name (default `main`) |

## Related

- [MotherDuck ingest](../outputs/motherduck.md)
- [Warehouses overview](../../configuration/warehouses.md)
