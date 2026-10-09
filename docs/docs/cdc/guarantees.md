---
title: CDC guarantees
description: What you can rely on when Skipprd streams inserts, updates, and deletes into a warehouse — and what to check when a row is missing or duplicated.
---

# CDC guarantees

The promise is the **current table**, not a replay of every intermediate change. After Skipprd has applied a CDC pipeline, the destination rows for the replicated set should match the source, including deletes.

This is the contract you operate against. Implementation details stay inside Skipprd.

## Order

Each CDC batch carries a source position (a PostgreSQL LSN, a binlog coordinate, a stream sequence). Skipprd records that position only after the batch is durable in the write-ahead log. The next run resumes from there.

Applying an older batch after a newer one is a bug. If you see an old version of a row overwrite a newer one, file it against the source or destination connector — do not rewind the pipeline yourself.

## Deletes

Deletes are explicit. Skipprd tells the destination to remove the row (or mark it deleted, when that is how the warehouse models it). A missing delete is usually one of:

- The source is still on `cdc_mode: snapshot`, which never emits deletes.
- The destination only appends, so the old row stays.
- The source table has no primary key, so the database cannot describe which row to delete.

## Retries

After a crash, Skipprd writes the same committed batches again. Destinations that support exactly-once apply recognise a retry and do not insert a second copy. Destinations that only append can duplicate those batches — pick a warehouse from the exactly-once list in [Exactly-once delivery](/concepts/exactly-once) when duplicates matter.

The resume position Skipprd stores is a cache of progress. The write-ahead log is the source of truth. If you delete `DATA_DIR` (or the S3 WAL), Skipprd has nothing to resume from and the source starts again.

## What this is not

- It is not "at-least-once plus hope a MERGE in the warehouse is correct."
- It is not shipping the source log to you to interpret.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| Deletes never appear | `cdc_mode` is `snapshot`, or the destination is append-only | Switch to `snapshot_then_cdc` or `cdc_only`, and use a destination that can apply deletes |
| Duplicates after a crash | The destination appended the retried batch | Use a destination from the exactly-once list, or deduplicate downstream on the primary key |
| Sync starts from the beginning | The WAL and resume state were deleted | Restore `DATA_DIR` or the S3 WAL from backup. If they are gone, treat it as a new pipeline |
| Updates rejected at the source | The table has no primary key or replica identity | Add a primary key, or set replica identity, then restart sync |

## Next steps

- [Change data capture](/cdc/)
- [Exactly-once delivery](/concepts/exactly-once)
- [WAL and buffering](/configuration/buffering)
