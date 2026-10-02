# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Breaking

- The `Iceberg` data-sink and schema-sink plugin is **`SkipprLake`**. Nested `catalog.type` / `query_engine` / `table_prefix` / `table_location_prefix` are removed. Flat keys: `warehouse`, `catalog_table`, `table_namespace`, `region`.
- Athena on Iceberg is a separate plugin **`AthenaIceberg`**. `Athena` stays Hive Parquet + Glue. `skipprd query` serves WAL only for non-SkipprLake sinks.
- **`Duckdb`** is a skipprd output plugin (filesystem Iceberg catalog). SDE does not treat DuckDB as a warehouse.
- SDE skipprd-binary env is **`SKIPPRD_BIN`** only.
- Plugin identity in `by_name` is generated `DataSource`/`DataSink`/`SchemaSink::parse` (case-insensitive). `Azure` is not `AzureBlob`.
- SkipprStore (`skippr.store.type` + `skippr.store.name`) replaces `offset_store` / `offset_dynamodb_table`. Catalog pointers MAY share that table. Old YAML/env keys still parse and log a deprecation warning.
- `skipprd query` / `SHOW DOCS` / `sql-help` statement copy comes from `sqlrt::docs::get_sql_docs()`. Checked-in `sql-docs.md` must match `get_sql_docs_formatted()`. Glue/Athena are not the engine query catalog.

## [5.7.2] - 2024-03-23

### Added
- Full support for `array` of `structs` data type
- New syntax support in configuration files
- Schema dump now outputs metadata structure for easier alter statements
- Automatic schema synchronization on startup

### Changed
- Improved CPU thread count handling
- Updated output plugin naming convention
- Code cleanup and optimization

### Fixed
- Various compilation issues
- Syntax-related bugs
- Performance issues with large record sets

### Technical Details
- Schema synchronization improvements
- Code structure optimization
- Build system enhancements

## [0.1.0] - 2024-03-23
- Initial development version 