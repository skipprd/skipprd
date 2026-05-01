# skipprd discover

Connect to the data source, sample records, and infer the pipeline schema. Unlike `sync`, discover never writes to the output destination -- it only discovers schemas and persists metadata.

## Usage

```bash
skipprd discover --pipeline <name> [--output <mode>] [--log [LEVEL]]
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
3. Connects to the data source configured by `DATA_SOURCE_PLUGIN_NAME`
4. Samples records and infers the complete nested schema via type inference
5. Persists the updated pipeline metadata
6. Exits

Discover does **not** initialize or sync to any output plugin. It is purely a schema inference operation.

## Example

```bash
DATA_SOURCE_PLUGIN_NAME=s3 \
DATA_SOURCE_S3_BUCKET=my-source-bucket \
DATA_SOURCE_S3_PREFIX=events/ \
SKIPPR_S3_BUCKET=my-state-bucket \
skipprd discover --pipeline events --log
```

### Structured output for programmatic use

```bash
skipprd discover --pipeline el_mssql --output json
```

This emits JSON events to stdout:

```json
{"event":"discover_start","pipeline":"el_mssql","timestamp":"2026-03-18T12:00:00Z"}
{"event":"namespace_discovered","namespace":"mssql.MyDB.dbo.customers","fields":[{"name":"id","type":"Long"},{"name":"email","type":"String"}],"timestamp":"..."}
{"event":"namespace_discovered","namespace":"mssql.MyDB.dbo.orders","fields":[{"name":"order_id","type":"Long"},{"name":"total","type":"Double"}],"timestamp":"..."}
{"event":"discover_complete","pipeline":"el_mssql","namespaces_discovered":2,"elapsed_ms":12000,"timestamp":"..."}
```

The `fields` array uses `SkipprDataType` names (`String`, `Long`, `Double`, `Boolean`, `Date`, `Timestamp`, etc.) representing the inferred source types.

## Reading discovered schemas

After `skipprd discover` completes, use [`SHOW PIPELINE`](../sql/reference.md#show-pipeline) to retrieve the full discovered schema including field names and inferred types:

```bash
skipprd query --sql "SHOW PIPELINE el_mssql" --plain
```

## Key log events

- `Discovered new namespace: <name>` -- a new event type/schema was found
- `Discovered new field: <field>` -- a new field was added to the schema
- `Updated pipeline metadata` -- schema persisted

## Notes

- Discovery must be run before the first `sync` to establish the pipeline schema.
- Re-running discover updates the schema if the source data has changed.
- Discovery does not ingest or move data -- it only reads a sample and infers types.
