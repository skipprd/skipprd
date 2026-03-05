# Exactly-Once Delivery

Skippr guarantees that every source record is written to the destination exactly once, even through crashes and restarts (including SIGKILL).

## How it works

Three mechanisms work together:

### 1. Write-Ahead Log (WAL)

Every ingested record is written to the WAL before any processing. WAL segments are immutable once flushed. On crash, the WAL is replayed to recover any data that was ingested but not yet compacted.

WAL segments can be stored on local disk (`WAL_STORAGE=disk`) or S3 (`WAL_STORAGE=s3`). S3 WAL removes any dependency on local state.

### 2. Offsets database

The offsets database tracks which source records have been fully processed (ingested, compacted, uploaded). It is stored on local disk at `DATA_DIR`.

On restart, Skippr reads the offsets database and skips any source records that were already committed. This prevents duplicate writes.

### 3. Compactor drain

On clean shutdown, the compactor service is drained: all in-flight WAL segments are compacted and uploaded before the process exits. If the drain fails, the process exits with a non-zero status code, signaling that the run should not be trusted.

## Crash recovery

When Skippr restarts after a crash:

1. The WAL is scanned — all segment files (disk or S3) are indexed
2. Committed offsets are loaded from the offsets database
3. Segments containing already-committed data are skipped
4. Remaining segments are re-compacted and uploaded

The key invariant: **no data is lost and no data is duplicated**, because the offsets database records what has been uploaded and the WAL preserves what was ingested.

## Chaos mode

Skippr includes a built-in chaos mode (`SKIPPR_CHAOS_MODE=yes`) that injects random SIGKILL signals during ingestion. This is used in CI/CD to validate exactly-once guarantees under failure conditions.

The integrity check at the end of each run verifies:

```
Compactor: summary uploaded_rows=X expected_msgs=Y quarantined_parts=Z
```

A clean run has `uploaded_rows == expected_msgs` and `quarantined_parts == 0`.

## Tombstones

When a WAL segment has been fully compacted and uploaded, Skippr writes a tombstone marker. Tombstones prevent a segment from being reprocessed on recovery. Tombstones are only written after the segment is successfully deleted, preventing orphaned state.
