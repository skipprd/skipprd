# ClickHouse Input

Reads rows from ClickHouse tables via the HTTP API and converts them to JSON.

## How it works

1. Connects to ClickHouse using the HTTP interface.
2. Queries specified tables or executes a custom SQL query with `FORMAT JSONEachRow`.
3. Each row is emitted as a JSON record.
4. Namespace convention: `clickhouse.{database}.{table_name}`.

## Configuration

```yaml
data_sources:
  source:
    Clickhouse:
      url: "http://localhost:8123"
      database: default
      user: default
      password: secret
      tables: ["events", "metrics"]
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `url` | `http://localhost:8123` | ClickHouse HTTP endpoint |
| `database` | `default` | Database name |
| `user` | `default` | Username |
| `password` | | Password |
| `tables` | | List of tables to read |
| `query` | | Custom SQL query (overrides tables) |
| `batch_size_rows` | `10000` | Rows per ingest batch |
| `format` | `json` | Data format |

## Authentication

Use the configured ClickHouse user and password, or rely on the default local user for development. For security best practices, we strongly advise against storing the password in `skippr.yml`. Use environment variable interpolation instead: replace the `password` value with your own `${ENV_VAR}` reference.

The relevant part of `skippr.yml` looks like this:

```yaml
data_sources:
  source:
    Clickhouse:
      password: "${CLICKHOUSE_PASSWORD}"
```

Set the env var before running `skipprd`:

macOS / Linux

```bash
export CLICKHOUSE_PASSWORD="secret"
```

Windows PowerShell

```powershell
$env:CLICKHOUSE_PASSWORD = "secret"
```

Windows Command Prompt

```cmd
set CLICKHOUSE_PASSWORD=secret
```

## Troubleshooting

| Symptom | Fix |
|---|---|
| authentication failed | Verify the ClickHouse user, password, and database name. |
| connection refused | Check the HTTP URL, port, and network access to the ClickHouse endpoint. |
