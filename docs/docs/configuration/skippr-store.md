---
title: Skippr Store
description: Local KV for resume positions, WAL catalog, stream state, shredded schema, and the DDL journal. Durable engine is fjall on DATA_DIR.
---

# Skippr Store

Skippr Store is the local key-value store on the writer host. Payload bytes stay in WAL `.seg` files or S3 objects. The store is the authority for which segments are live, how far each source has got, what the pipeline schema is, which migrations have applied, and where keyed aggregates have reached.

There is no remote table that other nodes consult. One process writes a `DATA_DIR`. Ephemeral hosts without a durable `DATA_DIR` are unsupported. DynamoDB is not Skippr Store. SkipprLake may still use a DynamoDB *Iceberg catalog table* via destination config. Never set `skippr.store.type` to pick that catalog.

## What lives where

| Family | What it holds | Used by |
|---|---|---|
| `catalog` | Live WAL: segments, index, compaction, reclaim, sequence | Ingest, compact, [`skipprd query`](/cli/query) pin |
| `offsets` | Source resume and CDC envelopes (rebuildable from catalog) | `skipprd sync` |
| `schema` | Shredded pipeline metadata | Discover, evolution, [ALTER](/operations/alter) |
| `journal` | DDL migrate SQL journal (id + checksum + phase) | [`skipprd migrate`](/cli/migrate), `ALTER TABLE` |
| `stream_state` | Materialised aggregate checkpoint and WAL cursor in one transaction | [Materialised views](/concepts/materialised-views) |
| `store_meta` | Format version, leftover `sled` copy cursor, snapshot sequence | Open, recovery |

WAL payloads are **not** in the KV. Optional store snapshots (`SKIPPR_S3_BUCKET`) are best-effort and must not fail ingest. Recovery opens the live fjall log. It does not restore from a snapshot.

## How the jobs use it

**Ingest and transforms.** Stateless `transform.ops` run in memory, then Skipprd commits the batch to the WAL and records the catalog + offsets. The lake sees transformed rows.

**Materialised views.** After WAL commit, the processor updates `stream_state` in the same Skippr Store transaction as its cursor. Compact waits on that cursor. See [Materialised views](/concepts/materialised-views).

**Migrations.** `skipprd migrate` and `skipprd query --sql "ALTER TABLE …"` apply in-stream. The `journal` family records each file (or query checksum) so a retry is idempotent. Schema shreds into `schema/`. Live WAL and the destination follow. See [skipprd migrate](/cli/migrate).

**Realtime SQL.** `skipprd query` pins a flushed snapshot of the store (`query_pin`) so SELECT can union Iceberg with the WAL slices that were live at that pin. A second process may **read** while ingest holds the writer. It must not take `writer.lock` or commit. See [Stream SQL](/concepts/stream-sql).

## Backend

The live engine is **fjall** under `{DATA_DIR}/skippr-store`. Leftover YAML `skippr.store.type=sled` maps to fjall. An on-disk `{DATA_DIR}/db` directory from that leftover engine is copied into fjall on start, then archived. `skippr.store.type=dynamodb` fails closed.

Tests may use an in-memory store. That is not a production engine.

## Configure

Local default, no store block:

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

A second **writer** process against the same `DATA_DIR` fail-closes. Extra **reader** processes pin a flushed copy and query. They do not write.

## Settings

| Key | Values | Description |
|---|---|---|
| `skippr.store.type` / `SKIPPR_STORE_TYPE` / `--store-type` | `fjall` (default) | Local engine. Leftover `sled` maps to `fjall`. `dynamodb` is an error |
| `WAL_STORAGE` | `disk`, `s3` | See [WAL and buffering](/configuration/buffering) |
| `SKIPPR_WAL_S3_BUCKET` | bucket | Dedicated WAL bucket when `WAL_STORAGE=s3` |
| `SKIPPR_S3_BUCKET` | bucket | Optional store snapshots only |

## Next steps

- [WAL and buffering](/configuration/buffering)
- [Stream SQL](/concepts/stream-sql)
- [Materialised views](/concepts/materialised-views)
- [skipprd migrate](/cli/migrate)
- [How Skipprd works](/concepts/how-it-works)
- [SkipprLake](/connectors/outputs/skipprlake)
