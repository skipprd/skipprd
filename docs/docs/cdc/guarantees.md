---
title: CDC guarantees
description: Final-state CDC in Skipprd uses order tokens, tombstones, and replay-safe compaction so the destination converges on the correct table.
---

# CDC guarantees

The promise is the **final table**, not a faithful replica of every intermediate log record. A destination that has applied Skipprd CDC should match the source's current rows for the replicated set, including deletes.

## Order

CDC batches carry an order token (source LSN, stream offset, or equivalent). The host stores that position only after the batch is visible in the WAL. On restart, sources resume from host-provided checkpoints. Applying an older token after a newer one is a bug; sinks must treat compaction ids as replay-safe so retries do not double-insert.

## Deletes

Deletes are tombstones, not silent omissions. The sink applies them so the destination row disappears (or is marked deleted, when the destination model requires it). A snapshot-only source has no tombstone stream; use `snapshot_then_cdc` or `cdc_only` when deletes must land.

## Reconciliation

Warehouse writes go through compaction. Retried compaction reuses the same `compaction_id`. Destinations that support idempotent apply can ignore work they have already seen. The offsets database is not the source of truth; it materializes WAL-visible progress. See [exactly-once](/concepts/exactly-once).

## What this is not

- It is not "at-least-once plus hope MERGE is correct."
- It is not shipping raw WAL bytes to the warehouse for you to interpret.

## Troubleshooting

- **Deletes missing** — confirm the source `cdc_mode` is not `snapshot`, and that the sink table is not append-only when the contract requires retract.
- **Duplicates after a crash** — check that the sink honors `compaction_id`. File a connector bug if it appends blindly on replay.
- **Resume from zero** — the host lost WAL-visible checkpoints. Recovery should replay committed WAL; if the WAL itself was deleted, the stream has no memory.
