# Snowflake Output

Writes compacted Parquet data to Snowflake using the Snowflake REST SQL API.

## How it works

1. Receives compacted Parquet streams from the WAL compactor.
2. Serializes the stream to a local temporary Parquet file.
3. Uploads the Parquet file to a Snowflake stage via `PUT`.
4. Executes `COPY INTO` to load the staged data into the target table.
5. Namespace-to-table mapping converts dots to underscores and lowercases the result (e.g., `mssql.MyDB.dbo.customers` → `mssql_mydb_dbo_customers`).

## Authentication

The plugin authenticates via username/password against the Snowflake login endpoint:

```
POST https://{account}.snowflakecomputing.com/session/v1/login-request
```

The session token is cached and reused for subsequent API calls.

## Configuration

```bash
DATA_OUTPUT_PLUGIN_NAME=Snowflake
SNOWFLAKE_ACCOUNT=myorg-myaccount
SNOWFLAKE_USER=skippr_loader
SNOWFLAKE_PASSWORD=secret
SNOWFLAKE_WAREHOUSE=COMPUTE_WH
SNOWFLAKE_DATABASE=RAW_DATA
SNOWFLAKE_SCHEMA=PUBLIC
```

Or via YAML pipeline config:

```yaml
data_outputs:
  destination:
    Snowflake:
      account: "myorg-myaccount"
      user: "skippr_loader"
      password: "${SNOWFLAKE_PASSWORD}"
      warehouse: "COMPUTE_WH"
      database: "RAW_DATA"
      schema: "PUBLIC"
      role: "LOADER_ROLE"
      stage: "@SKIPPR_STAGE"
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `SNOWFLAKE_ACCOUNT` | *(required)* | Snowflake account identifier (e.g., `myorg-myaccount`) |
| `SNOWFLAKE_USER` | *(required)* | Snowflake login user |
| `SNOWFLAKE_PASSWORD` | *(required)* | Snowflake login password |
| `SNOWFLAKE_WAREHOUSE` | *(required)* | Compute warehouse name |
| `SNOWFLAKE_DATABASE` | *(required)* | Target database |
| `SNOWFLAKE_SCHEMA` | *(required)* | Target schema |
| `SNOWFLAKE_ROLE` | | Optional role to assume |
| `SNOWFLAKE_STAGE` | `@~` | Stage for file uploads. Defaults to user stage. |

## Table naming

Skippr namespaces are converted to Snowflake table names by replacing all dots with underscores and lowercasing:

| Skippr Namespace | Snowflake Table |
|---|---|
| `mssql.MyDB.dbo.customers` | `mssql_mydb_dbo_customers` |
| `s3.events.click_stream` | `s3_events_click_stream` |

## Schema management

- On first load, the plugin creates the table using `CREATE TABLE IF NOT EXISTS` with columns mapped from the skippr schema.
- Schema evolution is supported: new columns are added via `ALTER TABLE ADD COLUMN`.

## Type mapping

| Skippr Type | Snowflake Type |
|---|---|
| String | `VARCHAR` |
| Integer / Long | `NUMBER(38,0)` |
| Double | `DOUBLE` |
| Boolean | `BOOLEAN` |
| Date | `DATE` |
| Timestamp | `TIMESTAMP_NTZ` |
| Array / Record / Map | `VARIANT` |

## Required permissions

The Snowflake user needs:

- `USAGE` on warehouse, database, and schema
- `CREATE TABLE` on the target schema
- `INSERT`, `SELECT` on target tables
- `WRITE` on the configured stage
