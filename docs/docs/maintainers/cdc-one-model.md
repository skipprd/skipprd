# CDC one model

One event, one winner test. Both live next to `WalRowMeta` in `src/plugins/cdc.rs`. Plugins import that module through the runtime SDK. The query engine uses it directly.

`WalRowMeta` stays the bytes on the WAL part: mutation, event id, order-token bytes, and the before-image. `CdcEvent` is that meta once a reader knows the row's WAL address, including the before-image. Callers do not compare tokens themselves.

Hard cutover. Every row is written under this model. There is no assignment of ties onto old rows, no reader for a segment that already holds two token widths, and no backfill of a side file or a warehouse column.

## The event

```rust
pub struct CdcEvent {
    pub mutation: MutationKind, // Snapshot, Insert, Update, Delete
    pub event_id: EventId,      // computes dedup on this
    pub order: OrderToken,      // non-empty, fixed width, bytewise Ord
    pub tie: Option<Tie>,       // None only before the row has a WAL address
    /// Ingest batch transaction id. Assigned-by-write.
    /// The source never supplies it. The WAL write stamps it.
    /// Not a source transaction id, not an LSN, and not an event id.
    pub ingest_batch_id: IngestBatchId,
    /// Source transaction id, when the plugin sent one.
    /// Not `ingest_batch_id`, not an LSN, and not an event id.
    pub source_transaction_id: Option<SourceTransactionId>,
    /// Old row, copied from `WalRowMeta`. The old key is this row's business key.
    /// Absent on insert, on a snapshot that creates the key,
    /// and on an update that did not carry one.
    pub before: Option<BeforeImage>,
}

pub struct Tie {
    pub seq: u64,
    pub ordinal: u32,
    pub row: u64, // later position wins
}

/// What Iceberg and the warehouse store for a key. The tie is kept.
pub struct StoredWinner {
    pub order: OrderToken,
    pub tie: Tie,
}
```

```rust
pub struct WalRowMeta {
    pub mutation: MutationKind,
    pub event_id: Vec<u8>,
    pub order_token: Vec<u8>,
    /// Ingest batch transaction id. Assigned-by-write.
    /// The source never sets this field. The WAL write stamps one value
    /// on every row of the submit.
    /// Not a source transaction id, not an LSN, and not an event id.
    pub ingest_batch_id: IngestBatchId,
    /// Source transaction id, when the plugin sent one.
    /// Not `ingest_batch_id`, not an LSN, and not an event id.
    pub source_transaction_id: Option<SourceTransactionId>,
    /// Old row, when the source sent one.
    /// The old key is this row's business key.
    /// Absent on insert, on a snapshot that creates the key,
    /// and on an update that did not carry one.
    pub before: Option<BeforeImage>,
}
```

`BeforeImage` is the previous row the source already holds. The shared business-key encoder reads the old key from that row. It is not a second key encoding. `before` stays optional.

On an update, the old key is chosen in this order:

1. `before` is present. The old key is that row's business key.
2. `before` is absent. Ingest rejects the update. It does not insert the new key. It does not write a row under the old key. The reject logs one error, and that error is the only record of the drop.

The reject log is an error operators can alert on. It is not an ordinary ingest error. The message names the new key, the event id, and the reason: no before-image. It does not name an entity. The event has no entity id. A search for that reason finds these rejects and no other ingest failure.

There is no cache of a last-seen key. Nothing rebuilds the old row from the lake, the side file, or the warehouse.

`EventId` stays on the event. Computes dedup on it. It is not an order token.

`OrderToken` rejects an empty token and a token whose width does not match the source. That check runs when the WAL part is written (`WalPartMeta::cdc`), so Iceberg, query, and the warehouse cannot each invent it.

## Two constructors

Ingest does not call `from_wal`. A source builds `WalRowMeta` before any segment position exists, and ingest stores that meta. At the WAL write, ingest stamps one ingest batch transaction id, `ingest_batch_id`, on every row of that submit. The source never supplies it, and the source-built meta does not carry it. It is not a source transaction id, not an LSN, and not an event id. A plugin may also set `source_transaction_id` on the row. That field is optional. It is the source's own transaction id, and it is never the ingest batch id. Stateless compute filters the same vec in step with the rows, still with no tie. Ingest acks are eventually consistent. The catalog does not wait on the source, and it does not block the ack. The catalog is a trailing WAL consumer. It publishes entries from committed segments. The offset high-water, the stored Postgres resume LSN, and the deadletter offset hint trail that same WAL. Postgres is told the resume LSN when the WAL write is durable.

`CdcEvent::from_wal(meta, tie)` is the read and compact constructor. It runs only after the part has a sequence, ordinal, and row. It sets `tie` to `Some`. It copies `before`, `ingest_batch_id`, and `source_transaction_id` from `WalRowMeta` onto the event. Commit stores that same tie. A committed winner is a `StoredWinner`, never a token with the tie stripped. `from_wal` does not drop `before`.

## Winner test and collapse

`beats(candidate, incumbent)` is the only comparison. Query, Iceberg, and the warehouse call this function. The lake winner is the incumbent argument. Commit does not change the rule.

- A greater order token wins.
- The same token and a later tie wins, including when the incumbent is already committed.
- The same token and the same tie does not win. A replay of a committed row stays put.

`collapse` is the only per-key reduction. It takes the candidate events and the stored incumbent, and calls `beats`. Its result is explicit:

```rust
pub enum Collapse {
    Upsert(CdcEvent),
    Tombstone { key: BusinessKey, winner: StoredWinner },
}
```

A delete whose key was never stored is still a `Tombstone`. The tombstone blocks an older upsert. The side file and the warehouse both store the winning `StoredWinner`. They do not decide the rule.

The incumbent for a key is one value: whichever of the row winner and the tombstone winner `beats` the other. Query reads both. A delete that compacted first stays the incumbent, so an older upsert still in the WAL does not come back.

Tombstones are not expired. There is no cleanup tied to a reader pin, and no size cap. The side file and the tombstone table grow with the keys they store. That growth stays until metrics show it is a problem.

There is no `apply_op`. Warehouse compaction calls `collapse`. `collapse` already returns `Upsert` or `Tombstone`.

There is no `encode_row`. Changelog sinks call the shared projection and write every change. They do not call `collapse`. Any encoding of a row includes the mutation, the token, and the tie. A changelog can replay an equal-token pair in the order `beats` decided.

Query helper columns are the Arrow spelling of one `CdcEvent`: mutation, token, and one tie. They are not a second schema. Append rows have no CDC meta and stay outside `from_wal`. `collapse` leaves them as appends.

Business-key identity is one encoder, shared by query and Iceberg. Arrow row keys and length-prefixed strings are not two identities.

A primary-key change is one projection in front of both consumers. It runs on `CdcEvent` after `from_wal`, so the old key is the business key of `before` on that event. `from_wal` has already assigned each half a tie from its WAL address. The projection then copies the update's tie onto both the delete of the old key and the insert of the new key. The two halves share that tie. They do not keep the separate address ties. The delete keeps the parent update's event id. The insert gets an id derived from that parent id, the parent id plus an insert suffix, or an equivalent deterministic function of the parent id. There is no counter and no coordination. The derived id is unique to that parent, so a collision with an unrelated event id does not happen. Dedup cannot drop one half. Both carry the update's token, the same `ingest_batch_id`, and the same `source_transaction_id` when the update had one. Compute and `collapse` read the pair. Neither path invents its own key move. An update rejected at step 2 never becomes an event.

## Writers

Iceberg persists a shard by version, not by lock. See [Side-file versions](#side-file-versions).

Before the warehouse writer removes a row or applies a delete, it runs `beats` against the complete set for that key: the current row token, the current tombstone token, and the incoming event. It acts only on the winner. That update is one transaction and one catalog entry on the shared sequence below. A reader does not pin that entry alone. Row token 10, tombstone token 4, incoming delete at 7: the winner is the row at 10, and the writer must not remove it. A stored result of tombstone 7 and no row is not a correct answer.

## Visibility

There is one catalog. Iceberg compaction and warehouse compaction append to it by one atomic append. That append assigns the next sequence number. A range pin reads that one sequence, so entries from every sink stay in one order.

Readers pin a contiguous range of catalog entries, from a start entry through the tip. The tip is the latest committed entry on that sequence. Nothing beyond it is in the range. The catalog returns that prefix as one set. That set is what `beats` runs on. Query and serve do not list the WAL and open the snapshot as two loads. A reader gives up single-entry freshness. The range is consistent by construction.

The range pin is Skippr's. Iceberg cannot read a start-to-tip range. Athena reads the current Glue snapshot and cannot run `beats`. Skippr collapses the range, then publishes one Iceberg snapshot. Athena reads that snapshot.

A compact writer first puts the Iceberg snapshot and the side-file version in storage nothing references. The catalog entry is written last. That entry is the only pointer that makes them reachable. It names the snapshot, the side-file version, and the WAL slice removal. The entry that names a slice removal is not published unless the global minimum sequence is past that segment. When no compute is running, there is no stored minimum and the removal may be published. The side file and the data snapshot are not separately loadable. A slice is not gone unless that entry holds the snapshot of its rows. A later version is a different entry.

If the process crashes before the entry, the staged files are harmless. A reader whose pin is missing keeps the previous range. A crash does not look like data loss.

A vacuum job deletes storage objects not named by any live catalog entry. It waits a grace period, so it cannot delete a staged object before the catalog points at it.

Ingest acks are eventually consistent. The catalog does not wait on the source, and it does not block ingest. The catalog is a trailing WAL consumer. It publishes entries from committed segments. A query that pins a range sees that prefix as one set.

The offset high-water, the Postgres resume LSN in the Skipper store, and the deadletter offset hint are trailing WAL consumers. They are off the ingest path. Postgres is told the resume LSN the moment the WAL write is durable. That send does not wait for the store to consume the WAL. The store's Postgres offset trails. The offset high-water is not written into a catalog row, and the ack does not depend on it. `persist_offset_hint` does not fail the ingest request when the store write fails. The hint trails the WAL. A failed store write is not an ingest failure.

A later entry does not drop a key an earlier entry showed. The key is still in the WAL part of the entry, or it is in the snapshot published with the removal. The winner at the later entry `beats` or equals the winner at the earlier one.

The catalog reads committed WAL segments. The range boundary is `source_transaction_id` when the row has one, and `ingest_batch_id` when it does not. The two ids are not interchangeable. If the chosen start falls inside a batch, the start slides backward to that batch's first entry. When that start entry sits on a batch boundary and holds the tails of two batch ids, the start slides back to the earliest of their first entries. No overlapping batch is left out. The range is then that first entry through the tip, so it includes every committed entry of those batches. The tip does not move. There is no forward stretch past the latest committed entry. A pin cannot show a later row of a batch and omit its first entry.

Computes do not pin this range. They still apply the log in cursor order. Compaction does not reclaim a segment any running compute still needs. That hold is one global minimum, in [Computes stay off collapse](#computes-stay-off-collapse).

## Where each surface sits

| Surface | What it calls | What it does not call |
|---|---|---|
| Ingest | Stores `WalRowMeta`. On an update, uses `before`, else rejects the update and logs the error (new key, event id, reason: no before-image). Width and empty-token checks on the WAL write. Ack is eventually consistent. The catalog trails committed segments and does not block the ack. Postgres is told the resume LSN when the WAL write is durable. | `from_wal`, `beats`, `collapse`, an ack that waits until a catalog entry is listable, an ack that waits on the offset high-water, a Postgres LSN held for the Skipper store, a deadletter hint that fails the request, a cached last-seen key, an insert when the old key is unknown, a before-image rebuilt from the lake, a reject logged as an ordinary ingest error, a reject log that names an entity |
| Iceberg compaction | `from_wal`, `collapse`, `beats`, with the stored winner as incumbent. Commit checks the side-file version, reloads, re-runs `beats`, and retries when it is stale. One catalog entry pins that version with the slice removal and the snapshot (`BusinessKey → StoredWinner`). The slice removal is published only after the global minimum is past that segment. When no compute is running, there is no minimum and compaction proceeds. One run reads that minimum once and sizes only slices past it. | A private hex compare, a last-write-wins put, a lock around the shard, a snapshot load apart from the side file, compacting a segment the stored minimum has not passed, a store read of the minimum per slice |
| Warehouse compaction | `collapse`, then `beats` against the current row token, the current tombstone token, and the incoming event, in one transaction, published as one catalog entry. Act only on the winner. | A separate tombstone compare, SQL `ORDER BY` token, ordinal, removing a row that still wins, a result of tombstone 7 and no row when the row was at 10 |
| Other CDC sinks (S3, GCS, Azure, SFTP, Athena, file, stdout) | The shared projection for every change. Any encoding includes the mutation, the token, and the tie. | `collapse`, `encode_row`, an encoding that drops the tie |
| `skipprd query` and `skipprd serve` | Pin a contiguous range from a start entry through the tip. The tip is the latest committed entry. A start inside a `source_transaction_id`, or an `ingest_batch_id` when the source id is absent, rewinds to that batch's first entry. `from_wal`, `collapse`, and the same `beats` as Iceberg on that set. | A pin of one entry, a start left mid-batch, a tip stretched past the latest committed entry, a key-only anti-join, a second sidecar schema, a WAL list and a snapshot from two loads |
| Computes | The pre-split event pair. Dedup one `event_id` with `beats`, keeping the copy that wins. Apply every other event in WAL order. Each cursor commit stores the global minimum, the lowest sequence any running compute still needs. | `collapse`, a minimum sequence that lives only in memory, a store write on the ingest path |

## Computes stay off collapse

Stateless compute runs at ingest. It filters the `WalRowMeta` vec in step with the Arrow rows. The tie does not exist yet.

The stream processor applies every event that survives dedup, in WAL cursor order. An update retracts the old measures. A delete subtracts. That needs the full `MutationKind`, not an upsert. Collapsing first would leave a sum with only the last row.

Dedup is per `event_id`, and it uses `beats`. The copy with the greater token is the one applied. If that copy arrives after a lower token for the same id was applied, the processor retracts the lower one and applies the higher one. It does not keep the first copy it saw.

A key change is the shared projection above, not a compute-only retract. The delete of the old key exists because `from_wal` copied `before` onto the event. The projection copies the update's tie onto both halves after `from_wal` assigns address ties. The delete keeps the parent event id. The insert gets the id derived from that parent. Dedup does not drop one of them. The processor and `collapse` both see that delete and the insert of the new key. A rejected update is not applied.

The stored minimum is one global value, the lowest sequence any running compute still needs. A faster compute cannot raise it past a slower one. Each compute writes that global value in the same Skipper store transaction as its cursor, its measures, its last row, and its dedup. It is not a separate write, and it is not on the ingest path. Startup reads that stored minimum before it processes and before compaction may reclaim. A restart cannot drop a segment a running compute has not applied. When no compute is running, the stored minimum is absent and compaction is free to proceed.

While a minimum is stored, compaction must not remove or compact a WAL segment until that global minimum is past the segment. The catalog entry that names the slice removal is not published before then. Sizing and scheduling use the same minimum. Only slices past it are eligible. A slice at or behind it is ineligible. One compaction run reads the global minimum once from the Skipper store and keeps that value in memory for every candidate slice. It does not read the store once per slice. An update or an eviction of that in-memory copy is an atomic swap of the whole snapshot. No thread sees a partial update. Every compact thread sees one coherent minimum. When the minimum is absent, every slice is eligible.

`Latest` is not this model. Its order is user columns on the compute definition. Apply does not consult the CDC token.

## What this removes

- Query `MUT_*` codes, `TieBreak`, and the inline compare in `cdc_reconcile`.
- The key-only lake anti-join. A WAL row does not hide a lake row unless it `beats` the stored winner.
- Dropping the tie when a token is committed.
- Iceberg `compare_cdc_order_tokens`, its private winner struct, and a shard put that does not re-run `beats` after a reload.
- A side-file put that does not name the version the catalog entry will pin.
- A lock taken so two compact jobs cannot share a shard.
- The second file encoder (`plugins/data_sink/file/src/cdc_encode.rs` beside `plugins/shared/cdc_encode.rs`).
- `apply_op`. Warehouse compaction calls `collapse`.
- `encode_row`. Changelog sinks call the shared projection. An encoding of a row that omits the tie.
- Warehouse `CdcApplyMutation` ranking and `resolve_apply_action` as a separate winner. Both become `collapse` plus `beats`.
- A tombstone check that is not the same stored winner as the row token.
- Tombstone expiry, a cleanup tied to the oldest reader pin, and a size cap on the side file or the tombstone table.
- The SQL window that keeps the earlier ordinal on an equal token. A later tie wins, before commit and after it.
- Compute dedup that drops a later copy of an `event_id` without comparing tokens.
- A primary-key change interpreted inside the processor and not inside `collapse`.
- `from_wal` dropping `before`. A primary-key pair that shares an event id, or that keeps two address ties instead of the update's tie. An insert id that is not a deterministic function of the parent event id. A counter or a coordinated mint for that id. The source supplying `ingest_batch_id`.
- A processor minimum sequence that lives only in memory, or one minimum per compute. A faster compute raising the global minimum past a slower one. Reclaim during startup before the stored minimum is read.
- Compaction of a WAL segment the global minimum has not passed, while a compute is running. A catalog entry that names that slice removal. Sizing or scheduling a slice at or behind the minimum. A store read of the minimum for each slice. A partial in-memory minimum. Blocking compaction when no compute is running and no minimum is stored.
- An update accepted with no `before`. No insert of the new key, and no row written under the old key.
- A cached last-seen key used as the old key.
- A reject with no error log, or a log that omits the new key, the event id, or the reason. The reason is no before-image, and it is not an ordinary ingest error. A reject log that names an entity. The event has no entity id.
- A before-image rebuilt from the lake, the side file, or the warehouse.
- A WAL list and an Iceberg snapshot loaded as two steps.
- Reclaim of a slice before the snapshot that contains it is the pinned entry.
- A side file load that is not the same catalog entry as the data snapshot.
- An ingest ack that waits until a catalog entry is listable. The catalog waiting on the source, or blocking ingest.
- An offset high-water in the catalog row the ack depends on. A Postgres resume LSN held until the Skipper store consumes the WAL. A `persist_offset_hint` failure that fails the ingest request.
- A pin of a single catalog entry. A start left inside a `source_transaction_id`, or inside an `ingest_batch_id` used as the boundary. A start entry that holds two batch tails and rewinds only one of them. A tip stretched past the latest committed entry. Using one of those ids as the other. Dropping `ingest_batch_id` because a source transaction id was supplied.
- A second data source plugin or a second data sync. A catalog sequence per sink. An atomic append that does not assign the next sequence number.
- Athena running `beats`, or Athena reading a start-to-tip range. Iceberg reading that range. A range collapse that is not done on the Skippr side before the Iceberg snapshot is published.

## Concurrency this must survive

There is exactly one data source plugin and one data sync at a time. Batches are integer batches from that plugin.

The compactor runs many threads inside that one sync. `scheduler_jobs` is leftover cores while ingest is busy (`max(1, CPUs − active_ingest)`), or `2 × CPUs` (clamped 2 to 128) when ingest is idle, Drain, or Paused. Dest upload and multipart stay at machine caps unless a safeguard fires. Sink sessions are about 2 to 16.

A lane is `(sink, namespace)`. A second job on that lane is not refused. `blocked_lanes` is only set after a failure. Two slices of one namespace can compact at the same time. The side file is 256 shards by key hash, so two keys, or the same key in two slices, can hit one shard. They share it by version, in [Side-file versions](#side-file-versions).

Every catalog entry, from Iceberg compaction and from the warehouse, lands on one shared sequence. A single atomic append assigns the next sequence number. A range pin reads that sequence. The tip is the same entry for every sink. A start entry that holds the tails of two batch ids slides back to the earliest of their first entries, so neither batch is left split.

The stored minimum is loaded into memory once per compaction run. An update or an eviction of that copy is an atomic swap of the whole snapshot. Every compact thread sees one coherent minimum. No thread sees a partial update.

A platform run lock is what keeps a second `skipprd` process off the same pipeline. This loop does not. Readers do not see in-flight puts. They see a contiguous range of that shared sequence. Each Iceberg entry names one side-file version, its snapshot, and its slice removal.

Two warehouse transactions can apply one table at once. Each is one catalog entry on the shared sequence. Before a writer removes a row or applies a delete, it runs `beats` against the current row token, the current tombstone token, and the incoming event, and it acts only on the winner. A read of one slot, then a later write, must not drop a row that still wins. Tombstone 7 and no row, when the row was at 10, is not a correct answer.

## Side-file versions

Each catalog entry pins one side-file version. The version is the only name of that shard's bytes. Two entries do not share an unnamed object.

A compact writer reads a version, runs `beats`, and puts only if that version is still current. The check is at commit time. There is no lock before it.

If the version is stale, the put loses. The writer reloads the current version, runs `beats` against that committed state, and retries the put. The winner's tokens stay. The loser's candidates land only where they still win.

Two jobs on one shard cannot clobber each other. The second commit sees the first version and retries against it.

## Tests (TDD)

Write these red, then implement. Functional tests lock `beats`. DST scripts lock the query world. One end-to-end run locks ingest through Iceberg, query, and serve. The shard race and the warehouse SQL stay functional. They do not need a cloud job.

### (a) Equal token, committed tie kept

- Functional: `beats` — same token, incumbent already stored with the earlier tie, candidate has the later tie. Candidate wins. The same pair with both ties uncommitted picks the same row.
- DST: compact the earlier row, leave the later in the WAL, query expects the later row. The existing `b_equal_tokens_tie_break_seq` (both still in the WAL) expects that same row.
- End-to-end: not required. The DST pair is the regression.

### (b) Tombstone read by query

- Functional: `collapse` with a tombstone incumbent whose token beats the WAL upsert returns no row. The reverse, a newer upsert, returns the row.
- DST: tombstone stored, older upsert still in the WAL, query for that key is empty. A key-only hide would show the upsert; the stored incumbent does not.
- End-to-end: GitHub-hosted SkipprLake CDC jobs for Postgres, MySQL, and DynamoDB delete id 2 after snapshot and assert that key is absent after compact. Athena Iceberg late-delete is the Glue-path proof for Postgres.

### (c) Two Iceberg writers, one shard

- Functional: two persists of one shard. One writes a never-stored-key tombstone, the other an older upsert. The second put conflicts, reloads, runs `beats`, and the tombstone remains. Repeat with the upsert starting first and the tombstone arriving with a higher token. The tombstone wins.
- DST: one writer, tombstone then older upsert, stored winner is the tombstone. This does not replace the conflict test.
- End-to-end: none. Two cloud processes are not the race. The in-process jobs are.

### (d) Warehouse one stored token

- Functional: row token 10, tombstone 4, delete at 7. `beats` of that set keeps the row at 10. The writer does not remove it, and both slots remain 10, in one transaction. A result of tombstone 7 and no row fails. Flip `golden_contract_covers_mixed_replay_stale_composite_and_equal_tokens`: an equal token keeps the later tie, not the earlier ordinal.
- DST: none. The world does not run warehouse SQL.
- End-to-end: none.

### (e) Computes

- Functional: `from_wal` copies `before` onto `CdcEvent` and assigns ties from WAL addresses. The primary-key projection then copies the update's tie onto both halves. When `before` is present and the key differs, it emits a delete of the old key and an insert of the new key, same token and that shared tie. The delete keeps the parent event id. The insert id is that parent id plus an insert suffix, with no counter. The halves do not keep separate address ties. `collapse` of that pair drops the old key and keeps the new one. Dedup does not drop either half. An update with no `before` is rejected: no insert, no row under the old key, and one error log naming the new key, the event id, and the reason (no before-image). The log does not name an entity. That log is distinct from an ordinary ingest error. Nothing supplies the old key from a cache, the lake, the side file, or the warehouse.
- Functional: each cursor commit writes the global minimum in that same transaction, and the stored value stays the lowest sequence any running compute still needs. A faster compute does not raise it past a slower one. A restart reads it before processing. Compaction does not remove or compact a segment until that global minimum is past it, and the catalog entry that names that removal is not published before then. One compaction run reads the minimum once and does not size a slice at or behind it. When no compute is running, the minimum is absent and compaction proceeds.
- DST: `g_dedup_keeps_higher_token` — same event id, second amount higher, the measure follows the higher one and retracts the first. Today's `g_processor_dedup_event_id` keeps the first copy. `g_pk_change_matches_collapse` — processor totals and `collapse` survivors agree: old key 0, new key present.
- End-to-end: none.

### Changelog sinks

- Functional: the shared projection on two changes of one key writes both rows, each with the mutation, the token, and the tie. It does not call `collapse`. An equal-token pair replays in the order `beats` decided.
- DST and end-to-end: none. File and S3 copy jobs are not this check.
