# Skipprd sync

Ingest data from the source into the WAL. If the pipeline has a `data_sink`, compact into Parquet and upload to the destination. Without a sink, the WAL is the dataset — skipprd does not compact or reclaim.

## Usage

```bash
skipprd sync --pipeline <name> [--once] [--output <mode>] [--log [LEVEL]]
skipprd --config skippr.yml sync --pipeline <name> [--once] [--output <mode>] [--log [LEVEL]]
```

## Flags

| Flag | Required | Description |
|---|---|---|
| `--pipeline, -p` | No | Pipeline name. Falls back to `PIPELINE_NAME`. |
| `--once` | No | Run a single sync pass across all pipelines and exit. Without this flag, multi-pipeline mode loops continuously. |
| `--output` | No | Output mode: `progress` (default, interactive spinner), `json` (structured JSON lines to stdout), or `text` (plain text summaries). |
| `--log` | No | Enable logging. Optional level: `debug`, `info`, `warn`, `error`. Defaults to `info` when flag is present. |

## What it does

1. Loads pipeline metadata (schema) from S3
2. Syncs the schema to the destination when a sink is configured
3. Reads data from the source in batches
4. Writes records to the WAL as segments
5. If a `data_sink` is set, the compactor reads segments, converts to Parquet, and lands them. Without a sink this step is skipped.
6. On completion with a sink, the compactor drains remaining segments before exit
7. Commits offsets to the offsets database

## Example

```bash
skipprd --config skippr.yml sync --pipeline events --log
```

### Batch sync with structured output

```bash
skipprd sync --pipeline el_mssql --once --output json
```
