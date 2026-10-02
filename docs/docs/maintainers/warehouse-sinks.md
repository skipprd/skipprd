# Warehouse sinks (contract)

Normative locks for skipprd warehouse families. Campaign notes: [warehouse-sinks-cutover.md](./warehouse-sinks-cutover.md). SkipprLake dbt/REST is [skipprlake-dbt-rest.md](./skipprlake-dbt-rest.md).

The **data sink plugin key** selects the warehouse family. There is no `query_engine`.

| Plugin key | Writes | Read by |
|---|---|---|
| `SkipprLake` | Iceberg on object storage; catalog pointers in DynamoDB or Cloud Tables | `skipprd query` (Ballista over persisted data, DataFusion over WAL) |
| `Athena` | Hive Parquet plus Glue | Athena only |
| `AthenaIceberg` | Iceberg on S3, Glue catalog | Athena only |
| `Duckdb` | Iceberg on `file://`, metadata colocated with data | DuckDB clients only. skipprd output only. SDE does not model DuckDB. |

Configs using `Iceberg:` fail as an unknown plugin. Nested `catalog.type` is not a SkipprLake field. Table location is `{warehouse}/{table_namespace}/{table_name}`. Each SkipprLake sink owns a unique `(catalog_table, table_namespace)`.

`skipprd query` resolves `QueryBackend { SkipprLake(cfg), WalOnly }` once at the edge. Non-SkipprLake pipelines serve live WAL only.
