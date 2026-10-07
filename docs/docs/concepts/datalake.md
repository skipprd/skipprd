# Datalake

A **Datalake** stores data as files in object storage so many tools can read it, instead of copying it into one vendor’s warehouse first.

A **data warehouse** is optimized for SQL analytics on modeled tables. Skipprd's Datalake **enables** that: ELT pipelines land data in the lake; [Apache Iceberg](https://iceberg.apache.org/) tables on those files are what warehouse engines and Skipprd SQL both query. Iceberg is the open table format on the Datalake — snapshots, schema, and SQL over Parquet — without locking the data in a closed engine.

Skipprd is not a warehouse product. Warehouses you already use (Athena, Snowflake, and others) can read the same Iceberg tables.

## How Skipprd uses it

1. **ELT** ingest writes a durable WAL, then compact to Iceberg Parquet in object storage.
2. **SkipprLake**, **AthenaIceberg**, and **Duckdb** each write an Iceberg catalog skipprd can query. SkipprLake pointers live in DynamoDB or Cloud Tables (`catalog_table`). AthenaIceberg uses Glue. Duckdb uses a filesystem catalog on `file://`.
3. Those sinks maintain their Iceberg tables automatically. They expire snapshots older than 24 hours once the newest 100 commits are kept, rewrite small files, and remove equality-delete files. Commit cost and table metadata stay flat as history grows. Older snapshots are not a time-travel promise.
4. The lake identity is Iceberg `namespace.table`. `skipprd serve` Flight SQL and REST expose that name. `skipprd query` also registers `pipeline.namespace` as the local Iceberg ∪ WAL view of ingest. Hive Athena stays WAL-only for skipprd SQL. That clustered path is documented in [maintainer architecture](../maintainers/hla-distributed-query-iceberg-catalog.md).

See also [How Skipprd Works](how-it-works.md), [SkipprLake](../connectors/outputs/skipprlake.md), [AthenaIceberg](../connectors/outputs/athenaiceberg.md), and [Duckdb](../connectors/outputs/duckdb.md).
