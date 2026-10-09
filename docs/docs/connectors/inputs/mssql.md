---
title: SQL Server data source
description: Copy Microsoft SQL Server tables into your destination with Skipprd as a one-time full snapshot of each table.
---

# SQL Server

The SQL Server source reads tables from one Microsoft SQL Server database and loads each table in full, once. Later sync runs skip tables that are already loaded, so new and changed rows are not picked up and deletes are not captured. Use it for migrations and one-off copies of a database into a warehouse.

## Before you begin

- **Network access.** The machine running Skipprd must reach the server on its port (default `1433`).
- **No TLS.** Skipprd connects to SQL Server without encryption, including the login. The server must not force encryption (**Force Encryption** off in SQL Server Configuration Manager), and you should run Skipprd on a private network or through a tunnel. Azure SQL Database always requires encryption, so this source cannot connect to it.
- **A SQL login with read access.** Skipprd lists tables from `INFORMATION_SCHEMA` and runs `SELECT *` on each one. `db_datareader` covers both:

```sql
CREATE LOGIN skippr WITH PASSWORD = 'Change-me-123!';
USE MyDB;
CREATE USER skippr FOR LOGIN skippr;
ALTER ROLE db_datareader ADD MEMBER skippr;
```

## Configure

The connection string holds the password, so keep the whole string in an environment variable. Skipprd also reads `.env` and `.env.local` next to `skippr.yml`.

```bash
export MSSQL_CONNECTION_STRING="server=tcp:sql.internal,1433;database=MyDB;user id=skippr;password=Change-me-123!"
```

::: code-group

```python [Python]
from skippr import Config, DataSourceMssql, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "sqlserver",
    DataSourceMssql(
        connection_string=EnvRef("MSSQL_CONNECTION_STRING"),
        tables=["dbo.customers", "dbo.orders"],
    ),
)
pipe = cfg.pipeline("sqlserver", Pipeline(data_source=src))
cfg.save()
```

```bash [CLI]
skipprd connect data-source mssql \
  --pipeline sqlserver \
  --name sqlserver \
  --connection-string '${MSSQL_CONNECTION_STRING}' \
  --tables dbo.customers \
  --tables dbo.orders
```

```yaml [YAML]
pipelines:
  sqlserver:
    data_source: data_sources.sqlserver

data_sources:
  sqlserver:
    Mssql:
      connection_string: ${MSSQL_CONNECTION_STRING}
      tables: ["dbo.customers", "dbo.orders"]
```

:::

Omit `tables` to load every base table in the database.

Add a destination to the pipeline (see the [connector catalog](/connectors/)), then discover the schema and run one sync pass:

::: code-group

```python [Python]
from skippr import Session

session = Session(pipe)
session.discover()
session.sync(once=True)
```

```bash [CLI]
skipprd discover --pipeline sqlserver --log
skipprd sync --pipeline sqlserver --once --log
```

:::

To try the source locally, start SQL Server in Docker and point the connection string at it:

```bash
docker run -e 'ACCEPT_EULA=Y' -e 'MSSQL_SA_PASSWORD=YourStrong!Passw0rd' \
  -p 1433:1433 --name mssql-dev \
  -d mcr.microsoft.com/mssql/server:2022-latest

export MSSQL_CONNECTION_STRING="server=tcp:127.0.0.1,1433;database=master;user id=sa;password=YourStrong!Passw0rd"
```

## Options

| Key | Type | Default | Description |
|---|---|---|---|
| `connection_string` | `${ENV}` reference | Required | ADO.NET-style connection string: `server=tcp:<host>,<port>;database=<db>;user id=<login>;password=<password>`. |
| `tables` | list of strings | every base table in the database | Tables to read, as `schema.table`, or `table` for the `dbo` schema. |
| `batch_size_rows` | integer | `10000` | Rows per batch handed to Skipprd. |
| `query_timeout_seconds` | integer | none | Not used by this source. |
| `format` | string | `json` | Leave unset. Rows are always sent as JSON. |
| `batch_size_bytes` | integer | none | Not used by this source. |
| `batch_size_seconds` | integer | none | Not used by this source. |

## What gets synced

**Tables.** The tables in `tables`, or every base table in `INFORMATION_SCHEMA.TABLES` when you omit it. Views are not included in discovery. Skipprd reads each table with one `SELECT *` and holds the result in memory before writing it, so size the machine for your largest table.

**Table names.** Each source table lands in a destination table named after the table alone, without the schema: `dbo.customers` becomes `customers`. Skipprd lowercases names and replaces characters other than letters, digits, and `_` with `_`. Tables with the same name in different schemas land in the same destination table, so split them across pipelines if you need them apart.

**Types.**

| SQL Server type | Arrives as |
|---|---|
| `tinyint`, `smallint`, `int`, `bigint`, `real`, `float` | number |
| `decimal`, `numeric` | string with the column's scale, for example `"120.50"` |
| `money`, `smallmoney` | string with four decimal places, for example `"9.9900"` |
| `bit` | boolean |
| `char`, `varchar`, `nchar`, `nvarchar`, `text`, `ntext`, `xml` | string |
| `uniqueidentifier` | string |
| `date` | string, `YYYY-MM-DD` |
| `time` | string, `HH:MM:SS` with fractional seconds |
| `datetime`, `datetime2`, `smalldatetime` | string, `YYYY-MM-DDTHH:MM:SS` with fractional seconds, no time zone |
| `datetimeoffset` | string in RFC 3339 with the offset, for example `2025-01-03T09:15:00+01:00` |
| `binary`, `varbinary`, `image` | array of byte values |

Decimal values arrive as strings so no precision is lost. Run `skipprd discover` to infer column types; see [Schema discovery and evolution](/concepts/schema).

**Incremental behaviour.** Skipprd records each table as loaded once its rows are durably written. Later runs skip it, even if rows changed. To reload the database, run the source under a new pipeline name, which starts with fresh progress.

**Deletes.** Not captured.

## Troubleshooting

Run sync with `--log` to see these messages. A connection failure is logged, and the run ends without loading anything.

| Symptom | Cause | Fix |
|---|---|---|
| `Failed to connect to MSSQL` in the log | Wrong server, port, or credentials, or no network path. | Check the connection string and that port `1433` (or yours) is reachable from the machine running Skipprd. |
| `Login failed for user` | Wrong login or password, or SQL authentication is disabled. | Check the login, and enable **SQL Server and Windows Authentication mode** on the server. |
| The connection is refused during login with an encryption error | The server forces encryption, or it is Azure SQL Database. | Turn off **Force Encryption** for this server on a private network. Encrypted connections are not supported by this source. |
| `Failed to query table ...` in the log, and that table is missing | The name is wrong or the login lacks `SELECT`. | Fix the entry in `tables`, or add the login to `db_datareader`. Other tables still load. |
| Two source tables end up in one destination table | They share a name in different schemas. | Put each schema in its own pipeline. |
| Changed rows never arrive | Each table loads once. | Run the source under a new pipeline name to take a fresh copy. |

## Next steps

- [Connector catalog](/connectors/) — pick a destination
- [skippr.yml](/configuration/skippr-yml) — the full project file, with a SQL Server to Snowflake example
- [skipprd sync](/cli/sync) — run once or on a schedule
