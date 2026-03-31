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
