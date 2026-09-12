---
title: Change data capture
description: Skipprd CDC converges on the correct final table state using order tokens, tombstones, and warehouse reconciliation.
---

# Change data capture

CDC in Skipprd is not a dump of the source log. Supported paths carry enough order and deletion information to converge on the table as it should be in the destination.

Sources declare a CDC mode in their execution contract:

| Mode | Behaviour |
|---|---|
| `snapshot` | Bounded snapshot only. No CDC stream. |
| `snapshot_then_cdc` | Snapshot, checkpoint, then native CDC. Later runs resume and skip the snapshot. |
| `cdc_only` | Native CDC stream only. |

Ingest treats CDC batches as first-class. Position (for example a Postgres LSN) is part of resume state. The WAL is still the durable boundary: a change is real once it is committed there, then compacted into the sink with a stable `compaction_id`.

## Next

- [Guarantees](guarantees.md) — order tokens, tombstones, reconciliation
- [Exactly-once](/concepts/exactly-once)
- [PostgreSQL source](/connectors/inputs/postgres)
