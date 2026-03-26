# MSSQL Input

Reads data from Microsoft SQL Server tables via the TDS protocol.

## Supported formats

- Row-based JSON (each row serialized as a JSON object)

## How it works

1. Connects to the configured MSSQL instance via TCP/TDS.
2. When no explicit table list is provided, discovers all base tables from `INFORMATION_SCHEMA.TABLES`.
3. For each table, runs `SELECT * FROM [schema].[table]` and converts each row to a JSON object.
4. Batches are ingested through the standard WAL pipeline, with offset tracking per table.
5. Namespace convention: `mssql.{database}.{schema}.{table}` (e.g., `mssql.MyDB.dbo.customers`).

## Configuration

```bash
DATA_SOURCE_PLUGIN_NAME=Mssql
MSSQL_CONNECTION_STRING="Server=tcp:myserver.database.windows.net,1433;Database=MyDB;User Id=myuser;Password=mypass;Encrypt=true;TrustServerCertificate=false;"
```

Or via YAML pipeline config:

```yaml
data_sources:
  source:
    Mssql:
      connection_string: "Server=tcp:localhost,1433;Database=MyDB;User Id=sa;Password=pass;"
      tables:
        - "dbo.customers"
        - "dbo.orders"
      batch_size_rows: 10000
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `MSSQL_CONNECTION_STRING` | *(required)* | ADO.NET-style connection string for MSSQL |
| `tables` | *(auto-discover)* | Optional list of `schema.table` names to ingest. When omitted, all base tables are discovered. |
| `batch_size_rows` | `10000` | Number of rows per ingest batch |
| `query_timeout_seconds` | `300` | Query timeout in seconds |
| `batch_size_bytes` | | Override WAL segment size in bytes |
| `batch_size_seconds` | | Override WAL segment flush interval in seconds |

## Namespace convention

Each table produces a namespace using the pattern:

```
mssql.{database}.{schema}.{table}
```

For example, the table `dbo.customers` in database `MyDB` produces the namespace `mssql.MyDB.dbo.customers`.

## Type mapping

| MSSQL Type | Skippr Type |
|---|---|
| `nvarchar`, `varchar`, `char`, `text`, `ntext` | String |
| `int`, `smallint`, `tinyint` | Integer / Long |
| `bigint` | Long |
| `float`, `real`, `decimal`, `numeric`, `money` | Double |
| `bit` | Boolean |
| `date` | Date |
| `datetime`, `datetime2`, `smalldatetime`, `datetimeoffset` | Timestamp |

## Offset tracking

Each table is tracked as a single offset unit. Once a table has been fully ingested, subsequent `--once` runs will skip it unless offsets are reset.
