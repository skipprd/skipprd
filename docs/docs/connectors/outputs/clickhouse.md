# ClickHouse Output

Writes data to ClickHouse tables via the HTTP API using `INSERT FORMAT JSONEachRow`.

## How it works

1. Receives Arrow RecordBatch streams.
2. Auto-creates tables with `CREATE TABLE IF NOT EXISTS` using Nullable MergeTree columns.
3. Converts each batch row to JSON and inserts via the ClickHouse HTTP bulk insert endpoint.
4. Table name defaults to the namespace (dots replaced with underscores) or can be overridden.

## Configuration

```yaml
data_sinks:
  sink:
    Clickhouse:
      url: "http://localhost:8123"
      database: default
      user: default
      password: secret
      table: my_table
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `url` | | ClickHouse HTTP endpoint |
| `database` | | Database name |
| `user` | | Username |
| `password` | | Password |
| `table` | (from namespace) | Target table name |
| `format` | `json` | Data format |

## Authentication

Use the configured ClickHouse user and password, or rely on the default local user for development. For security best practices, we strongly advise against storing the password in `skippr.yml`. Use environment variable interpolation instead: replace the `password` value with your own `${ENV_VAR}` reference.

The relevant part of `skippr.yml` looks like this:

```yaml
data_sinks:
  warehouse:
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
| authentication failed | Verify the ClickHouse user, password, URL, and database name. |
| writes succeed but deduped state looks delayed | This destination relies on ReplacingMergeTree merges. Use `FINAL` for point-in-time correctness when querying fresh data. |
