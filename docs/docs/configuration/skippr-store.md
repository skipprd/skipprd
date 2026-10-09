---
title: State store
description: Persist resume positions and catalog pointers so a new machine can continue a pipeline. Choose local state, DynamoDB, or Skippr Cloud Tables.
---

# State store

Skipprd records how far each source has got and, for some lake destinations, where the catalog lives. That record is the **state store**. The write-ahead log is still the source of truth for data; the store is how a new process finds its place.

You need an external store when the machine is ephemeral. On a long-lived host with `WAL_STORAGE=disk`, the default local store is enough.

## Choose a backend

| Type | Set with | Use when |
|---|---|---|
| `sled` (default) | nothing, or `SKIPPR_STORE_TYPE=sled` | Persistent `DATA_DIR` on one host |
| `dynamodb` | `SKIPPR_STORE_TYPE=dynamodb` | S3 WAL, Lambda, or a cluster of nodes |
| `cloud-tables` | `SKIPPR_STORE_TYPE=cloud-tables` | The same job on Skippr Cloud Tables |

`WAL_STORAGE=clustered` defaults to DynamoDB if you do not set a type. Set `cloud-tables` when that cluster runs on Skippr Cloud.

Name the table with `SKIPPR_STORE_NAME` (or `skippr.store.name` in `skippr.yml`). Required for DynamoDB, Cloud Tables, and clustered WAL.

## Configure

Local default — no store block:

```yaml
skippr:
  # WAL stays on DATA_DIR; resume state stays next to it
```

S3 WAL plus DynamoDB:

::: code-group

```yaml [YAML]
skippr:
  wal_s3_bucket: acme-skipprd-wal
  store:
    type: dynamodb
    name: acme-skipprd-offsets
```

```bash [CLI]
export WAL_STORAGE=s3
export SKIPPR_WAL_S3_BUCKET=acme-skipprd-wal
export SKIPPR_STORE_TYPE=dynamodb
export SKIPPR_STORE_NAME=acme-skipprd-offsets
skipprd sync --once --pipeline google_analytics
```

```python [Python]
from skippr import Config

cfg = Config.discover()
# Store type and name are engine settings (environment / skippr.store).
# Connectors still go through Config as usual.
cfg.save()
```

:::

Clustered WAL on DynamoDB:

```bash
export WAL_STORAGE=clustered
export SKIPPR_STORE_NAME=acme-skipprd-offsets
skipprd sync --pipeline google_analytics
```

Skippr Cloud Tables:

```yaml
skippr:
  store:
    type: cloud-tables
    name: acme-skipprd-offsets
```

```bash
export WAL_STORAGE=clustered
export SKIPPR_STORE_TYPE=cloud-tables
export SKIPPR_STORE_NAME=acme-skipprd-offsets
skipprd sync --pipeline google_analytics
```

## After a crash

If a resume row is missing but the WAL still has the batch, the next run rebuilds progress from the WAL. You do not edit the store by hand.

If both the WAL and the store are gone, the pipeline starts from the beginning of the source.

## Settings

| Key | Values | Description |
|---|---|---|
| `skippr.store.type` / `SKIPPR_STORE_TYPE` / `--store-type` | `sled`, `dynamodb`, `cloud-tables` | Where resume state lives |
| `skippr.store.name` / `SKIPPR_STORE_NAME` / `--store-name` | table name | Required except for local `sled` |
| `WAL_STORAGE` | `disk`, `s3`, `clustered` | See [WAL and buffering](/configuration/buffering) |
| `SKIPPR_WAL_S3_BUCKET` | bucket | Dedicated WAL bucket when `WAL_STORAGE=s3` |

`sled` is a config value, not something you operate as a database.

## Next steps

- [WAL and buffering](/configuration/buffering)
- [How Skipprd works](/concepts/how-it-works)
- [SkipprLake](/connectors/outputs/skipprlake)
