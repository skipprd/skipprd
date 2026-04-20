# Runtime Plugin Contract

This is the hard contract for Skippr's runtime plugin boundary after the TCP cutover.

## Core rules

1. **WAL commit is the only durable boundary.** A batch becomes durable when the host makes the committed WAL segment visible.
2. **The offsets DB is host-owned materialized state.** It reflects visible WAL commits. It is not the source of truth and it is not owned by plugins.
3. **Runtime transport is TCP only.** Every runtime plugin opens one control channel and one data channel, then authenticates both with `RuntimeSessionHello`.
4. **Source plugins never mutate host durability state.** They read resume state from the host, emit batches, emit offset hints, and emit checkpoints. The host owns WAL writes and offset materialization.
5. **Sink and schema work must be replay-safe.** The host may replay the same logical compaction after reconnect, restart, or crash. `compaction_id` is the stable idempotency key for that work.

## Transport shape

- **Control channel**: handshake, installs, schema refresh requests, offset RPC, checkpoint updates, completion, and errors
- **Data channel**: Arrow payloads and source-emitted batch data
- **Frame encoding**: length-prefixed bincode frames shared by host and SDK
- **Session model**: child connects to host listeners published through env vars, then sends the shared session token on both sockets

There is no stdio compatibility path. Runtime plugins now speak the same TCP protocol in local tests and in production.

## Ownership split

### Host owns

- WAL persistence and visibility
- offsets DB writes
- recovery and WAL replay
- sink/schema orchestration
- schema state distribution

### Source plugins own

- reading external systems
- producing checkpoints and bootstrap anchors
- emitting prepared batches, raw batches, or sink-write requests
- asking the host to validate resume state

### Sink and schema plugins own

- destination-specific writes
- destination-specific DDL/schema sync
- replay-safe handling of repeated compaction work

## Recovery contract

On restart, the host recovers from committed WAL state first and rebuilds the rest from there:

1. Scan committed WAL segments.
2. Re-materialize offsets/checkpoint view from committed WAL state.
3. Re-run any remaining compaction work.
4. Resume sources from host-provided checkpoint and materialized offsets.

The offsets DB is therefore a cache of host-owned durable progress, not an authority on its own.

## Crash matrix

| Crash point | What must be true after restart |
| --- | --- |
| Before committed WAL visibility | The batch is not durable yet. The source resumes from the last materialized host state. |
| After committed WAL visibility, before offsets materialization | WAL replay re-materializes offsets and re-queues compaction. |
| After offsets materialization, before sink/schema ack | The host may replay the same compaction with the same `compaction_id`. |
| After sink/schema ack, before WAL cleanup/tombstone | Replaying the same compaction must still be safe and idempotent. |
| After WAL cleanup/tombstone | Recovery skips that compaction because the durable work is already complete. |

## Practical implications

- Request acknowledgements (`request_id`) are transport/session level and used to match replies.
- `compaction_id` is the cross-retry semantic identifier and must remain stable for replay.
- Source plugins must be able to reconnect without owning any local durable state beyond their normal source-specific checkpoint logic.
- Tests should exercise disconnect, crash, schema refresh, and replay paths against the TCP transport rather than a helper-only compatibility path.
