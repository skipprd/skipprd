# skippr discover

Connect to the data source, sample records, and infer the pipeline schema.

## Usage

```bash
skippr discover --pipeline <name> [--log [LEVEL]] [--verbose]
```

## Flags

| Flag | Required | Description |
|---|---|---|
| `--pipeline, -p` | No | Pipeline name. Falls back to `PIPELINE_NAME` env var. |
| `--log` | No | Enable logging. Optional level: `debug`, `info`, `warn`, `error`. Defaults to `info` when flag is present. |
| `--verbose` | No | Stream verbose logs to stdout instead of showing a progress bar. |

## What it does

1. Reads configuration from environment variables (and optional config file)
2. Connects to the data source configured by `DATA_SOURCE_PLUGIN_NAME`
3. Samples records and infers the complete nested schema
4. Persists the schema as pipeline metadata to `SKIPPR_S3_BUCKET`

## Example

```bash
DATA_SOURCE_PLUGIN_NAME=s3 \
DATA_SOURCE_S3_BUCKET=my-source-bucket \
DATA_SOURCE_S3_PREFIX=events/ \
SKIPPR_S3_BUCKET=my-state-bucket \
skippr discover --pipeline events --log
```

## Key log events

- `Discovered new namespace: <name>` — a new event type/schema was found
- `Discovered new field: <field>` — a new field was added to the schema
- `Updated pipeline metadata in S3` — schema persisted

## Notes

- Discovery must be run before the first `sync` to establish the pipeline schema.
- Re-running discover updates the schema if the source data has changed.
- Discovery does not ingest or move data — it only reads a sample.
