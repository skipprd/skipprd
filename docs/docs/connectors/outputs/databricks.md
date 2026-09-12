# Databricks / Delta Lake Output

Writes data to Databricks or any Delta Lake table. Supports two modes:

- **Delta Lake mode** (when `delta_table_uri` is set): writes Arrow batches directly to a Delta table at any object store URI (S3, Azure, GCS, local filesystem). No Databricks workspace required.
- **COPY mode** (default): uploads Parquet to Databricks Volumes via the Files API, then optionally executes `COPY INTO` via the SQL Statement API.

## How it works

### Delta Lake mode

1. Collects Arrow RecordBatches from the stream.
2. Opens (or creates) the Delta table at the configured URI.
3. Appends batches using the `deltalake` write operation.

### COPY mode

1. Serializes record batches to Parquet.
2. Uploads the Parquet file to Databricks Volumes via the Files API.
3. Optionally executes a `COPY INTO` SQL statement via the SQL Statement API.

## Configuration (Delta Lake mode)

```yaml
data_sinks:
  sink:
    Databricks:
      delta_table_uri: "s3://my-bucket/delta-table"
      storage_options:
        AWS_REGION: us-east-1
```

## Configuration (COPY mode)

```yaml
data_sinks:
  sink:
    Databricks:
      workspace_url: "https://my-workspace.cloud.databricks.com"
      token: "dapi..."
      warehouse_id: "abc123"
      catalog: main
      schema: default
      table: events
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `delta_table_uri` | | Delta table URI. When set, enables Delta Lake mode (`s3://`, `az://`, `gs://`, `file:///`) |
| `storage_options` | | Key-value map for object store auth (e.g. `AWS_REGION`, `AWS_ACCESS_KEY_ID`) |
| `workspace_url` | | Databricks workspace URL (COPY mode) |
| `token` | | Personal access token (COPY mode) |
| `warehouse_id` | | SQL warehouse ID (enables COPY INTO in COPY mode) |
| `catalog` | `main` | Unity Catalog name (COPY mode) |
| `schema` | `default` | Schema name (COPY mode) |
| `table` | `data` | Target table name (COPY mode) |
| `format` | `parquet` | Output format |

## Authentication

Configure `workspace_url`, `token`, and optional `warehouse_id` directly when you connect the warehouse or in `skippr.yml`.

For security best practices, we strongly advise against storing the token in `skippr.yml`. Use environment variable interpolation instead: replace the `token` value with your own `${ENV_VAR}` reference.

The relevant part of `skippr.yml` looks like this:

```yaml
data_sinks:
  warehouse:
    Databricks:
      token: "${DATABRICKS_TOKEN}"
```

Set the env var before running `skipprd`:

macOS / Linux

```bash
export DATABRICKS_TOKEN="dapi..."
```

Windows PowerShell

```powershell
$env:DATABRICKS_TOKEN = "dapi..."
```

Windows Command Prompt

```cmd
set DATABRICKS_TOKEN=dapi...
```

## Troubleshooting

| Symptom | Fix |
|---|---|
| authentication failed | Verify the workspace URL, token, and any SQL warehouse ID values. |
| writes or COPY INTO fail | Check catalog and schema permissions, warehouse availability, and whether the token can use that SQL warehouse. |
