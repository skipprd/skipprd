# Skippr SQL Documentation

This document describes all SQL statements supported by Skippr.

## Schema Operations

### ALTER SCHEMA ALTER COLUMN

**Syntax:**
```sql
ALTER SCHEMA <pipeline_name>[.<schema_name>] ALTER COLUMN <column_name> TYPE <new_type>
```

**Description:**
Changes the data type of a column in a schema. For arrays, use ARRAY<TYPE> format.

**Example:**
```sql
ALTER SCHEMA bike_hire ALTER COLUMN price TYPE DECIMAL(10,2)
```

### ALTER SCHEMA DROP COLUMN

**Syntax:**
```sql
ALTER SCHEMA <pipeline_name>[.<schema_name>] DROP COLUMN <column_name>
```

**Description:**
Drops a column from a schema. Supports nested fields using dot notation.

**Example:**
```sql
ALTER SCHEMA bike_hire DROP COLUMN user_id
```

### DROP SCHEMA

**Syntax:**
```sql
DROP SCHEMA <pipeline_name>[.<schema_name>]
```

**Description:**
Drops a schema from a pipeline. On the next sync, the schema will be re-discovered.

**Example:**
```sql
DROP SCHEMA bike_hire
```

### SCHEMA DUMP

**Syntax:**
```sql
SCHEMA DUMP <pipeline_name>[.<schema_name>] TO '<destination_path>'
```

**Description:**
Exports the schema definition of a pipeline or a specific schema within a pipeline to a file.

**Example:**
```sql
SCHEMA DUMP bike_hire TO 'bike_hire_schema.json'
```

### SCHEMA LOAD

**Syntax:**
```sql
LOAD SCHEMA '<source_path>' INTO <pipeline_name>
```

**Description:**
Loads a schema definition from a JSON file into a pipeline. Column `type` maps VARCHAR/STRING/TEXT→String, NUMBER/INT/INTEGER/BIGINT→Long, DOUBLE/FLOAT/REAL/NUMERIC/DECIMAL→Double, BOOLEAN/BOOL→Boolean, DATE→Date, TIMESTAMP/DATETIME/TIMESTAMP_NTZ→Timestamp, VARIANT/OBJECT/ARRAY→String.

**Example:**
```sql
LOAD SCHEMA 'bike_hire_schema.json' INTO bike_hire
```

## Pipeline Operations

### DISABLE PIPELINE

**Syntax:**
```sql
DISABLE PIPELINE <pipeline_name>
```

**Description:**
Disables a pipeline, preventing it from processing data.

**Example:**
```sql
DISABLE PIPELINE analytics
```

### DROP PIPELINE

**Syntax:**
```sql
DROP PIPELINE <pipeline_name>
```

**Description:**
Drops all schemas and data for a pipeline. In sync mode, this immediately removes the pipeline data directory. In query mode, the pipeline will be dropped on the next sync run.

**Example:**
```sql
DROP PIPELINE analytics
```

### ENABLE PIPELINE

**Syntax:**
```sql
ENABLE PIPELINE <pipeline_name>
```

**Description:**
Enables a pipeline for processing.

**Example:**
```sql
ENABLE PIPELINE analytics
```

### RESET PIPELINE

**Syntax:**
```sql
RESET PIPELINE <pipeline_name>
```

**Description:**
Resets the offset database and purges WAL files for a pipeline. In sync mode, this immediately removes the pipeline data directory. In query mode, the pipeline will be reset on the next sync run.

**Example:**
```sql
RESET PIPELINE analytics
```

### SHOW PIPELINE

**Syntax:**
```sql
SHOW PIPELINE <pipeline_name>
```

**Description:**
Show pipeline status as JSON: namespaces (name, enabled, fields), offsets, and metadata_location (S3 URI, or a local path when SKIPPRD_EL_STORAGE_MODE=local).

**Example:**
```sql
SHOW PIPELINE el_mssql
```

## Data Operations

### DEADLETTERS TABLE

**Syntax:**
```sql
SELECT <columns> FROM _dl_<pipeline_name> [WHERE namespace = '<ns>'] [ORDER BY processed_time DESC]
```

**Description:**
Query deadletters from the configured deadletter destination. The table name is `_dl_<pipeline_name>`.

**Example:**
```sql
SELECT id, namespace, error FROM _dl_bike_hire WHERE namespace = 'rides' ORDER BY processed_time DESC LIMIT 50
```

### DROP DATABASE

**Syntax:**
```sql
DROP DATABASE <database_name>
```

**Description:**
Drops an Iceberg namespace from the catalog.

**Example:**
```sql
DROP DATABASE data_warehouse
```

### DROP TABLE

**Syntax:**
```sql
DROP TABLE [<schema_name>.]<table_name>
```

**Description:**
Drops an Iceberg table from the catalog and local metadata.

**Example:**
```sql
DROP TABLE analytics.user_events
```

## Query Operations

### DATEDIFF

**Syntax:**
```sql
DATEDIFF(<start_date>, <end_date>)
```

**Description:**
Calculates the difference in days between two dates. Accepts RFC3339 formatted date strings.

**Example:**
```sql
SELECT id, DATEDIFF(start_date, end_date) AS duration FROM bike_hire
```

### SELECT

**Syntax:**
```sql
SELECT <columns> FROM <table_name> [WHERE <condition>] [GROUP BY <expressions>] [HAVING <condition>] [ORDER BY <expressions>] [LIMIT <count>]
```

**Description:**
Executes a standard SQL query against Iceberg tables and the live WAL.

**Example:**
```sql
SELECT user_id, COUNT(*) FROM bike_hire WHERE date > '2023-01-01' GROUP BY user_id LIMIT 10
```

### SHOW CATALOG

**Syntax:**
```sql
SHOW CATALOG FOR <pipeline>[.<namespace>]
```

**Description:**
Show catalog fields for <pipeline>[.<namespace>]. Falls back to object storage if local cache is missing.

**Example:**
```sql
SHOW CATALOG FOR bike_hire.ride_start
```

### SHOW DOCS

**Syntax:**
```sql
SHOW DOCS
```

**Description:**
Displays the documentation for all supported SQL statements.

**Example:**
```sql
SHOW DOCS
```

### SHOW SEMANTIC

**Syntax:**
```sql
SHOW SEMANTIC FOR <pipeline>[.<namespace>]
```

**Description:**
Show semantic roles for <pipeline>[.<namespace>]. Falls back to object storage if local cache is missing.

**Example:**
```sql
SHOW SEMANTIC FOR bike_hire.ride_start
```

### SHOW STATS

**Syntax:**
```sql
SHOW STATS FOR <pipeline>[.<namespace>]
```

**Description:**
Show per-field statistics JSON for a pipeline (optionally filtered by namespace).

**Example:**
```sql
SHOW STATS FOR bike_hire.ride_start
```

### STREAM

**Syntax:**
```sql
STREAM <columns> FROM <table_name> [WHERE <condition>] [ORDER BY <expressions>] [LIMIT <count>]
```

**Description:**
Executes a streaming SQL query against the data currently ingesting into the WAL, continuously returning new results as data arrives.

**Example:**
```sql
STREAM user_id, event_type FROM user_events WHERE event_time > CURRENT_TIMESTAMP - INTERVAL '1' HOUR ORDER BY event_time LIMIT 100
```

