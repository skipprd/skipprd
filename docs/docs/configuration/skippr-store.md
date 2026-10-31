---
title: Skippr Store
description: Local KV for resume positions, WAL catalog, stream state, and shredded schema. Durable engine is fjall on DATA_DIR.
---

# Skippr Store

Skipprd records how far each source has got, WAL catalog rows (live segments, compaction, reclaim), stream-processor checkpoints, and shredded pipeline schema in **Skippr Store**. Payload bytes stay in `.seg` files or S3 objects; the store is the authority for which segments are live.

The store is a **local KV on the writer host**. There is no remote table that other nodes consult. Ephemeral hosts without a durable `DATA_DIR` are unsupported.

## Backend

The live engine is **fjall** under `{DATA_DIR}/skippr-store`. Leftover YAML `skippr.store.type=sled` maps to fjall. An on-disk `{DATA_DIR}/db` directory from that leftover engine is copied into fjall on start, then archived.

`skippr.store.type=dynamodb` fails closed. DynamoDB is not Skippr Store. SkipprLake may still use a DynamoDB *Iceberg catalog table* via destination config — never via `skippr.store.type`.

## Configure

Local default — no store block:

```yaml
skippr:
  # WAL stays on DATA_DIR; Skippr Store stays next to it
```

Leftover YAML `skippr.store.type: sled` maps to fjall. It is not a live engine choice.

S3 WAL plus local store (WAL payloads on S3, KV still on `DATA_DIR`):

::: code-group

```yaml [YAML]
skippr:
  wal_s3_bucket: acme-skipprd-wal
  store:
    type: fjall
```

```bash [CLI]
export WAL_STORAGE=s3
export SKIPPR_WAL_S3_BUCKET=acme-skipprd-wal
skipprd sync --once --pipeline google_analytics
```

```python [Python]
from skippr import Config, FjallStore

cfg = Config.discover()
cfg.store(FjallStore())
cfg.save()
```

:::

## After a crash

Open ignores optional store snapshots and replays the local fjall log. If a resume row is missing but the WAL still has the batch, the next run rebuilds progress from the WAL.

If both the WAL and the store are gone, the pipeline starts from the beginning of the source.

A second **writer** process against the same `DATA_DIR` fail-closes.

## Settings

| Key | Values | Description |
|---|---|---|
| `skippr.store.type` / `SKIPPR_STORE_TYPE` / `--store-type` | `fjall` (default) | Local engine. Leftover `sled` maps to `fjall`. `dynamodb` is an error |
| `WAL_STORAGE` | `disk`, `s3` | See [WAL and buffering](/configuration/buffering) |
| `SKIPPR_WAL_S3_BUCKET` | bucket | Dedicated WAL bucket when `WAL_STORAGE=s3` |
| `SKIPPR_S3_BUCKET` | bucket | Optional store snapshots only |

## Next steps

- [WAL and buffering](/configuration/buffering)
- [How Skipprd works](/concepts/how-it-works)
- [SkipprLake](/connectors/outputs/skipprlake)
