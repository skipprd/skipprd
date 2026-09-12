# Delta Lake Input

Reads data from a Delta Lake table at any object store URI (S3, Azure, GCS, or local filesystem).

## How it works

1. Opens the Delta table at the configured URI using the `deltalake` crate.
2. Registers the table in a DataFusion session for SQL querying.
3. Optionally applies a filter predicate for partition/row pushdown.
4. Converts each Arrow RecordBatch row to a JSON record.
5. Namespace convention: `delta_lake.{table_uri}`.

## Configuration

```yaml
data_sources:
  source:
    DeltaLake:
      table_uri: "s3://my-bucket/delta-table"
      storage_options:
        AWS_REGION: us-east-1
      filter: "date > '2024-01-01'"
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `table_uri` | | Delta table URI (`s3://`, `az://`, `gs://`, `file:///`) |
| `storage_options` | | Key-value map for object store auth (e.g. `AWS_REGION`, `AWS_ACCESS_KEY_ID`) |
| `version` | | Specific Delta table version to read |
| `filter` | | SQL WHERE clause for pushdown filtering |
| `batch_size_rows` | `10000` | Rows per ingest batch |
| `format` | `json` | Data format |

## Authentication

Authentication depends on the storage backend referenced by `table_uri`. For security best practices, we strongly advise against storing storage credentials in `skippr.yml`. Use environment variable interpolation instead: replace the relevant `storage_options` value with your own `${ENV_VAR}` reference.

The relevant part of `skippr.yml` looks like this:

```yaml
data_sources:
  source:
    DeltaLake:
      table_uri: "s3://bucket/path/to/table"
      storage_options:
        AWS_ACCESS_KEY_ID: "${DELTA_AWS_ACCESS_KEY_ID}"
        AWS_SECRET_ACCESS_KEY: "${DELTA_AWS_SECRET_ACCESS_KEY}"
```

Set the env vars before running `skipprd`:

macOS / Linux

```bash
export DELTA_AWS_ACCESS_KEY_ID="AKIA..."
export DELTA_AWS_SECRET_ACCESS_KEY="secret"
```

Windows PowerShell

```powershell
$env:DELTA_AWS_ACCESS_KEY_ID = "AKIA..."
$env:DELTA_AWS_SECRET_ACCESS_KEY = "secret"
```

Windows Command Prompt

```cmd
set DELTA_AWS_ACCESS_KEY_ID=AKIA...
set DELTA_AWS_SECRET_ACCESS_KEY=secret
```

## Troubleshooting

| Symptom | Fix |
|---|---|
| table cannot be opened | Verify `table_uri`, backend credentials, and that the Delta log is present at that path. |
| schema inference looks wrong | Check the selected table version and any optional filter expression. |
