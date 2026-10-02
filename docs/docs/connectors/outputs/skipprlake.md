# SkipprLake Output

Writes compacted batches to Apache Iceberg tables via SkipprLake. Clustered `skipprd query` unions those tables with live WAL.

Pair with the [SkipprLake schema sink](../schema_sinks/skipprlake.md). `AthenaIceberg` and `Duckdb` are separate plugins.

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
```

`catalog_table` is a DynamoDB table (or Cloud Tables namespace) that holds Iceberg pointers. It MAY be the same table as SkipprStore (`skippr.store.name`); PK/SK prefixes do not collide.

Each SkipprLake data sink must own a unique `(catalog_table, table_namespace)`. Table location is `{warehouse}/{table_namespace}/{table_name}`.

Object storage credentials are `object_store`. Omit it or set `type: s3` for the AWS default credential chain. Use `type: r2` for Cloudflare R2 (or other path-style S3-compatible stores). Catalog pointer credentials stay on the catalog backend, not `object_store`.

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
| `warehouse` | *(required)* | Iceberg warehouse root (`s3://…` or `file:///…`) |
| `catalog_table` | *(required)* | Catalog pointer table. MAY share SkipprStore. |
| `region` | | AWS region for the catalog backend |
| `object_store` | `s3` | Object-store credentials for Parquet |
| `table_namespace` | `default` | Iceberg namespace for sink-managed tables |

## Supported write policies

| Policy | Supported |
| --- | --- |
| `append` | Yes |
| `merge_by_key` | Yes |
| `replace_partition` | Yes |
| `replace_table` | Yes |

See [Source landing semantics](../../concepts/source-landing-semantics.md).

## Pipeline wiring

```yaml
pipelines:
  reports:
    data_source: data_sources.saas
    data_sink: data_sinks.lake
```

Pairing is `data_sinks.<name>.schema_sink`, not a pipeline-level `schema_sink` field.

## Related

- [SkipprLake schema sink](../schema_sinks/skipprlake.md)
- [Output destination](../../configuration/output.md)
