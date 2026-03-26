# skippr - EL Integration Work Items

Work items required to support skippr-dbt's table-to-table EL orchestration.
skippr-dbt will generate config, invoke skippr CLI, and read structured output.
These items make skippr a capable execution engine for that workflow.

---

## 1. MSSQL Input Plugin

**Priority:** Hard requirement
**Location:** `src/plugins/mssql_input.rs`, `src/plugins/mod.rs`, `src/helpers/configuration.rs`

### Summary

New input plugin that reads rows from MSSQL tables and feeds them into skippr's existing ingestion pipeline (discover -> schema evolution -> WAL -> compaction -> output).

### Config

Add `Mssql` variant to `DataSourcePluginConfig` enum in `src/helpers/configuration.rs`:

```rust
pub enum DataSourcePluginConfig {
    S3(DataSourceS3PluginConfig),
    File(DataSourceLocalFilePluginConfig),
    Mssql(DataSourceMssqlPluginConfig),
}
```

```yaml
data_inputs:
  source_db:
    Mssql:
      connection_string: "${MSSQL_CONNECTION_STRING}"
      tables:                           # optional, omit to discover all user tables
        - dbo.customers
        - dbo.orders
      batch_size_rows: 10000            # rows per Arrow batch
      query_timeout_seconds: 300
```

### Plugin Config Struct

```rust
#[derive(Debug, Deserialize, Clone)]
pub struct DataSourceMssqlPluginConfig {
    pub connection_string: String,
    pub tables: Option<Vec<String>>,          // fully qualified: schema.table
    pub batch_size_rows: Option<usize>,       // default 10_000
    pub query_timeout_seconds: Option<u64>,   // default 300
    pub format: Option<String>,               // always "row" for DB sources
    pub batch_size_bytes: Option<i64>,
    pub batch_size_seconds: Option<i64>,
}
```

### Table Discovery (when `tables` is omitted)

When `tables` is `None`, the plugin must discover all user tables from the source:

```sql
SELECT TABLE_SCHEMA, TABLE_NAME
FROM INFORMATION_SCHEMA.TABLES
WHERE TABLE_TYPE = 'BASE TABLE'
ORDER BY TABLE_SCHEMA, TABLE_NAME
```

Return the results as `schema.table` pairs. This list becomes the set of tables to ingest.

### Namespace Strategy

Each source table maps to a skippr namespace using the fully qualified identifier to avoid metadata collisions at the destination:

```
mssql.{database}.{schema}.{table}
```

Example: `mssql.MyDatabase.dbo.customers`

The dot-separated namespace preserves the full source lineage and avoids collisions when multiple sources or schemas are ingested into the same pipeline. skippr's existing namespace handling already supports arbitrary string namespaces.

### Data Flow

1. Connect to MSSQL via `tiberius` (or `bb8-tiberius` for connection pooling)
2. For each table: `SELECT * FROM [schema].[table]` with batched row streaming
3. Convert each row batch to an Arrow RecordBatch (using skippr's existing type inference or a direct MSSQL-to-Arrow type map)
4. Feed into `Ingest` via the same `IngestTask` / `IngestBatch` path that `DataSourceS3Plugin` and `DataSourceLocalFilePlugin` use
5. Namespace is set to the fully qualified identifier (`mssql.{db}.{schema}.{table}`)

### Offset Tracking

Use skippr's existing `Offsets` (sled-based) with `OffsetKey` set to `mssql:{database}.{schema}.{table}`. For initial implementation, each sync run re-reads the full table. Incremental support (via a configured cursor column) is a follow-up.

### Dependencies

- `tiberius` crate (MSSQL TDS protocol client)
- `bb8` or `deadpool` for connection pooling (optional, can start without)
- `tokio-util` (for the TDS TCP stream)

---

## 2. Snowflake Output Plugin

**Priority:** Hard requirement
**Location:** `src/plugins/snowflake_output.rs`, `src/plugins/mod.rs`, `src/helpers/configuration.rs`

### Summary

New output plugin that writes compacted Parquet data to a Snowflake table. Implements the existing `DataOutputPlugin` trait.

### Config

Add `Snowflake` variant to `DataSinkPluginConfig` enum in `src/helpers/configuration.rs`:

```rust
pub enum DataSinkPluginConfig {
    Athena(DataSinkAthenaPluginConfig),
    File(DataSinkFilePluginConfig),
    S3(DataSinkS3PluginConfig),
    Snowflake(DataSinkSnowflakePluginConfig),
}
```

```yaml
data_outputs:
  warehouse:
    Snowflake:
      account: "${SNOWFLAKE_ACCOUNT}"
      user: "${SNOWFLAKE_USER}"
      password: "${SNOWFLAKE_PASSWORD}"
      warehouse: "${SNOWFLAKE_WAREHOUSE}"
      database: ANALYTICS
      schema: RAW
      role: "${SNOWFLAKE_ROLE}"
      stage: "@~"                        # default user stage, or named stage
```

### Plugin Config Struct

```rust
#[derive(Debug, Deserialize, Clone)]
pub struct DataSinkSnowflakePluginConfig {
    pub account: String,
    pub user: String,
    pub password: String,
    pub warehouse: String,
    pub database: String,
    pub schema: String,
    pub role: Option<String>,
    pub stage: Option<String>,          // default: "@~" (user stage)
    pub format: Option<String>,
}
```

### Data Flow

1. Receive `SendableRecordBatchStream` from compaction (Parquet-ready Arrow batches)
2. Serialize to in-memory Parquet (sorted, Snappy-compressed via `serialize_to_parquet`)
3. `PUT` to Snowflake stage to obtain upload credentials for backing storage
4. Upload Parquet to stage's backing S3 (with client-side AES-256-CBC encryption when required)
5. Execute `COPY INTO {database}.{schema}.{table} FROM '{stage}/...' FILE_FORMAT=(TYPE=PARQUET) MATCH_BY_COLUMN_NAME=CASE_INSENSITIVE`
6. `REMOVE` the staged file after successful load
7. Map the skippr namespace (e.g. `mssql.MyDatabase.dbo.customers`) to a destination table name. The table name derivation should use the final segment by default (e.g. `customers`) but be overridable via the mapping spec.

### Schema Management

The Snowflake output plugin must:

- `CREATE TABLE IF NOT EXISTS` using the schema from skippr's `PipelineMetadata` (converted to Snowflake DDL types)
- Handle schema evolution: `ALTER TABLE ADD COLUMN` when new fields appear (same as Athena/Glue plugin does for Glue catalog updates)
- Type mapping from skippr `SkipprDataType` to Snowflake types:

| SkipprDataType    | Snowflake Type    |
|-------------------|-------------------|
| String            | VARCHAR           |
| Integer           | NUMBER(38,0)      |
| Long              | NUMBER(38,0)      |
| Double            | DOUBLE            |
| Boolean           | BOOLEAN           |
| Date              | DATE              |
| Timestamp         | TIMESTAMP_NTZ     |
| TimestampMilli    | TIMESTAMP_NTZ     |
| Array             | VARIANT           |
| Record            | VARIANT           |
| Map               | VARIANT           |

### Snowflake Connectivity

Use the Snowflake REST SQL API (`snowflake-api` crate or direct HTTP) rather than ODBC. This keeps the dependency lightweight and container-friendly.

Auth credentials are passed via env vars and resolved through `Config::getenv` (same pattern as all other skippr config).

---

## 3. Implement `LOAD SCHEMA` DDL

**Priority:** Hard requirement
**Location:** `src/sqlrt/parser/mod.rs`, `src/sqlrt/query.rs`

### Summary

The `LOAD SCHEMA` statement is already parsed in the SQL parser but returns a "not implemented" error. This needs to be fully implemented so that skippr-dbt can contribute destination schemas to skippr before running `sync`.

### Current State

In `src/sqlrt/parser/mod.rs`, `parse_load()` returns:
```rust
Err(ParserError::ParserError(
    "Not implemented - LOAD SCHEMA not currently supported".to_string(),
))
```

The `SchemaLoadStatement` struct and `SchemaLoadDest` enum already exist.

### Syntax

```sql
LOAD SCHEMA '/path/to/schema.json' INTO pipeline_name
```

### Schema JSON Format

The schema file is a JSON document describing the destination columns for each namespace. This is the contract between skippr-dbt (which generates it) and skippr (which consumes it):

```json
{
  "version": 1,
  "tables": [
    {
      "skippr_namespace": "mssql.MyDatabase.dbo.customers",
      "columns": [
        { "name": "id", "type": "NUMBER" },
        { "name": "created_at", "type": "TIMESTAMP_NTZ" },
        { "name": "email", "type": "VARCHAR" }
      ]
    }
  ]
}
```

### Implementation

1. Uncomment the parse logic in `parse_load()` (the commented-out code already handles the syntax)
2. In `src/sqlrt/query.rs`, handle the `Statement::SchemaLoad` case:
   - Read the JSON file from the specified path
   - Parse and validate the schema JSON
   - For each entry in `tables`:
     - Convert the `columns` to skippr's internal `Metadata` format (`HashMap<String, Metadata>`)
     - Map the `type` strings to `SkipprDataType` values
     - Upsert into the pipeline's `PipelineMetadata.metadata` under the specified `skippr_namespace`
   - Persist the updated `PipelineMetadata` (via `Config::set_metadata()`)
3. Return a success/failure result that skippr-dbt can parse

### Type Mapping (JSON `type` string -> `SkipprDataType`)

The type strings in the schema JSON are destination-native types (e.g., Snowflake types). The LOAD SCHEMA implementation should map common destination type names to `SkipprDataType`:

| Destination Type | SkipprDataType |
|------------------|----------------|
| VARCHAR, STRING, TEXT | String |
| NUMBER, INT, INTEGER, BIGINT | Long |
| DOUBLE, FLOAT, REAL, NUMERIC, DECIMAL | Double |
| BOOLEAN, BOOL | Boolean |
| DATE | Date |
| TIMESTAMP, TIMESTAMP_NTZ, TIMESTAMP_LTZ, TIMESTAMP_TZ, DATETIME | Timestamp |
| VARIANT, OBJECT, ARRAY | String (or Record/Array contextually) |

Unrecognized types should default to `String` with a warning log.

---

## 4. Implement `SHOW PIPELINE` DDL

**Priority:** Hard requirement
**Location:** `src/sqlrt/parser/mod.rs`, `src/sqlrt/query.rs`

### Summary

New SQL DDL statement that returns pipeline status information. Used by skippr-dbt to query pipeline state after sync operations.

### Syntax

```sql
SHOW PIPELINE pipeline_name
```

### Parser Changes

Add `PIPELINE` to `SkipprShowCommand` enum and handle it in the `SHOW` branch of `parse_statement()`:

```rust
enum SkipprShowCommand {
    DOCS,
    STATS,
    SEMANTIC,
    CATALOG,
    PIPELINE,  // new
}
```

Add a new `Statement` variant:

```rust
pub enum Statement {
    // ... existing variants ...
    ShowPipeline { pipeline: String },
}
```

### Output Format

Return a structured JSON result containing:

```json
{
  "pipeline": "my_pipeline",
  "status": "active",
  "namespaces": [
    {
      "name": "mssql.MyDatabase.dbo.customers",
      "field_count": 12,
      "enabled": true,
      "last_schema_md5": "abc123..."
    },
    {
      "name": "mssql.MyDatabase.dbo.orders",
      "field_count": 8,
      "enabled": true,
      "last_schema_md5": "def456..."
    }
  ],
  "offsets": {
    "mssql:MyDatabase.dbo.customers": "2024-03-18T10:30:00Z",
    "mssql:MyDatabase.dbo.orders": "2024-03-18T10:30:00Z"
  },
  "metadata_location": "s3://bucket/tenant/workspace/pipeline/metadata/metadata.json"
}
```

### Implementation

1. Load `PipelineMetadata` via `Config::get_metadata()`
2. Load offset state from the sled-based `Offsets` store
3. Assemble the response JSON
4. When `--plain` flag is set on the `query` command, output as raw JSON to stdout (parseable by skippr-dbt)

---

## 5. Structured Output Modes

**Priority:** Required for skippr-dbt integration
**Location:** `src/helpers/logging.rs`, `src/main.rs`, `src/cli/mod.rs`

### Summary

skippr-dbt needs to parse skippr's output programmatically. The `--output` flag on the `sync` command should support three modes.

### CLI Change

Add `--output` flag to `SyncOptions` in `src/cli/mod.rs`:

```rust
#[derive(Parser, Clone, PartialEq)]
pub struct SyncOptions {
    #[arg(short, long)]
    pub pipeline: Option<String>,
    #[arg(long, default_value = "progress")]
    pub output: String,                 // "progress" (default), "json", or "text"
}
```

### Output Modes

| Mode | Description | Use case |
|------|-------------|----------|
| `progress` | Interactive progress bar (`ProgressUi`) | Human running skippr directly (default) |
| `json` | Structured JSON lines to stdout | skippr-dbt parsing output programmatically |
| `text` | Plain text log lines to stdout | Non-interactive terminals, CI logs |

### JSON Log Format (when `--output json`)

Each line is a self-contained JSON object:

```json
{"event": "sync_start", "pipeline": "el_mssql", "timestamp": "2024-03-18T10:30:00Z"}
{"event": "namespace_discovered", "namespace": "mssql.MyDB.dbo.customers", "field_count": 12}
{"event": "schema_evolved", "namespace": "mssql.MyDB.dbo.customers", "fields_added": ["new_col"]}
{"event": "batch_ingested", "namespace": "mssql.MyDB.dbo.customers", "rows": 10000, "bytes": 524288}
{"event": "compaction_complete", "namespace": "mssql.MyDB.dbo.customers", "parquet_file": "..."}
{"event": "output_synced", "namespace": "mssql.MyDB.dbo.customers", "rows_written": 50000}
{"event": "sync_complete", "pipeline": "el_mssql", "namespaces_synced": 2, "total_rows": 150000, "elapsed_ms": 45000}
{"event": "sync_error", "pipeline": "el_mssql", "error": "Connection refused", "namespace": "mssql.MyDB.dbo.customers"}
```

### Implementation

- `progress` (default): existing `ProgressUi` behavior, unchanged
- `json`: replace `ProgressUi` with a JSON emitter; each significant event emits a JSON line to stdout; tracing logs continue to stderr
- `text`: disable `ProgressUi`, emit plain text summaries to stdout (similar to `--log info` but without tracing formatting)
- Exit code 0 on success, non-zero on failure (unchanged across all modes)
- The final `sync_complete` or `sync_error` event is always emitted in `json` mode

---

## 6. Batch/Bounded Sync Mode (`--once`)

**Priority:** Required for skippr-dbt integration
**Location:** `src/cli/mod.rs`, `src/main.rs`

### Summary

skippr currently runs as a long-lived daemon that continuously syncs. skippr-dbt needs a "run once and exit" mode: sync all pending data, then exit cleanly with a summary.

### Current Behavior

The `sync` command enters a loop: ingest -> WAL -> compact -> output -> sleep -> repeat. It runs until killed.

### CLI Change

Add `--once` flag to `SyncOptions` in `src/cli/mod.rs`:

```rust
#[derive(Parser, Clone, PartialEq)]
pub struct SyncOptions {
    #[arg(short, long)]
    pub pipeline: Option<String>,
    #[arg(long, default_value = "progress")]
    pub output: String,
    #[arg(long, default_value_t = false)]
    pub once: bool,                     // exit after single sync pass
}
```

`--once` and `--output` are independent flags. A typical skippr-dbt invocation uses both:

```bash
skippr sync --pipeline el_mssql --once --output json
```

### Desired Behavior

When invoked with `--once`, skippr should:

1. Run the full pipeline once (discover -> ingest -> WAL -> compact -> output sync)
2. Emit a summary (via structured JSON if `--output json`, or plain text otherwise)
3. Exit with code 0 on success, non-zero on failure

### Implementation

When `--once` is set:
- Skip the sleep/repeat loop
- After the output sync completes, drain any pending Glue/catalog operations
- Emit `sync_complete` event
- Exit

This is a minimal change to the existing sync loop — just break after the first iteration when `--once` is true.

---

## 7. Local Metadata Persistence

**Priority:** Hard requirement for skippr-dbt integration
**Location:** `src/helpers/configuration.rs`

### Summary

`Config::get_metadata()` and `Config::set_metadata()` currently always use S3 as the source of truth. skippr-dbt runs skippr in local disk mode with no S3 bucket configured. Metadata must be readable and writable on local disk.

### Current Behavior

```rust
pub async fn get_metadata() -> Result<PipelineMetadata, bool> {
    let s3_key = format!("{}/{}/{}/metadata/metadata.json", tenant, workspace, pipeline);
    match s3::get_json(&s3_key).await { ... }
}

pub async fn set_metadata(pipeline_metadata: &PipelineMetadata, evolved: bool) {
    let s3_key = format!("{}/{}/{}/metadata/metadata.json", tenant, workspace, pipeline);
    match s3::put_json(&s3_key, &json_value).await { ... }
}
```

Both are S3-only. When `skippr_s3_bucket` is empty (local disk mode), these fail or no-op.

### Desired Behavior

When `skippr_s3_bucket` is not configured (empty string), fall back to local disk:

- **Read**: `{data_dir}/metadata.json` (where `data_dir` = `Config::get_data_dir()`)
- **Write**: same path, atomic write (write to temp + rename)
- **S3 mode unchanged**: when `skippr_s3_bucket` is set, behavior is identical to today

### Implementation

Add a local branch to both `get_metadata` and `set_metadata`:

```rust
pub async fn get_metadata() -> Result<PipelineMetadata, bool> {
    let bucket = Self::get_skippr_s3_bucket();
    if bucket.is_empty() {
        // Local disk mode
        let path = format!("{}/metadata.json", Self::get_data_dir());
        match std::fs::read_to_string(&path) {
            Ok(contents) => {
                match serde_json::from_str::<PipelineMetadata>(&contents) {
                    Ok(md) => Ok(md),
                    Err(e) => { error!("Failed to parse local metadata: {}", e); Err(false) }
                }
            }
            Err(_) => Err(false) // no metadata yet
        }
    } else {
        // existing S3 path
        ...
    }
}
```

Same pattern for `set_metadata`: check bucket, write to local JSON file if empty.

### Affected Code Paths

- `Config::get_metadata()` — read
- `Config::set_metadata()` — write
- `Config::delete_metadata()` — delete
- `SHOW PIPELINE` (item 4) must also use the same local/S3 resolution
- Stats persistence (`write_namespace_stats_async`) — should also fall back to local disk when no S3 bucket is configured, or skip gracefully

---

## 8. Discover Mode Enhancements

**Priority:** Hard requirement for skippr-dbt integration
**Location:** `src/cli/mod.rs`, `src/main.rs`

### Summary

skippr-dbt delegates all source schema discovery to skippr via `skippr discover`. The existing discover mode needs several enhancements to work cleanly in this flow.

### Current State of `skippr discover`

The `discover()` function in `src/main.rs` (line 507):

1. Loads/creates `PipelineMetadata` from S3
2. Initializes offsets (sled, local disk)
3. **Initializes an output plugin** (design mistake — discover should not touch output)
4. Runs `sync_input_plugin()` which ingests data, triggering schema discovery via type inference
5. **Syncs discovered schemas to the output plugin** (writes Parquet + Glue updates — should not happen in discover)
6. Persists metadata to S3
7. Generates LLM summaries

### Changes Needed

#### 8a. Remove output plugin from discover (bug fix)

`skippr discover` should be purely about schema inference. It should never initialize or sync to an output plugin. This is a design correction, not just a skippr-dbt requirement.

Remove from `discover()`:
- Output plugin initialization (`sync_output_plugin()`)
- Output sync after ingestion
- Glue/catalog updates
- Stats persistence

After the fix, `discover()` should:
1. Load/create `PipelineMetadata` (local disk per item 7, or S3)
2. Initialize offsets
3. Run the input plugin to sample data and trigger type inference
4. Persist the updated `PipelineMetadata`
5. Exit

#### 8b. `--output` flag on discover

Add the same `--output` flag as sync, replacing the current `--verbose` boolean:

```rust
#[derive(Parser, Clone, PartialEq)]
pub struct DisocverOptions {
    #[arg(short, long)]
    pub pipeline: Option<String>,
    #[arg(long, default_value = "progress")]
    pub output: String,                 // "progress" (default), "json", or "text"
}
```

When `--output json`, emit structured events:

```json
{"event": "discover_start", "pipeline": "el_mssql", "timestamp": "..."}
{"event": "namespace_discovered", "namespace": "mssql.MyDB.dbo.customers", "fields": [{"name": "id", "type": "Long"}, {"name": "email", "type": "String"}]}
{"event": "namespace_discovered", "namespace": "mssql.MyDB.dbo.orders", "fields": [{"name": "order_id", "type": "Long"}, {"name": "total", "type": "Double"}]}
{"event": "discover_complete", "pipeline": "el_mssql", "namespaces_discovered": 2, "elapsed_ms": 12000}
```

The `fields` array uses `SkipprDataType` names (String, Long, Double, Boolean, Date, Timestamp, etc.) — these are the inferred source types, not destination types.

skippr-dbt invocation:

```bash
skippr discover --pipeline el_mssql --output json
```

#### 8c. Discover reads back via SHOW PIPELINE

After `skippr discover` completes, skippr-dbt reads the discovered schemas via `SHOW PIPELINE` (item 4). The `SHOW PIPELINE` implementation must include the discovered field schemas in its output — not just field counts, but the actual field names and inferred types. This is how skippr-dbt gets the source schemas to feed into the LLM mapping phase.

Extend the `SHOW PIPELINE` output (item 4) to include field details per namespace:

```json
{
  "pipeline": "el_mssql",
  "status": "active",
  "namespaces": [
    {
      "name": "mssql.MyDatabase.dbo.customers",
      "enabled": true,
      "fields": [
        { "name": "id", "type": "Long", "nullable": false },
        { "name": "email", "type": "String", "nullable": true },
        { "name": "created_at", "type": "Timestamp", "nullable": true }
      ]
    }
  ],
  "offsets": { ... }
}
```

The `fields` array is derived from the namespace's `Metadata.fields` in `PipelineMetadata`, converting each field's `determined_type` to its string representation and `out_field_name` to the field name.

---

## Summary Table

| Item | Files | New Deps | Priority |
|------|-------|----------|----------|
| MSSQL Input Plugin | `plugins/mssql_input.rs`, `plugins/mod.rs`, `configuration.rs` | `tiberius`, `bb8-tiberius` | Hard req |
| Snowflake Output Plugin | `plugins/snowflake_output.rs`, `plugins/mod.rs`, `configuration.rs` | `snowflake-api` or HTTP | Hard req |
| `LOAD SCHEMA` DDL | `sqlrt/parser/mod.rs`, `sqlrt/query.rs` | None | Hard req |
| `SHOW PIPELINE` DDL | `sqlrt/parser/mod.rs`, `sqlrt/query.rs` | None | Hard req |
| Output Modes (`--output`) | `cli/mod.rs`, `helpers/logging.rs`, `main.rs` | None | Required |
| Batch/Bounded Sync (`--once`) | `main.rs` | None | Required |
| Local Metadata Persistence | `helpers/configuration.rs` | None | Hard req |
| Discover Mode Enhancements | `cli/mod.rs`, `main.rs` | None | Hard req |
