# DuckDB / MotherDuck Input

Reads rows from DuckDB (local or MotherDuck cloud) tables and converts them to JSON.

## How it works

1. Opens a DuckDB connection using the provided connection string.
2. If a MotherDuck token is provided, authenticates via `SET motherduck_token`.
3. Queries specified tables or executes a custom SQL query.
4. Rows are serialized to JSON via the sync DuckDB API (wrapped in `spawn_blocking`).
5. Namespace convention: `duckdb.{database}.{table_name}`.

## Configuration

```yaml
data_sources:
  source:
    Duckdb:
      connection_string: "md:my_database"
      motherduck_token: "ey..."
      tables: ["users", "events"]
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `connection_string` | `:memory:` | DuckDB path or `md:` for MotherDuck |
| `motherduck_token` | | MotherDuck auth token |
| `database` | | Database to `USE` after connecting |
| `tables` | | List of tables to read |
| `query` | | Custom SQL query (overrides tables) |
| `batch_size_rows` | `10000` | Rows per ingest batch |
| `format` | `json` | Data format |
