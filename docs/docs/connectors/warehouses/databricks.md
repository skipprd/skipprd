# Databricks Warehouse

Query and model data on Databricks SQL warehouses.

Ingest may use the [Databricks data sink](../outputs/databricks.md).

## Configuration

```yaml
warehouses:
  primary:
    kind: databricks
    workspace_url: https://dbc-abc123.cloud.databricks.com
    token: ${DATABRICKS_TOKEN}
    warehouse_id: abcdef1234567890
    catalog: main
    schema: default
```

| Field | Description |
| --- | --- |
| `kind` | `databricks` |
| `workspace_url` | Databricks workspace URL |
| `token` | Personal access token or OAuth token |
| `warehouse_id` | SQL warehouse ID |
| `catalog` | Unity Catalog name (default `main`) |
| `schema` | Default schema (default `default`) |

## Related

- [Databricks ingest](../outputs/databricks.md)
- [Warehouses overview](../../configuration/warehouses.md)
