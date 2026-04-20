# Exactly-Once Delivery

Skippr's exactly-once contract starts at the WAL and stays host-owned all the way through recovery.

## Durable boundary

The only durable boundary is a **visible committed WAL segment**.

That means:

- a source batch is durable once the host has written it to the WAL and made the commit visible
- the offsets database is **not** the source of truth
- sink progress is not the primary durability ledger

Exactly-once output therefore depends on two things working together:

1. **WAL-first durability** in the host
2. **Replay-safe sink behavior** when compaction work is retried

## WAL

Every ingested record is written to the WAL before downstream compaction and destination writes. WAL segments can be stored on local disk (`WAL_STORAGE=disk`) or S3 (`WAL_STORAGE=s3`).

If Skippr crashes, recovery starts from committed WAL state, not from in-memory progress.

## Offsets database

The durable offsets database is stored on local disk at `DATA_DIR`, but it is owned by the host process only.

Its role is to materialize the latest source positions that are already represented by committed WAL state. In other words:

- offsets represent **WAL-visible progress**
- runtime source plugins do not mutate the DB directly
- runtime source plugins read resume information from the host over the runtime protocol

This keeps the host as the only authority for durable ingest progress.

## Compaction and sinks

After data is durable in the WAL, the host compacts that data and sends destination work to sink and schema plugins.

Compaction can be retried after crashes or reconnects, so sink-side work must be replay-safe. Skippr uses stable `compaction_id` values so repeated work can be identified and handled idempotently where the destination supports it.

## Crash recovery

When Skippr restarts after a crash:

1. The host scans committed WAL segments.
2. The host re-materializes offset and checkpoint state from committed WAL progress.
3. Any remaining compaction work is replayed.
4. Runtime source plugins resume from host-provided checkpoint and offset state.

The key invariant is: **WAL recovery is the source of truth, and the offsets DB is a host-owned cache of that truth**.

## Clean shutdown

On clean shutdown, the host drains in-flight WAL work before exit. If that drain fails, the process exits non-zero so the run is treated as untrusted and recovery will replay from WAL on the next start.

## Chaos mode

Skippr includes a built-in chaos mode (`SKIPPR_CHAOS_MODE=yes`) that injects random SIGKILL signals during ingestion. This is used to validate the WAL-first recovery model under crash conditions.
