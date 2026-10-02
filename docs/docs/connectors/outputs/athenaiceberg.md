# AthenaIceberg Output

Writes compacted batches to Apache Iceberg tables in the AWS Glue catalog, on S3. Query those tables with Amazon Athena.

`skipprd query` does not read AthenaIceberg tables; that path is live WAL only. Pair with the [AthenaIceberg schema sink](../schema_sinks/athenaiceberg.md). Do not pair this sink with the Hive [Glue](../schema_sinks/glue.md) schema sink.

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
```

Table location is `{warehouse}/{glue_database_name}/{table_name}`. Table names are unprefixed.

`athena_results_s3_bucket` is a **bucket name**, not an `s3://` URI.

Object storage credentials are `object_store`. Omit it or set `type: s3` for the AWS default credential chain. Use `type: r2` for Cloudflare R2 (or other path-style S3-compatible stores).

```yaml
object_store:
  type: r2
  endpoint: ${OBJECTS_S3_ENDPOINT}
  region: auto
  access_key_id: ${OBJECTS_ACCESS_KEY_ID}
  secret_access_key: ${OBJECTS_SECRET_ACCESS_KEY}
  path_style: true
```

| Field | Default | Description |
| --- | --- | --- |
| `warehouse` | *(required)* | Iceberg warehouse root (`s3://…`) |
| `glue_database_name` | *(required)* | Glue database; also the Iceberg namespace |
| `athena_workgroup_name` | *(required)* | Athena workgroup for SQL |
| `athena_results_s3_bucket` | *(required)* | Athena query-results bucket name |
| `region` | | AWS region for Glue and Athena |
| `catalog_id` | | Glue catalog ID when not the account default |
| `object_store` | `s3` | Object-store credentials for Parquet |

## Supported write policies

| Policy | Supported |
| --- | --- |
| `append` | Yes |
| `merge_by_key` | Yes |
| `replace_partition` | Yes |
| `replace_table` | Yes |

## Related

- [AthenaIceberg schema sink](../schema_sinks/athenaiceberg.md)
- [Athena Hive output](athena.md)
- [SkipprLake](skipprlake.md)
- [Output destination](../../configuration/output.md)
