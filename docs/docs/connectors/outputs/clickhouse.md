# ClickHouse Output

Writes data to ClickHouse tables via the HTTP API using `INSERT FORMAT JSONEachRow`.

## How it works

1. Receives Arrow RecordBatch streams.
2. Auto-creates tables with `CREATE TABLE IF NOT EXISTS` using Nullable MergeTree columns.
3. Converts each batch row to JSON and inserts via the ClickHouse HTTP bulk insert endpoint.
4. Table name defaults to the namespace (dots replaced with underscores) or can be overridden.

## Configuration

```yaml
data_sinks:
  sink:
    Clickhouse:
      url: "http://localhost:8123"
      database: default
      user: default
      password: secret
      table: my_table
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `url` | | ClickHouse HTTP endpoint |
| `database` | | Database name |
| `user` | | Username |
| `password` | | Password |
| `table` | (from namespace) | Target table name |
| `format` | `json` | Data format |
