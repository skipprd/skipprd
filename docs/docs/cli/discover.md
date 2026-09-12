# Skipprd discover

Connect to the data source, sample records, and infer the pipeline schema. Unlike `sync`, discover never writes to the output destination -- it only discovers schemas and persists metadata.

## Usage

```bash
skipprd discover --pipeline <name> [--output <mode>] [--log [LEVEL]]
skipprd --config skippr.yml discover --pipeline <name> [--output <mode>] [--log [LEVEL]]
```

## Flags

| Flag | Required | Description |
|---|---|---|
| `--pipeline, -p` | No | Pipeline name. Falls back to `PIPELINE_NAME` env var. |
| `--output` | No | Output mode: `progress` (default, interactive spinner), `json` (structured JSON lines to stdout), or `text` (plain text summaries). |
| `--log` | No | Enable logging. Optional level: `debug`, `info`, `warn`, `error`. Defaults to `info` when flag is present. |

## What it does

1. Loads or creates pipeline metadata (from S3 or local disk, depending on `SKIPPRD_EL_STORAGE_MODE`)
2. Initializes the offset database
3. Connects to the data source configured in `skippr.yml`
4. Samples records and infers the complete nested schema via type inference
5. Persists the updated pipeline metadata
6. Exits

Discover does **not** initialize or sync to any output plugin. It is purely a schema inference operation.

## Example

```bash
skipprd --config skippr.yml discover --pipeline events --log
```

### Structured output for programmatic use

```bash
skipprd discover --pipeline el_mssql --output json
```
