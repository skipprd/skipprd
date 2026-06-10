# Ingest offset contract

## Immutable input objects

Skipprd treats each source partition as an **immutable whole object** (S3 key, local file, API export blob, etc.). Once that object has been durably ingested, it must not be read again on later syncs.

- **`Closed=1`** on the offset store means “this object is done.” Sources list partitions and skip keys that are already closed (`partition_already_closed`, S3 `validate(Closed, 1)`).
- After a WAL segment for a partition is committed, ingest marks the offset **`Closed=1`** and sets **`Position`** to the highest line/LSN seen in that flush.
- Re-running a pipeline against the same immutable object should be a no-op at the source and ingest gate.

## When `Position` matters

`Position` is for **resume within a growing or CDC stream**, not for typical immutable file/API batches:

- **Postgres CDC** sets an explicit `offset_pos` (LSN) per batch via `IngestBatch::new_with_offset_pos`.
- **CDC batches** (`cdc_rows` present) always pass the ingest offset gate so changefeed semantics are preserved.
- File/API sources usually leave `offset_pos` as `None`; ingest compares line numbers only when `offset_pos` is set or CDC rows are present.

## Ingest gate (`process_batch`)

For each batch, ingest loads one offset snapshot per `offset_key` and decides per record:

1. CDC batch → ingest.
2. No offset entry → ingest.
3. `Closed != 0` → skip (immutable object already done).
4. When tracking position (CDC or explicit `offset_pos`) → skip records at or before the stored line/LSN.

This avoids per-record `Offsets::validate` sled lookups on the hot path.

## Operational notes

- Do not rely on line-based resume for immutable objects; use `Closed` and source-side skip logic.
- If a single large object is split across multiple `IngestBatch`es in one run, the first flush may set `Closed=1` before later chunks arrive — defer closing until end-of-object if that pattern appears in production.
