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
