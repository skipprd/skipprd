# MotherDuck Output

Writes data to MotherDuck cloud tables via the MotherDuck REST API.

## How it works

1. Authenticates with MotherDuck using a bearer token.
2. Auto-creates tables with `CREATE TABLE IF NOT EXISTS` via the SQL endpoint.
3. Inserts rows in batches of 1000 via SQL INSERT statements.
4. All operations use `POST https://api.motherduck.com/v1/sql`.

## Configuration

```yaml
data_sinks:
  sink:
    Motherduck:
      motherduck_token: "ey..."
      database: "my_database"
      table: events
      schema: main
```

`schema` is an optional query/model key; ingest uses `table` for writes.

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `motherduck_token` | *(required)* | MotherDuck auth token |
| `database` | | MotherDuck database name |
| `table` | (from namespace) | Target table name |
| `format` | `json` | Data format |

## Authentication

Configure `motherduck_token` directly when you connect the warehouse or in `skippr.yml`.

For security best practices, we strongly advise against storing the token in `skippr.yml`. Use environment variable interpolation instead: replace the `motherduck_token` value with your own `${ENV_VAR}` reference.

The relevant part of `skippr.yml` looks like this:

```yaml
data_sinks:
  warehouse:
    Motherduck:
      motherduck_token: "${MOTHERDUCK_TOKEN}"
```

Set the env var before running `skipprd`:

macOS / Linux

```bash
export MOTHERDUCK_TOKEN="md:..."
```

Windows PowerShell

```powershell
$env:MOTHERDUCK_TOKEN = "md:..."
```

Windows Command Prompt

```cmd
set MOTHERDUCK_TOKEN=md:...
```

## Troubleshooting

| Symptom | Fix |
|---|---|
| authentication failed | Verify the configured MotherDuck token and confirm it still has access to the selected database. |
| schema errors | Check the database and schema names and confirm the token can create or write objects there. |
