# Postgres Input

Reads rows from PostgreSQL tables and converts them to JSON.

## How it works

1. Connects to PostgreSQL using the provided credentials.
2. Queries specified tables or executes a custom SQL query.
3. If no tables or query specified, reads all public tables.
4. Rows are serialized to JSON.
5. Namespace convention: `postgres.{table_name}`.

## Configuration

```yaml
data_sources:
  source:
    Postgres:
      host: localhost
      port: 5432
      user: postgres
      password: secret
      database: mydb
      tables: ["users", "orders"]
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `host` | `localhost` | Postgres host |
| `port` | `5432` | Postgres port |
| `user` | `postgres` | Username |
| `password` | | Password |
| `database` | | Database name |
| `connection_string` | | Full connection string (overrides host/port/user/password/database) |
| `tables` | | List of tables to read |
| `query` | | Custom SQL query (overrides tables) |
| `batch_size_rows` | `10000` | Rows per ingest batch |
| `format` | `json` | Data format |

## Authentication

Configure `host`, `port`, `user`, `password`, and `database` directly in the source config, or use `connection_string` if you prefer a single DSN-style value.

For security best practices, we strongly advise against storing the password or connection string in `skippr.yml`. Use environment variable interpolation instead: replace the `password` or `connection_string` value with your own `${ENV_VAR}` reference.

The relevant part of `skippr.yml` looks like this:

```yaml
data_sources:
  source:
    Postgres:
      password: "${POSTGRES_SOURCE_PASSWORD}"
```

Set the env var before running `skipprd`:

macOS / Linux

```bash
export POSTGRES_SOURCE_PASSWORD="mypassword"
```

Windows PowerShell

```powershell
$env:POSTGRES_SOURCE_PASSWORD = "mypassword"
```

Windows Command Prompt

```cmd
set POSTGRES_SOURCE_PASSWORD=mypassword
```

## Troubleshooting

| Symptom | Fix |
|---|---|
| `connection refused` | Check the configured host and port are correct and the server is running |
| `password authentication failed` | Verify the configured username and password |
| SSL errors | Check the connection string or SSL-related connection parameters for the target server |
