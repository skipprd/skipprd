---
title: Azure Blob
description: Write Skipprd batches as blobs in an Azure Storage container.
---

# Azure Blob

Writes files to a container prefix. It does not create a Synapse table — use [Synapse](/connectors/outputs/synapse) when you want SQL there.

## Before you begin

- Storage account name and container.
- An account key **or** a SAS token. Store it in the environment.

```bash
export AZURE_STORAGE_ACCOUNT_KEY="..."
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSinkAzureBlob, EnvRef, Pipeline

cfg = Config.discover()
raw = cfg.data_sink(
    "raw",
    DataSinkAzureBlob(
        account_name="acmelanding",
        container="skipprd",
        account_key=EnvRef("AZURE_STORAGE_ACCOUNT_KEY"),
        prefix="events",
    ),
)
cfg.pipeline("events", Pipeline(data_source=cfg.get_data_source("sample"), data_sink=raw))
cfg.save()
```

```bash [CLI]
skipprd connect data-sink azure_blob \
  --pipeline events \
  --name raw \
  --account-name acmelanding \
  --container skipprd \
  --account-key '${AZURE_STORAGE_ACCOUNT_KEY}' \
  --prefix events
```

```yaml [YAML]
data_sinks:
  raw:
    AzureBlob:
      account_name: acmelanding
      container: skipprd
      account_key: ${AZURE_STORAGE_ACCOUNT_KEY}
      prefix: events
```

:::

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `account_name` | string | Required | Storage account |
| `container` | string | Required | Container |
| `account_key` | secret | Not set | Account key as `${ENV}` |
| `sas_token` | secret | Not set | SAS token as `${ENV}` (use instead of the key) |
| `prefix` | string | Not set | Blob prefix |
| `format` | string | Not set | For pipeline type `ingest`, set `format` on this data sink to `parquet` or `jsonl`. For pipeline type `copy`, set `format` on this data sink to `jsonl`, `csv`, or `xml`. If you omit `format` on a copy pipeline, the data sink follows the source: json → jsonl, csv → csv, xml → xml. In pipeline type `copy`, nested objects and arrays in CSV are written as compact JSON strings. When pipeline type is `copy`, the deadletter sink still writes `parquet` or `jsonl`; it does not follow a `csv` or `xml` data sink. |

A retry overwrites the same blob.

## How data lands

Blobs land at `{container}/{prefix}/{namespace}/…`.

- `{prefix}` is `prefix`.
- `{namespace}` is the source namespace (table or stream name).
- Optional partition folders follow the namespace: `p_country=GB/` from `batch_partition_fields`, and `year=2026/month=10/day=9/` from `batch_time_fields` (or `p_year=…` when `time_partition_prefix` is `p_`).
- The file name is the batch identity plus the `format` extension, for example `apply-0001.jsonl`.

## Troubleshooting

| Symptom | Fix |
|---|---|
| Authentication failed | Export `account_key` or `sas_token` in this shell |
| Container missing | Create it, or grant the key rights to create it |
| 403 on prefix | Narrow SAS to write on that prefix |

## Next steps

- [Synapse](/connectors/outputs/synapse)
- [S3 destination](/connectors/outputs/s3)
