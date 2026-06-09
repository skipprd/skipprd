# Azure Synapse Warehouse

Query and model data in Azure Synapse Analytics.

Ingest may use the [Synapse data sink](../outputs/synapse.md).

## Configuration

```yaml
warehouses:
  primary:
    kind: synapse
    connection_string: ${SYNAPSE_CONNECTION_STRING}
    schema: dbo
```

| Field | Description |
| --- | --- |
| `kind` | `synapse` |
| `connection_string` | ADO.NET-style Synapse connection string |
| `schema` | Default schema (default `dbo`) |

Store the connection string in an environment variable or secret manager reference (`${SYNAPSE_CONNECTION_STRING}`).

## Related

- [Synapse ingest](../outputs/synapse.md)
- [Warehouses overview](../../configuration/warehouses.md)
