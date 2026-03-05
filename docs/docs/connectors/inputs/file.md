# File Input

Reads data from the local filesystem.

## Supported formats

- JSON
- CSV / delimited
- Parquet

## Configuration

```bash
DATA_SOURCE_PLUGIN_NAME=file
DATA_SOURCE_PATH=/path/to/input-directory
```

| Variable | Default | Description |
|---|---|---|
| `DATA_SOURCE_PATH` | *(required)* | Path to the directory or file to ingest |

## Checkpointing

The file input tracks progress by file path and line offset. On restart, already-ingested files and lines are skipped.
