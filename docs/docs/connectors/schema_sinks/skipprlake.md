# SkipprLake Schema Sink

Aligns Iceberg table metadata with discovered schemas for pipelines using the [SkipprLake data sink](../outputs/skipprlake.md).

The schema sink uses the same `SkipprLake` config as the data sink. Pair them: a SkipprLake data sink must use a SkipprLake schema sink.

## Configuration

```yaml
data_sinks:
  lake:
    SkipprLake:
      warehouse: s3://my-iceberg-warehouse/
      catalog_table: my-iceberg-catalog
      region: us-east-1
      table_namespace: bronze
    schema_sink: schema_sinks.lake_schema

schema_sinks:
  lake_schema:
    SkipprLake:
      warehouse: s3://my-iceberg-warehouse/
      catalog_table: my-iceberg-catalog
      region: us-east-1
      table_namespace: bronze
```

| Field | Description |
| --- | --- |
| `warehouse` | Iceberg warehouse root — same as the data sink |
| `catalog_table` | Catalog pointer table — same as the data sink |
| `region` | AWS region for the catalog backend |
| `object_store` | Object-store credentials for Parquet |
| `table_namespace` | Iceberg namespace for sink-managed tables |

## Related

- [SkipprLake output](../outputs/skipprlake.md)
- [Output destination](../../configuration/output.md)
