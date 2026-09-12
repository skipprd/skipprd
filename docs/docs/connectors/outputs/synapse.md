# Azure Synapse Output

Writes data to Azure Synapse Analytics (or SQL Server) via TDS protocol.

## How it works

1. Connects using a TDS/ADO connection string.
2. Inserts rows via SQL INSERT statements.

## Configuration

```yaml
data_sinks:
  sink:
    Synapse:
      connection_string: "Server=myserver.database.windows.net;User Id=admin;Password=secret;Database=mydb"
      schema: dbo
      table: events
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `connection_string` | *(required)* | ADO-style connection string |
| `schema` | `dbo` | Target schema |
| `table` | `data` | Target table |
| `format` | `json` | Data format |

## Authentication

Configure the `connection_string` field in `skippr.yml`. Use `${SYNAPSE_CONNECTION_STRING}` rather than a literal secret.

For security best practices, we strongly advise against storing the connection string in `skippr.yml`. Use environment variable interpolation instead: replace the `connection_string` value with your own `${ENV_VAR}` reference.

The relevant part of `skippr.yml` looks like this:

```yaml
data_sinks:
  warehouse:
    Synapse:
      connection_string: "${SYNAPSE_CONNECTION_STRING}"
```

Set the env var before running `skipprd`:

macOS / Linux

```bash
export SYNAPSE_CONNECTION_STRING="Server=myserver.database.windows.net;User Id=admin;Password=secret;Database=mydb"
```

Windows PowerShell

```powershell
$env:SYNAPSE_CONNECTION_STRING = "Server=myserver.database.windows.net;User Id=admin;Password=secret;Database=mydb"
```

Windows Command Prompt

```cmd
set SYNAPSE_CONNECTION_STRING=Server=myserver.database.windows.net;User Id=admin;Password=secret;Database=mydb
```

## Troubleshooting

| Symptom | Fix |
|---|---|
| authentication failed | Verify the configured connection string and confirm the login can reach the database. |
| schema or table errors | Check the target schema name and that the login can create and write objects there. |
