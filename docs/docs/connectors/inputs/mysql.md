# MySQL Input

Reads data from MySQL tables via the `mysql_async` driver.

## Supported formats

- Row-based JSON (each row serialized as a JSON object)

## How it works

1. Connects using the configured connection string.
2. When no explicit table list is provided, discovers all base tables from `INFORMATION_SCHEMA.TABLES`.
3. For each table, runs `SELECT *` and converts each row to a JSON object.
4. Batches are ingested through the standard WAL pipeline, with offset tracking per table.
5. Namespace convention: `mysql.{database}.{schema}.{table}`.

## Configuration

```bash
DATA_SOURCE_PLUGIN_NAME=Mysql
MYSQL_CONNECTION_STRING="mysql://user:pass@localhost:3306/mydb"
```

Or via YAML pipeline config:

```yaml
data_sources:
  source:
    Mysql:
      connection_string: "mysql://user:pass@localhost:3306/mydb"
      tables:
        - "myschema.customers"
        - "myschema.orders"
      batch_size_rows: 10000
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `MYSQL_CONNECTION_STRING` | *(required)* | MySQL connection string |
| `tables` | *(auto-discover)* | Optional list of `schema.table` names to ingest. When omitted, all base tables are discovered. |
| `batch_size_rows` | `10000` | Number of rows per ingest batch |
| `batch_size_bytes` | | Override WAL segment size in bytes |
| `batch_size_seconds` | | Override WAL segment flush interval in seconds |

## Namespace convention

Each table produces a namespace using the pattern:

```
mysql.{database}.{schema}.{table}
```

## Type mapping

| MySQL Type | Skipprd Type |
|---|---|
| `varchar`, `text` | String |
| `int`, `smallint`, `tinyint` | Integer |
| `bigint` | Long |
| `float`, `double`, `decimal` | Double |
| `tinyint(1)` | Boolean |
| `date` | Date |
| `datetime`, `timestamp` | Timestamp |

## Offset tracking

Each table is tracked as a single offset unit. Once a table has been fully ingested, subsequent `--once` runs will skip it unless offsets are reset.

## Authentication

Use a MySQL connection string. Prefer an environment variable so credentials do not live in `skippr.yml`.

```bash
export MYSQL_CONNECTION_STRING="mysql://user:pass@host:3306/db"
```

## Troubleshooting

| Symptom | Fix |
|---|---|
| authentication or connection errors | Verify `MYSQL_CONNECTION_STRING`, host reachability, and the selected database name. |
| missing tables | Check the `--tables` list or omit it to let Skipprd discover all readable tables. |
