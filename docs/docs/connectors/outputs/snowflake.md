# Snowflake Output

Writes compacted Parquet data to Snowflake using S3 staging and `COPY INTO` via the Snowflake REST SQL API.

## How it works

1. Receives compacted Parquet streams from the WAL compactor.
2. Serializes the stream to an in-memory Parquet file (sorted, Snappy-compressed).
3. Sends a `PUT` command to Snowflake to obtain temporary upload credentials for the configured stage.
4. Uploads the Parquet file directly to the stage's backing cloud storage (S3), with client-side encryption when required by the stage.
5. Executes `COPY INTO` with `MATCH_BY_COLUMN_NAME = CASE_INSENSITIVE` to bulk-load the staged data into the target table.
6. Removes the staged file after a successful load.
7. Namespace-to-table mapping converts dots to underscores and lowercases the result (e.g., `mssql.MyDB.dbo.customers` → `mssql_mydb_dbo_customers`).

Schema DDL (CREATE SCHEMA, CREATE TABLE, ALTER TABLE ADD COLUMN) is handled proactively by the shared schema sync worker during pipeline initialisation, before any data flows.

## Authentication

The plugin supports two authentication methods:

**Key-pair (recommended):**

```bash
SNOWFLAKE_PRIVATE_KEY_PATH=/path/to/rsa_key.p8
```

Generates a JWT signed with the RSA private key. No password required.

**Username/password:**

```bash
SNOWFLAKE_PASSWORD=secret
```

Authenticates via `POST https://{account}.snowflakecomputing.com/session/v1/login-request`. The session token is cached and reused.

If both `SNOWFLAKE_PRIVATE_KEY_PATH` and `SNOWFLAKE_PASSWORD` are set, key-pair auth takes precedence.

## Configuration

```bash
DATA_OUTPUT_PLUGIN_NAME=Snowflake
SNOWFLAKE_ACCOUNT=myorg-myaccount
SNOWFLAKE_USER=skippr_loader
SNOWFLAKE_PRIVATE_KEY_PATH=/path/to/rsa_key.p8
SNOWFLAKE_WAREHOUSE=COMPUTE_WH
SNOWFLAKE_DATABASE=RAW_DATA
SNOWFLAKE_SCHEMA=PUBLIC
```

Or via YAML pipeline config:

```yaml
data_sinks:
  destination:
    Snowflake:
      account: "myorg-myaccount"
      user: "skippr_loader"
      private_key_path: "${SNOWFLAKE_PRIVATE_KEY_PATH}"
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
| `SNOWFLAKE_PASSWORD` | | Snowflake login password (used when key-pair auth is not configured) |
| `SNOWFLAKE_PRIVATE_KEY_PATH` | | Path to PKCS8 PEM private key for key-pair authentication |
| `SNOWFLAKE_WAREHOUSE` | *(required)* | Compute warehouse name |
| `SNOWFLAKE_DATABASE` | *(required)* | Target database |
| `SNOWFLAKE_SCHEMA` | *(required)* | Target schema |
| `SNOWFLAKE_ROLE` | | Optional role to assume |
| `SNOWFLAKE_STAGE` | `@~` | Snowflake stage for file uploads. Defaults to the user stage. Named stages (e.g., `@SKIPPR_STAGE`) are also supported. |
| `SNOWFLAKE_STAGING_S3_BUCKET` | | Optional S3 bucket for direct S3 staging (bypasses the Snowflake stage). See below. |
| `SNOWFLAKE_STAGING_S3_PREFIX` | `skippr-staging` | Key prefix within the S3 staging bucket |

## Data loading

By default, the plugin uploads Parquet files directly to the configured Snowflake stage using the native file transfer protocol:

1. A `PUT` command is sent to Snowflake to obtain temporary upload credentials for the stage's backing cloud storage.
2. The Parquet file is uploaded to the stage (with client-side AES-256-CBC encryption when required by internal stages).
3. `COPY INTO` loads the staged file into the target table.
4. The staged file is removed after a successful load.

This works out of the box with any S3-backed stage — including the default user stage (`@~`), named internal stages, and named external stages. No additional S3 bucket or IAM configuration is required.

### Optional: direct S3 staging

As an alternative, you can set `SNOWFLAKE_STAGING_S3_BUCKET` to bypass the Snowflake stage and upload Parquet directly to an S3 bucket. Snowflake reads from S3 using inline credentials in the `COPY INTO` statement. This can be useful when the Snowflake account's internal stage storage is limited or when you want to retain staged files for debugging.

## Table naming

Skippr namespaces are converted to Snowflake table names by replacing all dots with underscores and lowercasing:

| Skippr Namespace | Snowflake Table |
|---|---|
| `mssql.MyDB.dbo.customers` | `mssql_mydb_dbo_customers` |
| `s3.events.click_stream` | `s3_events_click_stream` |

## Schema management

Schema DDL runs proactively during pipeline initialisation via the shared schema sync worker (the same mechanism used by Athena):

- `CREATE SCHEMA IF NOT EXISTS` ensures the target schema exists.
- `CREATE TABLE IF NOT EXISTS` creates tables with columns mapped from the Skippr schema, including structured types (OBJECT, ARRAY, MAP).
- Schema evolution: new columns are added via `ALTER TABLE ADD COLUMN IF NOT EXISTS`.
- DDL operations are serialized per table and use schema-aware caching to avoid redundant DDL when the schema hasn't changed.

## Type mapping

| Skippr Type | Snowflake Type |
|---|---|
| String | `VARCHAR` |
| Integer / Long | `NUMBER(38,0)` |
| Double | `DOUBLE` |
| Boolean | `BOOLEAN` |
| Date | `DATE` |
| Timestamp | `TIMESTAMP_NTZ` |
| Struct | `OBJECT(field_name TYPE, ...)` |
| Array | `ARRAY(element_type)` |
| Map | `MAP(key_type, value_type)` |

Nested types are fully preserved as Snowflake structured types rather than flattened to `VARIANT`.

## Required permissions

The Snowflake user needs:

- `USAGE` on warehouse, database, and schema
- `CREATE SCHEMA` on the target database (if the schema doesn't exist)
- `CREATE TABLE` on the target schema
- `INSERT`, `SELECT` on target tables
