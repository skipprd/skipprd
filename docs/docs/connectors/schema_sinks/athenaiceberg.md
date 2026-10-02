# AthenaIceberg Schema Sink

Aligns Iceberg table metadata with discovered schemas for pipelines using the [AthenaIceberg data sink](../outputs/athenaiceberg.md).

The schema sink uses the same `AthenaIceberg` config as the data sink. Pair them: an AthenaIceberg data sink must use an AthenaIceberg schema sink, not Glue Hive.

## Configuration

```yaml
data_sinks:
  warehouse:
    AthenaIceberg:
      warehouse: s3://my-bucket/warehouse/
      glue_database_name: analytics
      athena_workgroup_name: primary
      athena_results_s3_bucket: my-athena-results
      region: us-east-1
    schema_sink: schema_sinks.warehouse_schema

schema_sinks:
  warehouse_schema:
    AthenaIceberg:
      warehouse: s3://my-bucket/warehouse/
      glue_database_name: analytics
      athena_workgroup_name: primary
      athena_results_s3_bucket: my-athena-results
      region: us-east-1
```

| Field | Description |
| --- | --- |
| `warehouse` | Iceberg warehouse root — same as the data sink |
| `glue_database_name` | Glue database — same as the data sink |
| `athena_workgroup_name` | Athena workgroup — same as the data sink |
| `athena_results_s3_bucket` | Athena results bucket — same as the data sink |
| `region` | AWS region for Glue |
| `catalog_id` | Glue catalog ID when not the account default |
| `object_store` | Object-store credentials for Parquet |

## Related

- [AthenaIceberg output](../outputs/athenaiceberg.md)
- [Output destination](../../configuration/output.md)
