# Google Cloud Storage Output

Writes Parquet files to a GCS bucket.

## How it works

1. Serializes record batches to Parquet.
2. Uploads to the configured bucket with optional prefix, namespace, and time partitioning.

## Configuration

```yaml
data_sinks:
  sink:
    Gcs:
      bucket: my-bucket
      prefix: "data/"
      service_account_key_path: "/path/to/key.json"
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `bucket` | *(required)* | GCS bucket name |
| `prefix` | | Key prefix for uploaded objects |
| `service_account_key_path` | | Path to service account JSON key |
| `format` | `parquet` | Output format |
