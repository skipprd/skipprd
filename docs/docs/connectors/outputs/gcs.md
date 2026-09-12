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

## Authentication

Authenticate with a Google Cloud service account key. For security best practices, we strongly advise against storing the service account key path in `skippr.yml`. Use environment variable interpolation instead: replace the `service_account_key_path` value with your own `${ENV_VAR}` reference.

The relevant part of `skippr.yml` looks like this:

```yaml
data_sinks:
  warehouse:
    Gcs:
      service_account_key_path: "${GCS_SERVICE_ACCOUNT_KEY_PATH}"
```

Set the env var before running `skipprd`:

macOS / Linux

```bash
export GCS_SERVICE_ACCOUNT_KEY_PATH="/path/to/key.json"
```

Windows PowerShell

```powershell
$env:GCS_SERVICE_ACCOUNT_KEY_PATH = "C:\path\to\key.json"
```

Windows Command Prompt

```cmd
set GCS_SERVICE_ACCOUNT_KEY_PATH=C:\path\to\key.json
```

## Troubleshooting

| Symptom | Fix |
|---|---|
| authentication failed | Verify the service account key path and confirm the key still belongs to an active service account. |
| writes fail | Check bucket permissions, object prefix settings, and any organization policies affecting the bucket. |
