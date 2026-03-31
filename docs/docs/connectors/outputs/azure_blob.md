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
