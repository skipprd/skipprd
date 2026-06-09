# Iceberg Schema Sink

Aligns Iceberg table metadata with discovered schemas for pipelines using the [Iceberg data sink](../outputs/iceberg.md).

The schema sink shares catalog configuration with the data sink. Both registry entries should use compatible `catalog` settings.

## Configuration

```yaml
pipelines:
  events:
    data_sink: data_sinks.lake
    schema_sink: schema_sinks.iceberg_catalog

data_sinks:
  lake:
    Iceberg:
      catalog:
        type: glue
        warehouse: s3://my-iceberg-warehouse/
        database: analytics
        region: us-east-1

schema_sinks:
  iceberg_catalog:
    Iceberg:
      catalog:
        type: glue
        warehouse: s3://my-iceberg-warehouse/
        database: analytics
        region: us-east-1
```

| Field | Description |
| --- | --- |
| `catalog` | Iceberg catalog definition (`glue`, `rest`, `unity`, or `polaris`) — same shape as the [Iceberg data sink](../outputs/iceberg.md) |
| `table_namespace` | Optional namespace prefix for table names |
| `table_prefix` | Optional table name prefix |
| `table_location_prefix` | Optional storage location override |
| `properties` | Extra Iceberg table properties |

## Pipeline wiring

Set `pipelines.<name>.schema_sink` to the `Iceberg` schema registry entry that matches the pipeline's `Iceberg` data sink catalog.

## Related

- [Iceberg output](../outputs/iceberg.md)
- [Output destination](../../configuration/output.md)
