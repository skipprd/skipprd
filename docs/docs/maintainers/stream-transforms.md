# Stream compute

Declared compute is bound `ComputeKind` in the Skippr Store journal (`compute#workspace#pipeline`), a map keyed by compute name. YAML / Python `transform.compute` (`type: sql`, `type: sdl`, or `type: py`) serialize that IR. `skipprd sync` admits new names before ingest (including WAL-only / no `data_sink`); hash mismatch fails the run until `ALTER COMPUTE` (or `DROP COMPUTE <name>` for `type: py`; the next sync admits the files). Ingest deadletters py batches as `PYTHON_COMPUTE` when disk path, handler, or digest does not match the journal. Processor start loads the journal; it does not replace the list. Default ingest is unchanged when `transform.compute` is unset. Store authority: [Skippr Store KV](skippr-store-kv.md).

## Placement

```text
source → type: py (JSON objects) → schema map → Arrow batch → apply_log → Stateless → WAL catalog commit
                                                                              ↓
                                                                    Keyed/Latest/Windowed (stream_state family)
                                                                              ↓
                                                                    existing compact / sinks
```

- `type: py` loads `path` and calls `handler(record)` 1:1 in a bounded `python3` worker on decoded JSON objects (after array unwrap), before `flatten_events` / schema map. Copy identity is the mapped JSON line. Sibling `.py` files in that directory are on `sys.path`. Changing `path`, `handler`, or a sibling `.py` changes the journal hash; ingest matches the journaled path, handler, and digest, and deadletters as `PYTHON_COMPUTE` on mismatch.
- Stateless (filter/derive) runs after schema mapping and before `WalCommitUnit` submit.
- Keyed/Latest/Windowed consume **immutable** committed SEGF segments. There is no second processing WAL.
- `type: copy` cannot set `transform.compute` sql or sdl. `type: py` is allowed. Schema is ALTER TABLE. Stateless is filter and derive only; alter it with `ALTER COMPUTE`.

## Aggregates

`stream_state` is a Skippr Store family (fjall). Do not add a second MV store. See [#19](https://github.com/skipprd/skipprd/issues/19) / [Skippr Store KV](skippr-store-kv.md).

- `agg/{pipeline}/{id}/{key}` — measure map
- `row/{pipeline}/{id}/{key}` — last business-row image (CDC deltas when WAL has only one row image)
- `ent/{pipeline}/{id}/{entity}` — leftover-column identity → last aggregate key (PK changes)
- `dedup/{pipeline}/{id}/{event_id}` — replay-safe event ids
- `meta/{pipeline}/{id}` — `{cursor}`

One Skippr Store transaction writes measures, last-row images, entity index, dedup keys, and the WAL cursor (`seq`, `segment_id`, `slice_ordinal`, `row_index`). CDC: insert adds; update applies `new - last_row` (missing last-row is treated as insert); delete subtracts last-row (missing last-row is a no-op); primary-key change is delete-old + insert-new, using leftover entity columns (for example `invoice_id`) to find the previous key.

P0 metrics: `stream_transform_records_total`, `stream_transform_failures_total`, `stream_processor_records_total`, `stream_processor_cdc_{insert,update,delete}_total`, `stream_processor_txn_failures_total`, `stream_processor_wal_lag`.

## Testing

All processor tests open a real fjall Skippr Store on the local filesystem. A successful apply flushes so a process crash after the txn can reopen the same path.

- Smoke (local iteration): `./scripts/stream-transforms-smoke.sh` (`SKIPPR_STREAM_TEST_PROFILE=smoke`).
- Full matrix (git commit via `scripts/precommit.sh`, and `.github/workflows/stream-transforms.yml` on pull requests): deterministic CDC scripts vs a golden model, crash injection at `BeforeTxn` / `BeforeCommit` / `DuringTxn` / `AfterTxn` with drop+reopen, multi-row batch atomicity, reclaim-hold restore from the disk cursor, and proptest fuzz of event streams plus Arrow/CDC alignment.

A config edit whose canonical hash differs from the named catalog entry fails closed until `ALTER COMPUTE` writes the new entry (View / KeyedTable / WindowedTable also accept `ALTER TABLE` sugar). Latest and StreamTableJoin identity changes are `DROP` then `CREATE`. `type: py` is `DROP COMPUTE <name>`; the next sync admits the files. WAL, offsets, and the destination stay. Sync admit does not wipe ad-hoc journal rows.

## Reclaim hold

Compaction may mark slices done and sink as today. `complete_slices_and_reclaim` deletes catalog row + payload only when `segment.seq < processor min_unconsumed_seq`. After the processor finishes a segment it calls `reclaim_fully_done_before`. Ingest is not blocked; WAL retention waits on processor lag.

## Out of this milestone

Processor output to sinks. Warehouse dests do not get Skippr MVs in v1. No sharding.
