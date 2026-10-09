# Changelog

All notable changes to this project will be documented in this file.

## [18.1.2]

### Added

- Enumerated `ALTER TABLE` matrix in Rust (`rename` / `merge` / `drop` / `promote`, matching and sequential Iceberg ids, nested `TO` paths) and an S3 → SkipprLake e2e job that runs the same statements against a real lake table.

### Fixed

- `RENAME COLUMN region TO detail.region` moves the field into the nested record and writes the flattened Iceberg name `detail_region`.
- Quoted hyphenated Iceberg namespaces (`s3_alter."skippr-e2e-sample-data"`) resolve for `ALTER TABLE`.

## [18.1.1]

### Fixed

- `ALTER TABLE` matches Iceberg columns by name (including flattened `detail.truck_reg` → `detail_truck_reg`) when skippr's hashed `field_id` is not the Iceberg id. Glue tables created with sequential Iceberg ids can be renamed, promoted, merged, and dropped.
- `discover` no longer force-enables a `DISABLED` pipeline, so the host loop cannot undo `DISABLE PIPELINE` before `ALTER TABLE`.

## [18.1.0]

## [18.0.0]

### Breaking

- Python `skippr` 18.0.0 builds configs with typed classes. `Config().workspace(...).storage(LocalStorage())`, then `cfg.data_source(name, DataSourceS3(...))`, `cfg.data_sink(...)`, `cfg.schema_sink(...)`, and `cfg.pipeline(name, Pipeline(data_source=ref, ...))`. Registration returns typed refs, and `Session(pipeline_ref)` is the only constructor. `Config.load` / `Config.discover` read YAML; `Config.save` merge-writes with the `skipprd connect` writer. Removed: `Session.connect()`, `Session(pipeline=..., config=..., config_file=...)`, `Session.config()`, `skippr.workspace()` / `tenant()` / `storage_mode()`, `SkipprRoot`, `StorageMode`, `OffsetStore`, `SkipprStore`, and the `DataSource` / `DataSink` / `SchemaSink` enums.
- Python plugin classes are always role-prefixed (`DataSourceOtlp`, not `Otlp`). Secret fields take `skippr.EnvRef`; plaintext is a type error. String enums are `Literal` types. Tagged fields take one class per variant (`DataSinkSkipprLakeWarehouseObjectStoreR2`). The wheel ships `skippr/__init__.pyi` and `py.typed`, and Linux wheels are `manylinux_2_28`.
- `pipelines.*.reset_offsets` / `reset_metadata` and the `RESET_OFFSETS` / `RESET_METADATA` env vars are removed. Nothing read them.
- `skippr.yml` rejects unknown keys outside plugin blocks: root, `skippr`, `skippr.store`, pipelines, `transform`, `stats`, `semantic_layer`, and `dbt`. `schema_sink` belongs on the data sink entry, not the pipeline. In those engine blocks `${ENV}` references fill string fields only, so `sync_frequency_seconds: ${SYNC}` is a load error at runtime too; plugin blocks are read by their plugin. The aliases `input` / `output` / `deadletter(s)` on pipelines and `data_inputs` / `data_outputs` / `data_deadletters` / `schema_outputs` at the root are removed; use `data_source` / `data_sink` / `deadletter_sink` and `data_sources` / `data_sinks` / `deadletter_sinks` / `schema_sinks`.
- One merge rule for `skipprd connect`, Python registration, and `Config.save`: each top-level key you set replaces that key; keys you don't set are kept. A flattened nested CLI flag (`--object-store-type file`) replaces the whole `object_store` block. A new file starts empty, so `connect` no longer writes `skipprd_el_storage_mode: local` unless you pass it. `--store-name` requires `--store-type`.
- `skipprd connect`, `Config.save`, and Python registration check the whole merged file: every entry names a known plugin (kinds match case-insensitively and are written in canonical case), and every secret field is exactly `${NAME}`. They write YAML only and refuse `skipprd.json` / `skipprd.toml`. Entry names cannot contain `.`. Every pipeline that has a `data_source` must validate, no two sinks may share a SkipprLake or Duckdb namespace, and every sink must pair with the schema sink it links. A failing check leaves the file as it was, including `connect` root flags such as `--workspace`. An entry still being filled in one field at a time may be written; `discover`, `sync`, and a `Session` refuse it, and `skipprd doctor` reports it. A Duckdb, SkipprLake, or AthenaIceberg schema sink and every data or deadletter sink of that plugin that links it hold one config: the fields `connect` or Python `data_sink` / `deadletter_sink` registration passes go to all of them, a new or empty one starts as a copy, and a group that already differs is refused rather than overwritten. `Config.save` never rewrites a pair. A `Session` runs the same whole-config check before it narrows to its pipeline.
- A Python `Session` resolves and checks only its own pipeline's entries, so `query` and `df` see that pipeline's views. Startup checks (dependency violations, `WAL_STORAGE`, `SKIPPRD_EL_STORAGE_MODE`, `SKIPPR_STORE_TYPE`, reserved pipeline names, the data directory) raise `ValueError` instead of exiting the interpreter. `Session.doctor()` returns `ok` and `checks`; the `config_path` key is removed. `S3Storage`, `DynamoDbStore`, `CloudTablesStore`, `workspace`, `tenant`, and `wal_s3_bucket` reject empty values. Free-form fields (`inject_fields`, `filters`) keep integers exact and raise on integers outside 64 bits, non-finite floats, non-`str` keys, and values that are not JSON.
- `skipprd query` registers views only for pipelines the config defines; a pipeline still listed in the workspace registry but removed from `skippr.yml` is skipped instead of failing the query.
- `pipelines.*.cdc.business_key_columns` is removed; use `cdc.default.business_key_columns`. `cdc` and `cdc.default` / `cdc.namespaces.*` reject unknown keys.
- Rust `skipprd::api::Session::from_config` returns `Session`, not `Result`.
- `TRANSFORM_FLATTEN_EVENTS` applies wherever `transform.flatten_events` is read, including cluster pipeline views and discovered metadata.
- The deprecated `offset_store` / `offset_dynamodb_table` keys are rejected at load, and `SKIPPR_OFFSET_STORE` / `SKIPPR_OFFSET_DYNAMODB_TABLE` are a startup error. Use `skippr.store` and `SKIPPR_STORE_TYPE` / `SKIPPR_STORE_NAME`.

## 17.0.0 and earlier (not split by version)

### Breaking

- The `Iceberg` data-sink and schema-sink plugin is **`SkipprLake`**. Nested `catalog.type` / `query_engine` / `table_prefix` / `table_location_prefix` are removed. Flat keys: `warehouse`, `catalog_table`, `table_namespace`, `region`.
- Athena on Iceberg is a separate plugin **`AthenaIceberg`**. `Athena` stays Hive Parquet + Glue. `skipprd query` serves Iceberg ∪ WAL for SkipprLake, AthenaIceberg, and Duckdb; Hive Athena and other non-Iceberg sinks stay WAL-only.
- **`skipprd serve`** exposes Iceberg REST (`crates/skippr-iceberg-rest`) and local Flight SQL for one physical Iceberg catalog.
- **`Duckdb`** is a skipprd output plugin (filesystem Iceberg catalog). SDE does not treat DuckDB as a warehouse.
- SDE skipprd-binary env is **`SKIPPRD_BIN`** only.
- Plugin identity in `by_name` is generated `DataSource`/`DataSink`/`SchemaSink::parse` (case-insensitive). `Azure` is not `AzureBlob`.
- `skipprd query` / `SHOW DOCS` / `sql-help` statement copy comes from `sqlrt::docs::get_sql_docs()`. Checked-in `sql-docs.md` must match `get_sql_docs_formatted()`. Glue/Athena are not the engine query catalog.
- SkipprStore (`skippr.store.type` + `skippr.store.name`) replaces `offset_store` / `offset_dynamodb_table`. Catalog pointers MAY share that table. Old YAML/env keys still parse and log a deprecation warning.

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