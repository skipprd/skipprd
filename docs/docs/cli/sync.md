# skippr sync

Ingest data from the source, buffer through the WAL, compact into Parquet, and upload to the destination.

## Usage

```bash
skippr sync --pipeline <name> [--log [LEVEL]]
```

## Flags

| Flag | Required | Description |
|---|---|---|
| `--pipeline, -p` | No | Pipeline name. Falls back to `PIPELINE_NAME` env var. |
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
DATA_SOURCE_PLUGIN_NAME=s3 \
DATA_SOURCE_S3_BUCKET=my-source-bucket \
DATA_SOURCE_S3_PREFIX=events/ \
DATA_OUTPUT_S3_BUCKET=my-output-bucket \
DATA_OUTPUT_S3_PREFIX=warehouse/events \
SCHEMA_OUTPUT_GLUE_DATABASE_NAME=my_database \
SKIPPR_S3_BUCKET=my-state-bucket \
PIPELINE_NAME=events \
skippr sync --log
```

## Key log events

| Log pattern | Meaning |
|---|---|
| `Syncing pipeline: <name>` | Run started |
| `Starting stream pipeline (...)` | Ingest worker active |
| `Uploaded ...parquet to S3 (rows=..., bytes=...)` | Data written to destination |
| `Finalising: compactor drained and stopped` | Clean shutdown |
| `Compactor: summary uploaded_rows=X expected_msgs=Y` | Integrity check (expect X == Y) |
| `Pipeline sync complete` | Run finished successfully |

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Clean completion |
| Non-zero | Compactor drain failed or runtime error — treat the run as untrusted |
