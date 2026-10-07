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

Object storage credentials are `object_store`. `type: s3` (or omit it) is the AWS default credential chain and requires a `s3://` warehouse. `type: r2` is Cloudflare R2 (or other path-style S3-compatible stores). `type: file` is local parquet and requires a `file:///` warehouse. Catalog pointer credentials stay on the catalog backend, not `object_store`.

```yaml
object_store:
  type: file
```

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
| `object_store` | `s3` | Parquet store: `file` (`file://` warehouse), `s3` (`s3://`), or `r2` (`s3://` + endpoint) |
| `table_namespace` | `default` | Iceberg namespace for sink-managed tables |

## Supported write policies

| Policy | Supported |
| --- | --- |
| `append` | Yes |
| `merge_by_key` | Yes |
| `replace_partition` | Yes |
| `replace_table` | Yes |

See [Source landing semantics](../../concepts/source-landing-semantics.md).

## Table history and maintenance

The sink maintains its own tables. Nothing needs scheduling.

- **Snapshot retention.** Snapshots older than 24 hours are expired once the newest 100 commits are kept, along with the manifests and metadata files only they used. Do not rely on time travel or rollback to older snapshots. Retention is fixed, not configurable.
- **Small files and deletes.** After a commit, the sink may rewrite a partition's small files into larger ones. When a partition collects more than 16 equality-delete files (from `merge_by_key` or CDC), it rewrites that partition with the deletes applied and drops them. A pass is bounded, so large tables are maintained over several commits.

Query results do not change: maintenance commits replace files without changing rows. External engines that read the table (Athena, Snowflake, DuckDB, Spark) see ordinary Iceberg `replace` and `overwrite` snapshots.

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
