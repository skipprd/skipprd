---
title: GCS destination
description: Write Skipprd batches as objects in a Google Cloud Storage prefix.
---

# GCS

Writes files to a bucket prefix. It does not create a BigQuery table — use [BigQuery](/connectors/outputs/bigquery) when you want SQL there.

## Before you begin

- A bucket and prefix.
- A service-account JSON with `storage.objects.create` (and list if you verify).

```bash
export GOOGLE_APPLICATION_CREDENTIALS="/path/to/sa.json"
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSinkGcs, Pipeline

cfg = Config.discover()
raw = cfg.data_sink(
    "raw",
    DataSinkGcs(
        bucket="acme-landing",
        prefix="skipprd/events",
        service_account_key_path="${GOOGLE_APPLICATION_CREDENTIALS}",
    ),
)
cfg.pipeline("events", Pipeline(data_source=cfg.get_data_source("sample"), data_sink=raw))
cfg.save()
```

```bash [CLI]
skipprd connect data-sink gcs \
  --pipeline events \
  --name raw \
  --bucket acme-landing \
  --prefix skipprd/events \
  --service-account-key-path '${GOOGLE_APPLICATION_CREDENTIALS}'
```

```yaml [YAML]
data_sinks:
  raw:
    Gcs:
      bucket: acme-landing
      prefix: skipprd/events
      service_account_key_path: ${GOOGLE_APPLICATION_CREDENTIALS}
```

:::

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `bucket` | string | Required | Bucket name |
| `prefix` | string | Required | Object prefix |
| `service_account_key_path` | path | Not set | Service-account JSON. ADC is used when unset. |
| `format` | string | Not set | Object format override |

A retry overwrites the same object.

## How data lands

Objects land under `gs://<bucket>/<prefix>/<namespace>/`.

## Troubleshooting

| Symptom | Fix |
|---|---|
| 403 | Grant object-create on the prefix |
| Credentials missing | Export `GOOGLE_APPLICATION_CREDENTIALS` |

## Next steps

- [BigQuery](/connectors/outputs/bigquery)
- [S3 destination](/connectors/outputs/s3)
