# Duckdb Schema Sink

Aligns Iceberg table metadata with discovered schemas for pipelines using the [Duckdb data sink](../outputs/duckdb.md).

The schema sink uses the same `Duckdb` config as the data sink. Pair them: a Duckdb data sink must use a Duckdb schema sink.

## Configuration

```yaml
data_sinks:
  local:
    Duckdb:
      warehouse: file:///Users/me/lake
      table_namespace: bronze
    schema_sink: schema_sinks.local_schema

schema_sinks:
  local_schema:
    Duckdb:
      warehouse: file:///Users/me/lake
      table_namespace: bronze
```

| Field | Description |
| --- | --- |
| `warehouse` | Iceberg warehouse root (`file://`) — same as the data sink |
| `table_namespace` | Iceberg namespace for sink-managed tables — same as the data sink |

## Related

- [Duckdb output](../outputs/duckdb.md)
- [Output destination](../../configuration/output.md)
