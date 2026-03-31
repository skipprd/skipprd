# MongoDB Input

Reads documents from a MongoDB collection, converting BSON to JSON.

## How it works

1. Connects to MongoDB using the provided connection string.
2. Executes a `find()` query with an optional JSON filter.
3. Each BSON document is serialized to JSON before ingest.
4. Namespace convention: `mongodb.{database}.{collection}`.

## Configuration

```yaml
data_sources:
  source:
    Mongodb:
      connection_string: "mongodb://localhost:27017"
      database: mydb
      collection: events
      filter: '{"status": "active"}'
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `connection_string` | *(required)* | MongoDB connection URI |
| `database` | *(required)* | Database name |
| `collection` | *(required)* | Collection name |
| `filter` | | Optional JSON filter document |
| `batch_size_rows` | | Rows per batch |
| `format` | `json` | Data format |
