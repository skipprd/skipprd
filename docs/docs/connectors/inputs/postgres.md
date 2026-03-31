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
