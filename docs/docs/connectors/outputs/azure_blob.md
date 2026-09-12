# Azure Blob Storage Output

Writes Parquet files to Azure Blob Storage.

## How it works

1. Serializes record batches to Parquet.
2. Uploads to the configured container with optional prefix, namespace, and time partitioning.

## Configuration

```yaml
data_sinks:
  sink:
    AzureBlob:
      account_name: mystorageaccount
      account_key: "base64key..."
      container: my-container
      prefix: "data/"
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `account_name` | *(required)* | Azure storage account name |
| `account_key` | | Access key (one of key/sas required) |
| `sas_token` | | SAS token |
| `container` | *(required)* | Blob container name |
| `prefix` | | Key prefix for uploaded objects |
| `format` | `parquet` | Output format |

## Authentication

Authenticate with either an account key or a SAS token. For security best practices, we strongly advise against storing the account key or SAS token in `skippr.yml`. Use environment variable interpolation instead: replace the `account_key` or `sas_token` value with your own `${ENV_VAR}` reference.

The relevant part of `skippr.yml` looks like this:

```yaml
data_sinks:
  warehouse:
    AzureBlob:
      account_key: "${AZURE_BLOB_ACCOUNT_KEY}"
```

Set the env var before running `skipprd`:

macOS / Linux

```bash
export AZURE_BLOB_ACCOUNT_KEY="base64key..."
```

Windows PowerShell

```powershell
$env:AZURE_BLOB_ACCOUNT_KEY = "base64key..."
```

Windows Command Prompt

```cmd
set AZURE_BLOB_ACCOUNT_KEY=base64key...
```

## Troubleshooting

| Symptom | Fix |
|---|---|
| authentication failed | Verify the storage account name, account key or SAS token, and container name. |
| writes fail | Check container permissions, prefix values, and any firewall restrictions on the storage account. |
