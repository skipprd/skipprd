# DuckDB / MotherDuck Output

Writes data to DuckDB (local or MotherDuck cloud) tables via SQL INSERT.

## How it works

1. Opens a DuckDB connection using the provided connection string.
2. If a MotherDuck token is provided, authenticates via `SET motherduck_token`.
3. Auto-creates tables with `CREATE TABLE IF NOT EXISTS`.
4. Inserts rows in batches of 1000 via SQL INSERT statements.
5. Sync operations use `spawn_blocking` since DuckDB's Rust API is synchronous.

## Configuration

```yaml
data_sinks:
  sink:
    Duckdb:
      connection_string: "md:my_database"
      motherduck_token: "ey..."
      table: events
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `connection_string` | | DuckDB path or `md:` for MotherDuck |
| `motherduck_token` | | MotherDuck auth token |
| `database` | | Database to `USE` after connecting |
| `table` | (from namespace) | Target table name |
| `format` | `json` | Data format |
