# Skipprd sync

Ingest data from the source, buffer through the WAL, compact into Parquet, and upload to the destination.

## Usage

```bash
skipprd sync --pipeline <name> [--once] [--output <mode>] [--log [LEVEL]]
skipprd --config skippr.yml sync --pipeline <name> [--once] [--output <mode>] [--log [LEVEL]]
```

## Flags

| Flag | Required | Description |
|---|---|---|
| `--pipeline, -p` | No | Pipeline name. Falls back to `PIPELINE_NAME` env var. |
| `--once` | No | Run a single sync pass across all pipelines and exit. Without this flag, multi-pipeline mode loops continuously. |
| `--output` | No | Output mode: `progress` (default, interactive spinner), `json` (structured JSON lines to stdout), or `text` (plain text summaries). |
| `--log` | No | Enable logging. Optional level: `debug`, `info`, `warn`, `error`. Defaults to `info` when flag is present. |

## What it does

1. Loads pipeline metadata (schema) from S3
2. Syncs the schema to the destination (creates Glue database/tables if needed)
3. Reads data from the source in batches
4. Writes records to the WAL as segments
5. The compactor service continuously reads segments, converts to Parquet, uploads to S3, and registers Glue partitions
6. On completion, the compactor drains all remaining segments before exit
7. Commits offsets to the offsets database

## Example

```bash
skipprd --config skippr.yml sync --pipeline events --log
```

### Batch sync with structured output

```bash
skipprd sync --pipeline el_mssql --once --output json
```
