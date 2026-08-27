# Iceberg Output

Writes compacted batches to Apache Iceberg tables using a configured catalog (Skippr, Glue, REST, Unity, or Polaris).

Pair with the [Iceberg schema sink](../schema_sinks/iceberg.md) when catalog DDL should run through `schema_sinks` instead of inline on every write.

## Configuration

```yaml
data_sinks:
  lake:
    Iceberg:
      catalog:
        type: glue
        warehouse: s3://my-iceberg-warehouse/
        database: analytics
        region: us-east-1
      table_namespace: bronze
      format: parquet
```

### Skippr catalog

Skippr-managed Iceberg catalog. Use this for clustered query (Iceberg ∪ live WAL). Create a DynamoDB table for catalog pointers and pass its name here. This MUST NOT be the offset/lease table (`SKIPPR_OFFSET_DYNAMODB_TABLE`).

```yaml
catalog:
  type: skippr
  table: my-iceberg-catalog
  warehouse: s3://my-iceberg-warehouse/
  region: us-east-1
```

### Glue catalog

```yaml
catalog:
  type: glue
  warehouse: s3://my-iceberg-warehouse/    # required — table storage root
  database: analytics                       # optional Glue database name
  catalog_id: "123456789012"               # optional — cross-account catalog
  region: us-east-1
```

### REST / Unity / Polaris catalogs

```yaml
catalog:
  type: rest
  uri: https://iceberg.example.com/catalog
  warehouse: analytics
```

Unity and Polaris use the same `uri` + `warehouse` shape with optional auth fields (`token`, `client_id`, `client_secret`).

| Field | Default | Description |
| --- | --- | --- |
| `catalog` | *(required)* | Catalog connection (see above) |
| `table_namespace` | | Namespace segment for table identifiers |
| `table_prefix` | | Prefix prepended to each namespace table name |
| `table_location_prefix` | | Override base path for new tables |
| `properties` | | Extra Iceberg table properties (map) |
| `format` | `parquet` | File format for data files |
| `query_engine` | | Optional Athena query engine for `skippr model` / `skippr query` |

Iceberg stays Iceberg. Model and query use `query_engine` on this sink. Do not project Iceberg YAML into a separate `Athena:` block.

```yaml
query_engine:
  type: athena
  workgroup: primary
```

## Supported write policies

Contract-aware sources can use this sink when landing semantics require merge or partition replace:

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
    schema_sink: schema_sinks.iceberg_catalog
```

## AWS permissions (Glue catalog)

When using `type: glue`, the runtime identity needs S3 read/write on the warehouse path and Glue catalog permissions for databases, tables, and commits.

## Related

- [Iceberg schema sink](../schema_sinks/iceberg.md)
- [Output destination](../../configuration/output.md)
