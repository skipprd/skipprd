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

| MSSQL Type | Skipprd Type |
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

## Authentication

| Variable | Description |
|---|---|
| `MSSQL_CONNECTION_STRING` | ADO.NET connection string for SQL Server |

### Connection string format

```bash
export MSSQL_CONNECTION_STRING="server=tcp:127.0.0.1,1433;database=testdb;user id=sa;password=YourPass;TrustServerCertificate=true"
```

Common parameters:

| Parameter | Description |
|---|---|
| `server` | Hostname and port (for example `tcp:myserver.database.windows.net,1433`) |
| `database` | Database name |
| `user id` | SQL Server login |
| `password` | Login password |
| `TrustServerCertificate` | Set to `true` for self-signed certs in dev or test |
| `Encrypt` | Set to `true` for Azure SQL or production workloads |

### Azure SQL

For Azure SQL Database, use the fully qualified server name:

```bash
export MSSQL_CONNECTION_STRING="server=tcp:myserver.database.windows.net,1433;database=mydb;user id=myuser@myserver;password=MyPass;Encrypt=true;TrustServerCertificate=false"
```

### Local dev with Docker

Spin up a local MSSQL instance for testing:

```bash
docker run -e 'ACCEPT_EULA=Y' -e 'SA_PASSWORD=YourStrong!Passw0rd' \
  -p 1433:1433 --name mssql-dev \
  -d mcr.microsoft.com/mssql/server:2022-latest
```

```bash
export MSSQL_CONNECTION_STRING="server=tcp:127.0.0.1,1433;database=master;user id=sa;password=YourStrong!Passw0rd;TrustServerCertificate=true"
```

## Troubleshooting

| Symptom | Fix |
|---|---|
| `Login failed for user` | Verify username and password in the connection string |
| `Cannot open server` | Check the server hostname, port, and network access |
| `SSL Provider: certificate verify failed` | Add `TrustServerCertificate=true` for self-signed certs, or install the server CA certificate |
