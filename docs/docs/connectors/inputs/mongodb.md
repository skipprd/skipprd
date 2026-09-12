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

## Authentication

Use a MongoDB connection string. For security best practices, we strongly advise against storing the connection string in `skippr.yml`. Use environment variable interpolation instead: replace the `connection_string` value with your own `${ENV_VAR}` reference.

The relevant part of `skippr.yml` looks like this:

```yaml
data_sources:
  source:
    Mongodb:
      connection_string: "${MONGODB_CONNECTION_STRING}"
```

Set the env var before running `skipprd`:

macOS / Linux

```bash
export MONGODB_CONNECTION_STRING="mongodb://user:pass@host:27017"
```

Windows PowerShell

```powershell
$env:MONGODB_CONNECTION_STRING = "mongodb://user:pass@host:27017"
```

Windows Command Prompt

```cmd
set MONGODB_CONNECTION_STRING=mongodb://user:pass@host:27017
```

## Troubleshooting

| Symptom | Fix |
|---|---|
| authentication failed | Verify the MongoDB connection string, credentials, and authentication database. |
| no documents returned | Check the selected database, collection, and optional filter document. |
