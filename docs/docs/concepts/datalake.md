# Datalake

A **Datalake** stores data as files in object storage so many tools can read it, instead of copying it into one vendor’s warehouse first.

A **data warehouse** is optimized for SQL analytics on modeled tables. Skippr’s Datalake **enables** that: ELT pipelines land data in the lake; [Apache Iceberg](https://iceberg.apache.org/) tables on those files are what warehouse engines and Skippr SQL both query. Iceberg is the open table format on the Datalake — snapshots, schema, and SQL over Parquet — without locking the data in a closed engine.

Skippr is not a warehouse product. Warehouses you already use (Athena, Snowflake, and others) can read the same Iceberg tables.

## How Skippr uses it

1. **ELT** ingest writes a durable WAL, then compact to Iceberg Parquet in object storage.
2. **Skippr catalog** (`catalog.type: skippr`) holds Iceberg table pointers. Self-hosted clustered runs use a DynamoDB catalog table; Skippr Cloud uses **tables**.
3. Clustered query unions the Iceberg snapshot with live WAL over Flight SQL. That path is documented in [maintainer architecture](../maintainers/hla-distributed-query-iceberg-catalog.md).

See also [How Skippr Works](how-it-works.md) and the [Iceberg output](../connectors/outputs/iceberg.md).
