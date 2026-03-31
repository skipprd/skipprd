# MotherDuck Input

Reads rows from MotherDuck cloud tables via the MotherDuck REST API.

## How it works

1. Authenticates with MotherDuck using a bearer token.
2. Queries specified tables or executes a custom SQL query via `POST https://api.motherduck.com/v1/sql`.
3. Response rows are serialized to JSON and ingested through the standard WAL pipeline.
4. Namespace convention: `motherduck.{database}.{table_name}`.

## Configuration

```yaml
data_sources:
  source:
    Motherduck:
      motherduck_token: "ey..."
      database: "my_database"
      tables: ["users", "events"]
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `motherduck_token` / `MOTHERDUCK_TOKEN` | *(required)* | MotherDuck auth token |
| `database` | | MotherDuck database name |
| `tables` | | List of tables to read |
| `query` | | Custom SQL query (overrides tables) |
| `batch_size_rows` | `10000` | Rows per ingest batch |
| `format` | `json` | Data format |
