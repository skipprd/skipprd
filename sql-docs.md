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

### SCHEMA LOAD

**Syntax:**
```sql
LOAD SCHEMA '<source_path>' INTO <pipeline_name>
```

**Description:**
Loads a schema definition from a file into a pipeline.

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

## Data Operations

### DROP TABLE

**Syntax:**
```sql
DROP TABLE [<schema_name>.]<table_name>
```

**Description:**
Drops a table from the metadata and from AWS Glue catalog.

**Example:**
```sql
DROP TABLE analytics.user_events
```

### DROP DATABASE

**Syntax:**
```sql
DROP DATABASE <database_name>
```

**Description:**
Drops a database from the AWS Glue Catalog.

**Example:**
```sql
DROP DATABASE data_warehouse
```

## Query Operations

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

### SELECT

**Syntax:**
```sql
SELECT <columns> FROM <table_name> [WHERE <condition>] [GROUP BY <expressions>] [HAVING <condition>] [ORDER BY <expressions>] [LIMIT <count>]
```

**Description:**
Executes a standard SQL query against the data. Supports querying from AWS Athena/Glue tables.

**Example:**
```sql
SELECT user_id, COUNT(*) FROM bike_hire WHERE date > '2023-01-01' GROUP BY user_id LIMIT 10
```

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

