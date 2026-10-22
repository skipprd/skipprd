# Stream transforms (first milestone)

Optional in-memory Arrow ops and a single-worker Sled processor. Default ingest is unchanged when `transform.ops` and `transform.aggregates` are unset.

## Placement

```text
source → schema map → Arrow batch → transform.ops → WAL catalog commit
                                              ↓
                                    transform.aggregates (Sled stream_state)
                                              ↓
                                    existing compact / sinks
```

- Stateless `ops` run after schema mapping and before `WalCommitUnit` submit, on both the JSON ingest path and the Arrow IPC runtime path.
- Stateful `aggregates` consume **immutable** committed SEGF segments. There is no second processing WAL.
- `type: copy` cannot set `ops` or `aggregates`.
- `WAL_STORAGE=clustered` cannot set `aggregates` (local Sled is not a distributed state store).

## Ops

Each `transform.ops` entry sets exactly one of `filter`, `project`, `derive`, `rename`, `cast`. Filter expressions are DataFusion SQL booleans; the boolean column is used as a mask so CDC `WalRowMeta` stays 1:1 with surviving rows. A transform error fails the ingest request and does not advance source offsets. Changing `ops` does not rewrite committed WAL.

## Aggregates

Sled tree `stream_state` on `{DATA_DIR}/db` (sibling of `offsets` / `wal_catalog`, not catalog authority):

- `agg/{pipeline}/{id}/{key}` — measure map
- `row/{pipeline}/{id}/{key}` — last business-row image (CDC deltas when WAL has only one row image)
- `ent/{pipeline}/{id}/{entity}` — leftover-column identity → last aggregate key (PK changes)
- `dedup/{pipeline}/{id}/{event_id}` — replay-safe event ids
- `meta/{pipeline}/{id}` — `{cursor, config_hash}`

One Sled transaction writes measures, last-row images, entity index, dedup keys, and the WAL cursor (`seq`, `segment_id`, `slice_ordinal`, `row_index`). CDC: insert adds; update applies `new - last_row` (missing last-row is treated as insert); delete subtracts last-row (missing last-row is a no-op); primary-key change is delete-old + insert-new, using leftover entity columns (for example `invoice_id`) to find the previous key.

P0 metrics: `stream_transform_records_total`, `stream_transform_failures_total`, `stream_processor_records_total`, `stream_processor_cdc_{insert,update,delete}_total`, `stream_processor_txn_failures_total`, `stream_processor_wal_lag`.

## Testing

All processor tests open a real Sled directory on the local filesystem (not `temporary(true)`). A successful apply flushes the tree so a process crash after the txn can reopen the same path.

- Smoke (local iteration): `./scripts/stream-transforms-smoke.sh` (`SKIPPR_STREAM_TEST_PROFILE=smoke`).
- Full matrix (git commit via `scripts/precommit.sh`, and `.github/workflows/stream-transforms.yml` on pull requests): deterministic CDC scripts vs a golden model, crash injection at `BeforeTxn` / `BeforeCommit` / `DuringTxn` / `AfterTxn` with drop+reopen, multi-row batch atomicity, reclaim-hold restore from the disk cursor, and proptest fuzz of event streams plus Arrow ops/CDC alignment.

Config-hash mismatch deletes that aggregate's prefix and rebuilds from WAL start.

## Reclaim hold

Compaction may mark slices done and sink as today. `complete_slices_and_reclaim` deletes catalog row + payload only when `segment.seq < processor min_unconsumed_seq`. After the processor finishes a segment it calls `reclaim_fully_done_before`. Ingest is not blocked; WAL retention waits on processor lag.

## Out of this milestone

Processor output to sinks, DataFusion table provider over Sled, joins/windows, distributed key ownership.
