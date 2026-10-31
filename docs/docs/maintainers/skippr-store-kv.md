# Skippr Store KV

Lock for [skipprd/skipprd#19](https://github.com/skipprd/skipprd/issues/19). Dated 2026-10-31, amended the same day after cluster/leases left `main`.

This file is the authority for Skippr Store. If this page and the issue body, README, CHANGELOG, or other maintainer pages disagree, this page wins. Do not duplicate the lock in the issue body.

## Hard constraints

- **No central coordination.** Skippr Store is a local KV on the writer host. There is no remote table that other nodes consult to decide who may write, who is primary, or what the catalog contains.
- **No DynamoDB Skippr Store backend.** `skippr.store.type=dynamodb`, `--features offset-store-dynamodb` as a store, and `crates/skippr-offset-store-dynamodb` as WAL catalog / offsets / fence authority MUST NOT exist after this cutover. Dynamo as a *source connector* or as a SkipprLake Iceberg *destination catalog table* is a different product; it is not Skippr Store. Plugins and e2e **MUST NOT select an Iceberg catalog backend via skippr.store.type** / `SKIPPR_STORE_TYPE` / `SKIPPR_STORE_NAME`.
- **No clustered WAL, membership, writer leases, or write quorum.** Those left `main` for `poc/clustered-ha`. Do not put them back as a store family.
- **No multi-writer.** One process writes the store for a `DATA_DIR`. Transforms, aggregations, materialized views, WAL catalog commits, and schema evolution share that writer.
- **No sharding.** `PipelineKey` is identity. Do not add shard-scoped leases or entity-hash ownership “for later.” Multi-writer is not the plan, so sharding has no job.
- **Reads may be many.** In-process today (ingest, instream transforms/aggs, client SQL). Extra reader processes later. Writes stay single-node.
- **Ephemeral hosts without a durable DATA_DIR are unsupported this milestone.** Throwaway compute does not get a remote Skippr Store. WAL payloads MAY still be S3; the KV stays local.

## Today on `main`

This cutover is the live code. Clustered WAL, gossip membership, writer leases, and replica Flight WAL pins stay gone. Skippr Store live impls are **memory** and **fjall**. Leftover `{DATA_DIR}/db` is a read-only sled migrate, not a Store trait. `skippr.store.type=dynamodb` fails closed. WAL payloads are disk or S3. Writer OCC is `CatalogFence` on trait commit. `stream_state` and shredded `schema` are families on that trait. Leftover `metadata.json` is import-only.

This lock is not a return of HA writes.

## Backends

| Kind | Role | Store trait |
|---|---|---|
| **memory** | Tests and DST | Yes (process lifetime, writes) |
| **fjall** | The durable local engine | Yes (writes). Auto-tune compaction in code. No `FJALL_*` / `ROCKSDB_*` env knobs. |
| **sled** | Leftover `{DATA_DIR}/db` | **not a Store trait implementation.** Open read-only. Prefix-iterate trees into fjall. Never commit, never create new sled trees as the live store. |

Live trait impls are **memory** and **fjall** only. Both MUST pass the same conformance suite (conditional commit, prefix scan, reader pin). Writer OCC is CatalogFence on trait commit. It is not a lease store, not membership, and not a Dynamo row. Readers do not take a write fence; they pin a seq / snapshot for the query.

RocksDB (including secondary) is out. Remote KV (Dynamo, Cloud Tables, FoundationDB, …) is out as Skippr Store.

Config after cutover: default is fjall on `DATA_DIR`. `skippr.store.type=dynamodb` fails closed. skippr.store.type=sled after cutover maps to fjall (leftover YAML; sled is detected on disk, not chosen as an engine). `sled` is not an operator live-engine choice.

`TXN_MAX_OPS` is a code ceiling, not a Dynamo 99-item portability leftover.

## Keyspaces (same recovery)

| Family | Role |
|---|---|
| `store_meta` | Format version, sled-migrate cursor, schema-import cursor, snapshot seq |
| `catalog` | Live WAL (`CatalogKey`: segments, index, compaction, seq, format) |
| `offsets` / `checkpoints` | Source resume; CDC envelopes; rebuildable from catalog |
| `stream_state` | MV / aggregate checkpoint; cursor in the same txn as measures |
| `schema` | Shredded pipeline metadata (replaces `metadata.json`) |
| `journal` | Existing migrate SQL journal; no second DB |

Do not add a `leases` family, membership rows, or cluster ids. Single writer + `CatalogFence` is the write authority.

## Single writer, many readers

**Writes** (one node, one process): ingest catalog commit, offset publish, schema evolution, stream-processor apply, compaction reclaim, migrate-journal.

**Reads** (many, overlapping):

- Instream transforms (stateless ops) and aggregations / MV (`stream_state` + WAL catalog)
- Client SQL (`skipprd query`, Flight SQL live-WAL tickets, Iceberg ∪ WAL)
- Later: extra **reader processes** against the same catalog, MV, and other families (not writes)

Those readers MUST share the trait (prefix scan + pin). They MUST NOT open a second store design, a replica Flight WAL leaf, or a central coordinator.

This milestone: one OS process opens fjall; ingest and query multiplex in-process. A second OS process that wants to **write** the same `DATA_DIR` fail-closes.

Log replication and extra reader processes are out of this ticket; the trait and key layout MUST NOT assume a single in-process reader, and MUST NOT grow write-side sharding to fake it.

## Sled migration (keep sled, no write path)

Do not delete the sled crate or the ability to **read** `{DATA_DIR}/db` in the same change as fjall. Existing production directories are sled. Killing sled before migrate would strand them.

On start:

1. Open or create fjall under `DATA_DIR` (the live store).
2. If `{DATA_DIR}/db` is a sled database, open it with sled 0.34 `open` (that crate has no read-only mode). MUST NOT insert, commit, or create sled trees. The live store is fjall.
3. Copy remaining sled trees into fjall using `store_meta` migrate cursor. Production ingest, transforms, and query keep running against **fjall**. Lookups that miss in fjall MAY read sled until that key is copied (then fjall wins).
4. Log at **info**, periodically and at completion: sled → fjall, keys remaining, trees done. Operators should see progress on a live workload, not a maintenance window.
5. When the cursor says complete: fsync, archive `{DATA_DIR}/db` to `db.sled-migrated-<seq>`, stop opening sled.

Crash mid-migrate: next start resumes from the cursor. Do not fail closed merely because sled and fjall both exist — that is the in-progress state. Fail closed only on unreadable sled, fjall open errors, or a migrate cursor that disagrees with checksums.

Golden fixture functional tests + DST: success, crash+retry, concurrent ingest during copy, second **writer** process fail-closed, leftover sled after archive ignored.

## `metadata.json` → `schema` family

Stop rewriting the whole `PipelineMetadata` JSON on every evolution.

```
schema/{pipeline}/head
schema/{pipeline}/ns/{namespace}/node/{field_id}   # one node + child id list, not child bodies
schema/{pipeline}/ns/{namespace}/contract
```

An evolution writes **O(depth of the changed path)** (leaf + ancestors + `head.schema_id`), not O(tree). Reconstruct in-memory with a prefix scan at load/pin. Per-record ingest still uses in-memory `METADATA`.

**Startup import (local file and S3).** Today the object key is `{tenant}/{workspace}/{pipeline}/metadata/metadata.json` via the storage adapter (`skipprd_el_storage_mode` local or S3). On start, if `schema/.../head` is missing:

- Import the JSON that exists (disk, S3, or both).
- If local and S3 both exist and **checksums differ, fail closed**. Do not pick a winner.
- If only one exists, import that one.
- Record the cursor in `store_meta`, then stop treating `metadata.json` as authority.

Same transparent info log as sled migrate. After cutover there is no shadow `metadata.json` write. A later start that still sees the file/object and a live `head` MUST NOT dual-write; JSON is leftover.

## Recovery

Keep SEGF as the ingest log. KV is a checkpoint. On crash: reopen fjall, replay **its** disk log. Do not recover the store from a snapshot.

## Snapshots

`skippr.skippr_s3_bucket` / `SKIPPR_S3_BUCKET` is **optional store snapshots only**. If unset, `{DATA_DIR}/store-snapshots/`. WAL payloads stay on disk or `SKIPPR_WAL_S3_BUCKET`. Snapshot PUT is best-effort and MUST NOT fail ingest. Open ignores snapshots and replays the local log.

S3 WAL is payload durability, not Skippr Store coordination.

## What is not Skippr Store

| Thing | Where it lives |
|---|---|
| WAL payload bytes | `.seg` on disk or S3 objects |
| Iceberg data files | Warehouse URI |
| SkipprLake / Glue Iceberg catalog pointers | Destination catalog (own config; **MUST NOT select an Iceberg catalog backend via skippr.store.type**) |
| Writer leases / membership / ClusterId | Deleted on `main`; stay deleted |

## DST

Every family: fence/CAS/prefix; schema shred byte-size + reconstruct; stream_state cursor atomicity + reclaim-hold; before/during/after txn crash drop+reopen; snapshot written but recover from **log**; sled concurrent migrate; metadata.json local + S3 import including checksum-mismatch fail-closed; `dynamodb` store type fails closed; leftover `type: sled` maps to fjall; no `leases` family; no shard keys.

## Out of this ticket

Turning on extra reader processes; log replication; RocksDB; Dynamo as store; payloads in the KV; DataFusion over `stream_state` ([#32](https://github.com/skipprd/skipprd/issues/32)). Public customer docs (`docs/docs/configuration/skippr-store.md`) ship with this cutover.

## Cutover

This table is history of the cutover, not a second live design. The right column is `main`.

| Before | After (live) |
|---|---|
| sled exclusive open (Dynamo optional) | fjall live; leftover sled is a migrate reader (not a Store trait); Dynamo store gone |
| `metadata.json` local / S3 whole document | `schema` family, shredded; one-way import on start; checksum mismatch fail-closed |
| `SKIPPR_S3_BUCKET` schema + misc | snapshots only |
| `stream_state` raw sled tree | family on the trait |
| clustered WAL / leases | already gone; do not restore |
| second process | second **writer** fail-closes; extra **reader processes** allowed by the trait later |
| Iceberg catalog via `skippr.store.type` | lake catalog has its own config |
| `TXN_MAX_OPS = 99` as Dynamo portability | code ceiling only |
