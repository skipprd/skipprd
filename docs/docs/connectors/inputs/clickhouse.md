# ClickHouse Input

Reads rows from ClickHouse tables via the HTTP API and converts them to JSON.

## How it works

1. Connects to ClickHouse using the HTTP interface.
2. Queries specified tables or executes a custom SQL query with `FORMAT JSONEachRow`.
3. Each row is emitted as a JSON record.
4. Namespace convention: `clickhouse.{database}.{table_name}`.

## Configuration

```yaml
data_sources:
  source:
    Clickhouse:
      url: "http://localhost:8123"
      database: default
      user: default
      password: secret
      tables: ["events", "metrics"]
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `url` | `http://localhost:8123` | ClickHouse HTTP endpoint |
| `database` | `default` | Database name |
| `user` | `default` | Username |
| `password` | | Password |
| `tables` | | List of tables to read |
| `query` | | Custom SQL query (overrides tables) |
| `batch_size_rows` | `10000` | Rows per ingest batch |
| `format` | `json` | Data format |
