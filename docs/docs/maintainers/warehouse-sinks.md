# Warehouse sinks (contract)

Normative locks for skipprd warehouse families. Campaign notes: [warehouse-sinks-cutover.md](./warehouse-sinks-cutover.md). SkipprLake dbt/REST is [skipprlake-dbt-rest.md](./skipprlake-dbt-rest.md).

The **data sink plugin key** selects the warehouse family. There is no `query_engine`.

| Plugin key | Writes | Read by |
|---|---|---|
| `SkipprLake` | Iceberg on object storage; catalog pointers in DynamoDB or Cloud Tables | `skipprd query` (Iceberg ∪ WAL), `skipprd serve`, Spark/Trino via REST |
| `Athena` | Hive Parquet plus Glue | Athena only |
| `AthenaIceberg` | Iceberg on S3, Glue catalog | `skipprd query` (Iceberg ∪ WAL), `skipprd serve`, and Athena SQL |
| `Duckdb` | Iceberg on `file://`, metadata colocated with data | `skipprd query` (Iceberg ∪ WAL), `skipprd serve`, and DuckDB `iceberg_scan`. SDE does not model DuckDB. |

Configs using `Iceberg:` fail as an unknown plugin. Nested `catalog.type` is not a SkipprLake field. Table location is `{warehouse}/{table_namespace}/{table_name}`. Each SkipprLake sink owns a unique `(catalog_table, table_namespace)`.

`skipprd query` resolves `QueryBackend { Iceberg(IcebergLake), WalOnly }` once at the edge. Iceberg sinks are SkipprLake, AthenaIceberg, and Duckdb. Hive Athena, Snowflake, and other non-Iceberg sinks serve live WAL only. `IcebergLake` carries a serializable `IcebergCatalogSpec` (Skippr / Glue / filesystem). Catalogs open asynchronously at I/O sites, not on the sync edge.
