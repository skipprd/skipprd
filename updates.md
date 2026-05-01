# Skippr EL Integration — Implementor Notes

This document covers the new features added to support `skippr-dbt` EL orchestration. It is aimed at developers implementing projects on top of Skippr.

---

## Summary of new features

| Feature | CLI / SQL | Description |
|---|---|---|
| MSSQL Input Plugin | `DATA_SOURCE_PLUGIN_NAME=Mssql` | Read tables from Microsoft SQL Server |
| Snowflake Output Plugin | `DATA_OUTPUT_PLUGIN_NAME=Snowflake` | Write compacted Parquet to Snowflake |
| `LOAD SCHEMA` DDL | `LOAD SCHEMA '<path>' INTO <pipeline>` | Pre-define destination schemas from a JSON file |
| `SHOW PIPELINE` DDL | `SHOW PIPELINE <name>` | Return structured pipeline status with field details |
| `--output json` / `--output text` | `skipprd sync --output json` | Structured output for programmatic parsing |
| `--once` flag | `skipprd sync --once` | Single sync pass (bounded execution) |
| Local Metadata Persistence | `SKIPPRD_EL_STORAGE_MODE=local` | Read/write skipprd EL metadata and stats to local disk |
| Discover Mode Enhancements | `skipprd discover --output json` | Schema-only mode with structured output, no output plugin |

---

## 1. MSSQL Input Plugin

### Connection

Set the ADO.NET connection string:

```bash
DATA_SOURCE_PLUGIN_NAME=Mssql
MSSQL_CONNECTION_STRING="Server=tcp:myserver.database.windows.net,1433;Database=MyDB;User Id=myuser;Password=mypass;Encrypt=true;"
```

Or via YAML:

```yaml
data_inputs:
  source:
    Mssql:
      connection_string: "Server=tcp:localhost,1433;Database=MyDB;User Id=sa;Password=pass;"
      tables:            # optional; omit to auto-discover all base tables
        - "dbo.customers"
        - "dbo.orders"
      batch_size_rows: 10000
```

### Table discovery

When `tables` is omitted, the plugin queries `INFORMATION_SCHEMA.TABLES` to discover all `BASE TABLE` entries.

### Namespace convention

```
mssql.{database}.{schema}.{table}
```

Example: `mssql.MyDB.dbo.customers`

### Offset tracking

Each table is tracked as a closed offset. After a table has been fully ingested, `--once` re-runs will skip it unless offsets are reset via `RESET PIPELINE`.

### Environment variables

| Variable | Required | Description |
|---|---|---|
| `DATA_SOURCE_PLUGIN_NAME` | Yes | Must be `Mssql` |
| `MSSQL_CONNECTION_STRING` | Yes | ADO.NET connection string |

---

## 2. Snowflake Output Plugin

### Authentication

Supports two methods:

- **Key-pair (recommended):** Set `SNOWFLAKE_PRIVATE_KEY_PATH` to a PKCS8 PEM private key. Generates a JWT — no password required.
- **Username/password:** Set `SNOWFLAKE_PASSWORD`. Authenticates via the Snowflake REST login endpoint.

### Configuration

```bash
DATA_OUTPUT_PLUGIN_NAME=Snowflake
SNOWFLAKE_ACCOUNT=myorg-myaccount
SNOWFLAKE_USER=skippr_loader
SNOWFLAKE_PRIVATE_KEY_PATH=/path/to/rsa_key.p8
SNOWFLAKE_WAREHOUSE=COMPUTE_WH
SNOWFLAKE_DATABASE=RAW_DATA
SNOWFLAKE_SCHEMA=PUBLIC
SNOWFLAKE_ROLE=LOADER_ROLE              # optional
SNOWFLAKE_STAGE=@SKIPPR_STAGE           # optional, defaults to @~ (user stage)
```

### Data flow

1. WAL compactor produces a Parquet stream.
2. Plugin serializes stream to an in-memory Parquet file (sorted, Snappy-compressed).
3. `PUT` to Snowflake stage to obtain upload credentials, then uploads Parquet to the stage's backing storage (with client-side encryption when required).
4. `COPY INTO <table> FROM '<stage>/...' FILE_FORMAT=(TYPE=PARQUET) MATCH_BY_COLUMN_NAME=CASE_INSENSITIVE` bulk-loads the data.
5. Staged file is removed after successful load.

### Namespace → table name mapping

Dots are replaced with underscores, then lowercased:

```
mssql.MyDB.dbo.customers  →  mssql_mydb_dbo_customers
```

### Schema management

Schema DDL runs proactively during pipeline initialisation via the shared schema sync worker (same mechanism as Athena):

- `CREATE SCHEMA IF NOT EXISTS` ensures the target schema exists.
- `CREATE TABLE IF NOT EXISTS` with full structured type support (OBJECT, ARRAY, MAP).
- Schema evolution: new fields trigger `ALTER TABLE ADD COLUMN IF NOT EXISTS`.
- DDL is serialized per table with schema-aware caching.

### Type mapping

| Skippr Type | Snowflake Type |
|---|---|
| String | `VARCHAR` |
| Integer / Long | `NUMBER(38,0)` |
| Double | `DOUBLE` |
| Boolean | `BOOLEAN` |
| Date | `DATE` |
| Timestamp | `TIMESTAMP_NTZ` |
| Struct | `OBJECT(field TYPE, ...)` |
| Array | `ARRAY(element_type)` |
| Map | `MAP(key_type, value_type)` |

---

## 3. LOAD SCHEMA DDL

### Syntax

```sql
LOAD SCHEMA '/path/to/schema.json' INTO my_pipeline
```

### JSON format

```json
{
  "version": 1,
  "tables": [
    {
      "skippr_namespace": "mssql.MyDB.dbo.customers",
      "columns": [
        { "name": "id", "type": "NUMBER" },
        { "name": "name", "type": "VARCHAR" },
        { "name": "email", "type": "VARCHAR" },
        { "name": "created_at", "type": "TIMESTAMP" },
        { "name": "is_active", "type": "BOOLEAN" }
      ]
    },
    {
      "skippr_namespace": "mssql.MyDB.dbo.orders",
      "columns": [
        { "name": "order_id", "type": "NUMBER" },
        { "name": "customer_id", "type": "NUMBER" },
        { "name": "total", "type": "DECIMAL" },
        { "name": "order_date", "type": "DATE" }
      ]
    }
  ]
}
```

### Type mapping table

| JSON `type` value | Skippr internal type |
|---|---|
| `VARCHAR`, `STRING`, `TEXT`, `NVARCHAR`, `CHAR`, `NCHAR`, `NTEXT` | String |
| `NUMBER`, `INT`, `INTEGER`, `BIGINT`, `SMALLINT`, `TINYINT` | Long |
| `DOUBLE`, `FLOAT`, `REAL`, `NUMERIC`, `DECIMAL`, `MONEY`, `SMALLMONEY` | Double |
| `BOOLEAN`, `BOOL`, `BIT` | Boolean |
| `DATE` | Date |
| `TIMESTAMP`, `TIMESTAMP_NTZ`, `TIMESTAMP_LTZ`, `TIMESTAMP_TZ`, `DATETIME`, `DATETIME2`, `SMALLDATETIME`, `DATETIMEOFFSET` | Timestamp |
| `VARIANT`, `OBJECT`, `ARRAY` | String |

### Usage from skippr-dbt

1. Generate the schema JSON from the destination (e.g., Snowflake `DESCRIBE TABLE` or MSSQL `INFORMATION_SCHEMA.COLUMNS`).
2. Write JSON to a temp file.
3. Execute: `skipprd query --sql "LOAD SCHEMA '/tmp/schema.json' INTO el_mssql" --plain`

---

## 4. SHOW PIPELINE DDL

### Syntax

```sql
SHOW PIPELINE el_mssql
```

### Output format

The output now includes full field details per namespace (field name, inferred type, and nullable flag):

```json
{
  "pipeline": "el_mssql",
  "status": "active",
  "namespaces": [
    {
      "name": "mssql.MyDB.dbo.customers",
      "enabled": true,
      "fields": [
        { "name": "id", "type": "Long", "nullable": true },
        { "name": "email", "type": "String", "nullable": true },
        { "name": "created_at", "type": "Timestamp", "nullable": true }
      ]
    }
  ],
  "offsets": {},
  "metadata_location": "s3://bucket/tenant/workspace/el_mssql/metadata/metadata.json"
}
```

When `SKIPPRD_EL_STORAGE_MODE=local`, the `metadata_location` field shows the local disk path.

### Usage from skippr-dbt

```bash
skipprd query --sql "SHOW PIPELINE el_mssql" --plain
```

Parse the JSON output to determine pipeline status and read the full discovered schema including field names and inferred types. This is how skippr-dbt retrieves source schemas for the LLM mapping phase.

---

## 5. Structured output modes (`--output`)

### Flags

```bash
skipprd sync --pipeline el_mssql --output json   # JSON lines to stdout
skipprd sync --pipeline el_mssql --output text   # Plain text to stdout
skipprd sync --pipeline el_mssql --output progress  # Default interactive spinner
```

### JSON event schema

All JSON events are emitted as single-line JSON to stdout. Tracing logs remain on stderr.

| Event | Key fields | When emitted |
|---|---|---|
| `sync_start` | `pipeline` | Start of sync |
| `namespace_discovered` | `namespace`, `field_count` | During schema preparation |
| `schema_evolved` | `namespace`, `fields_added` | When schema changes are detected |
| `batch_ingested` | `namespace`, `rows`, `bytes` | After each ingest batch |
| `compaction_complete` | `namespace`, `parquet_file` | After WAL compaction |
| `output_synced` | `namespace`, `rows_written` | After output plugin writes |
| `sync_complete` | `pipeline`, `namespaces_synced`, `total_rows`, `elapsed_ms` | End of sync |
| `sync_error` | `pipeline`, `error`, `namespace` | On error |

Every event includes a `timestamp` field (RFC 3339).

### Example JSON output

```json
{"event":"sync_start","pipeline":"el_mssql","timestamp":"2026-03-18T12:00:00.000Z"}
{"event":"namespace_discovered","namespace":"mssql.MyDB.dbo.customers","field_count":12,"timestamp":"2026-03-18T12:00:01.000Z"}
{"event":"namespace_discovered","namespace":"mssql.MyDB.dbo.orders","field_count":8,"timestamp":"2026-03-18T12:00:01.100Z"}
{"event":"sync_complete","pipeline":"el_mssql","namespaces_synced":2,"total_rows":15000,"elapsed_ms":4200,"timestamp":"2026-03-18T12:00:05.200Z"}
```

### Parsing from skippr-dbt

Read stdout line by line, parse each as JSON, switch on the `event` field:

```python
import json, subprocess

proc = subprocess.Popen(
    ["skipprd", "sync", "--pipeline", "el_mssql", "--once", "--output", "json"],
    stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
)
for line in proc.stdout:
    event = json.loads(line)
    if event["event"] == "sync_complete":
        print(f"Synced {event['total_rows']} rows in {event['elapsed_ms']}ms")
```

---

## 6. `--once` flag (bounded sync)

### Usage

```bash
skipprd sync --pipeline el_mssql --once
```

When `--once` is set:

- Runs a single full sync pass across all configured pipelines.
- Drains the compactor, flushes all data to the output, then exits with code 0.
- Without `--once`, multi-pipeline mode loops every 10 seconds.

### skippr-dbt invocation pattern

```bash
skipprd sync --pipeline el_mssql --once --output json
```

This is the canonical invocation for orchestrated EL: a single bounded pass with structured output for monitoring.

---

## 7. Local Metadata Persistence (`SKIPPRD_EL_STORAGE_MODE`)

### Problem

`Config::get_metadata()`, `Config::set_metadata()`, and related functions were S3-only. skippr-dbt runs skipprd in local disk mode with no S3 bucket configured.

### Solution

A new config option `SKIPPRD_EL_STORAGE_MODE` controls where skipprd EL metadata and stats are persisted:

| | |
|---|---|
| **Environment variable** | `SKIPPRD_EL_STORAGE_MODE` |
| **YAML** | `skippr.skipprd_el_storage_mode` |
| **Values** | `s3` (default), `local` |

### When `skipprd_el_storage_mode=local`

- **Metadata**: read from / written to `{DATA_DIR}/metadata.json` (atomic write via temp + rename)
- **Stats**: read from / written to `{DATA_DIR}/stats/{namespace}.json`
- **SHOW PIPELINE**: `metadata_location` returns the local disk path

Storage mode only controls where skipprd EL internal state is persisted. All destination operations (schema sync, Glue catalog updates, output writes) continue to work regardless of storage mode.

### Affected code paths

| Function | File | Behavior |
|---|---|---|
| `Config::get_metadata()` | `src/helpers/configuration.rs` | Reads local JSON file |
| `Config::set_metadata()` | `src/helpers/configuration.rs` | Atomic write to local JSON |
| `Config::delete_metadata()` | `src/helpers/configuration.rs` | Removes local JSON file |
| `Config::write_namespace_stats_async()` | `src/helpers/configuration.rs` | Writes to `stats/` subdirectory |
| `Config::read_namespace_stats_async()` | `src/helpers/configuration.rs` | Reads from `stats/` subdirectory |
| `show_pipeline()` | `src/sqlrt/query.rs` | Returns local path for `metadata_location` |

### skippr-dbt usage

```bash
SKIPPRD_EL_STORAGE_MODE=local \
DATA_DIR=./data \
skipprd discover --pipeline el_mssql --output json
```

Or via YAML:

```yaml
skippr:
  skipprd_el_storage_mode: local
```

---

## 8. Discover Mode Enhancements

### 8a. Output plugin removed from discover

`skipprd discover` no longer initializes or syncs to any output plugin. It uses a no-op output internally. The discover flow is now:

1. Load/create pipeline metadata (local or S3)
2. Initialize offsets
3. Run the input plugin to sample data and trigger type inference
4. Persist the updated metadata
5. Exit

### 8b. `--output` flag on discover

The `--verbose` flag has been replaced by `--output`:

```bash
skipprd discover --pipeline el_mssql --output json
```

| Mode | Description |
|---|---|
| `progress` | Interactive spinner (default) |
| `json` | Structured JSON lines to stdout |
| `text` | Plain text summaries to stdout |

#### JSON events emitted by discover

| Event | Key fields | When emitted |
|---|---|---|
| `discover_start` | `pipeline` | Start of discover |
| `namespace_discovered` | `namespace`, `fields` | After metadata is persisted, per namespace |
| `discover_complete` | `pipeline`, `namespaces_discovered`, `elapsed_ms` | End of discover |

The `fields` array in `namespace_discovered` includes the inferred field names and `SkipprDataType` names:

```json
{"event":"namespace_discovered","namespace":"mssql.MyDB.dbo.customers","fields":[{"name":"id","type":"Long"},{"name":"email","type":"String"}],"timestamp":"..."}
```

### 8c. SHOW PIPELINE now includes field details

`SHOW PIPELINE` output has been extended from simple field counts to full field details:

```json
{
  "namespaces": [
    {
      "name": "mssql.MyDB.dbo.customers",
      "enabled": true,
      "fields": [
        { "name": "id", "type": "Long", "nullable": true },
        { "name": "email", "type": "String", "nullable": true }
      ]
    }
  ]
}
```

This is how skippr-dbt retrieves the discovered source schemas to feed into the LLM mapping phase after running `skipprd discover`.

### skippr-dbt orchestration with discover

```bash
# Step 1: Discover schemas
skipprd discover --pipeline el_mssql --output json

# Step 2: Read discovered schemas
skipprd query --sql "SHOW PIPELINE el_mssql" --plain

# Step 3: Feed schemas to LLM mapper (skippr-dbt logic)
# Step 4: Generate and load destination schema
skipprd query --sql "LOAD SCHEMA '/tmp/schema.json' INTO el_mssql" --plain

# Step 5: Run sync
skipprd sync --pipeline el_mssql --once --output json
```

---

## End-to-end example: skippr-dbt orchestration

The following illustrates a full EL cycle driven by `skippr-dbt` using local storage mode:

### Step 0: Discover source schemas

```bash
SKIPPRD_EL_STORAGE_MODE=local \
DATA_SOURCE_PLUGIN_NAME=Mssql \
MSSQL_CONNECTION_STRING="Server=tcp:myserver,1433;Database=SalesDB;User Id=sa;Password=pass;" \
skipprd discover --pipeline el_mssql --output json
```

After discover, read the schemas:

```bash
skipprd query --sql "SHOW PIPELINE el_mssql" --plain
```

### Step 1: Generate and load schema

```bash
# Generate schema JSON from the destination system (e.g., Snowflake)
cat > /tmp/schema.json << 'EOF'
{
  "version": 1,
  "tables": [
    {
      "skippr_namespace": "mssql.SalesDB.dbo.customers",
      "columns": [
        { "name": "id", "type": "NUMBER" },
        { "name": "name", "type": "VARCHAR" },
        { "name": "email", "type": "VARCHAR" },
        { "name": "created_at", "type": "TIMESTAMP" }
      ]
    },
    {
      "skippr_namespace": "mssql.SalesDB.dbo.orders",
      "columns": [
        { "name": "order_id", "type": "NUMBER" },
        { "name": "customer_id", "type": "NUMBER" },
        { "name": "total", "type": "DECIMAL" },
        { "name": "order_date", "type": "DATE" }
      ]
    }
  ]
}
EOF

# Load the schema with skipprd
skipprd query --sql "LOAD SCHEMA '/tmp/schema.json' INTO el_mssql" --plain
```

### Step 2: Run sync

```bash
skipprd sync --pipeline el_mssql --once --output json 2>/dev/null | while read -r line; do
  event=$(echo "$line" | jq -r '.event')
  case "$event" in
    sync_start)
      echo "Sync started for pipeline: $(echo "$line" | jq -r '.pipeline')"
      ;;
    sync_complete)
      rows=$(echo "$line" | jq -r '.total_rows')
      ms=$(echo "$line" | jq -r '.elapsed_ms')
      echo "Sync complete: $rows rows in ${ms}ms"
      ;;
    sync_error)
      echo "ERROR: $(echo "$line" | jq -r '.error')"
      exit 1
      ;;
  esac
done
```

### Step 3: Check pipeline status

```bash
skipprd query --sql "SHOW PIPELINE el_mssql" --plain
```

This returns a JSON summary that `skippr-dbt` can parse to verify the pipeline state, namespace counts, and schema integrity.
