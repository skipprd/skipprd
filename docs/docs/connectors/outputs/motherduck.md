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
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `motherduck_token` | *(required)* | MotherDuck auth token |
| `database` | | MotherDuck database name |
| `table` | (from namespace) | Target table name |
| `format` | `json` | Data format |
