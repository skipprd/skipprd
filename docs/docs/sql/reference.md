# SQL Reference

Skippr includes a built-in SQL engine for querying destination tables, managing pipelines, and managing schemas. Execute SQL via `skippr-el query --sql "<statement>"`.

## Query operations

### SELECT

```sql
SELECT <columns> FROM <table>
  [WHERE <condition>]
  [GROUP BY <expressions>]
  [HAVING <condition>]
  [ORDER BY <expressions>]
  [LIMIT <count>]
```

Standard SQL query against Athena/Glue tables.

```sql
-- Example
SELECT user_id, COUNT(*) FROM bike_hire
WHERE date > '2023-01-01'
GROUP BY user_id LIMIT 10
```

### STREAM

```sql
STREAM <columns> FROM <table>
  [WHERE <condition>]
  [ORDER BY <expressions>]
  [LIMIT <count>]
```

Streaming query against the WAL, continuously returning new results as data arrives.

```sql
-- Example
STREAM user_id, event_type FROM user_events
WHERE event_time > CURRENT_TIMESTAMP - INTERVAL '1' HOUR
LIMIT 100
```

### DATEDIFF

```sql
DATEDIFF(<start_date>, <end_date>)
```

Returns the difference in days between two dates. Accepts RFC 3339 formatted strings.

```sql
-- Example
SELECT id, DATEDIFF(start_date, end_date) AS duration FROM bike_hire
```

### SHOW DOCS

```sql
SHOW DOCS
```

Displays documentation for all supported SQL statements.

## Pipeline operations

### ENABLE PIPELINE

```sql
ENABLE PIPELINE <pipeline_name>
```

Enables a pipeline for processing. The pipeline metadata must already exist (run `discover` first). Exits non-zero if the pipeline is not found.

### DISABLE PIPELINE

```sql
DISABLE PIPELINE <pipeline_name>
```

Disables a pipeline, preventing it from processing data.

### DROP PIPELINE

```sql
DROP PIPELINE <pipeline_name>
```

Drops all schemas and data for a pipeline. In sync mode, immediately removes the pipeline data directory. In query mode, the pipeline is dropped on the next sync run.

### RESET PIPELINE

```sql
RESET PIPELINE <pipeline_name>
```

Resets the offset database and purges WAL files for a pipeline. Use this to re-ingest from the beginning.

### SHOW PIPELINE

```sql
SHOW PIPELINE <pipeline_name>
```

Returns a JSON object describing the pipeline's current state, including namespaces with full field details (name, inferred type, nullable), offsets, and metadata location.

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
    },
    {
      "name": "mssql.MyDB.dbo.orders",
      "enabled": true,
      "fields": [
        { "name": "order_id", "type": "Long", "nullable": true },
        { "name": "total", "type": "Double", "nullable": true }
      ]
    }
  ],
  "offsets": {},
  "metadata_location": "s3://bucket/tenant/workspace/el_mssql/metadata/metadata.json"
}
```

When `SKIPPR_STORAGE_MODE=local`, the `metadata_location` field shows the local disk path instead of an S3 URI.

```sql
-- Example
SHOW PIPELINE el_mssql
```

### SHOW STATS

```sql
SHOW STATS FOR <pipeline>[.<namespace>]
```

Shows per-field statistics JSON for a pipeline, optionally filtered by namespace.

```sql
-- Example
SHOW STATS FOR bike_hire.ride_start
```

## Schema operations

### SCHEMA DUMP

```sql
SCHEMA DUMP <pipeline_name>[.<schema_name>] TO '<destination_path>'
```

Exports the schema definition to a JSON file.

```sql
-- Example
SCHEMA DUMP bike_hire TO 'bike_hire_schema.json'
```

### LOAD SCHEMA

```sql
LOAD SCHEMA '<source_path>' INTO <pipeline_name>
```

Loads a schema definition from a JSON file into a pipeline. The source file uses a structured JSON format:

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
        { "name": "created_at", "type": "TIMESTAMP" }
      ]
    }
  ]
}
```

**Type mapping:** The `type` field maps to Skippr internal types:

| JSON type | Skippr type |
|---|---|
| `VARCHAR`, `STRING`, `TEXT` | String |
| `NUMBER`, `INT`, `INTEGER`, `BIGINT` | Long |
| `DOUBLE`, `FLOAT`, `REAL`, `NUMERIC`, `DECIMAL` | Double |
| `BOOLEAN`, `BOOL` | Boolean |
| `DATE` | Date |
| `TIMESTAMP`, `DATETIME`, `TIMESTAMP_NTZ` | Timestamp |
| `VARIANT`, `OBJECT`, `ARRAY` | String |

```sql
-- Example
LOAD SCHEMA '/path/to/schema.json' INTO el_mssql
```

### ALTER SCHEMA DROP COLUMN

```sql
ALTER SCHEMA <pipeline_name>[.<schema_name>] DROP COLUMN <column_name>
```

Drops a column from a schema. Supports nested fields using dot notation.

### ALTER SCHEMA ALTER COLUMN

```sql
ALTER SCHEMA <pipeline_name>[.<schema_name>] ALTER COLUMN <column_name> TYPE <new_type>
```

Changes the data type of a column. For arrays, use `ARRAY<TYPE>` format.

```sql
-- Example
ALTER SCHEMA bike_hire ALTER COLUMN price TYPE DECIMAL(10,2)
```

## Data operations

### DROP DATABASE

```sql
DROP DATABASE <database_name>
```

Drops a database from the AWS Glue Catalog.

### DROP TABLE

```sql
DROP TABLE [<schema_name>.]<table_name>
```

Drops a table from the pipeline metadata and from the AWS Glue catalog.
