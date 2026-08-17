# HLA e2e red-team: gaps closed

**Style:** [AGENTS.md](../../../AGENTS.md) (compile-time invariants, simplest design, DRY) and the WBS completion rules in [hla-implementation-wbs.md](hla-implementation-wbs.md).  
**Closed:** 2026-08-13. Proof: `python3 tests/hla_e2e/run.py` (exit 0) plus the unit tests named below.

This is not a Ballista/Flight-protocol backlog. Query transport is Arrow Flight SQL 58.3 (`src/query_flight`); remaining live proof is `python3 tests/hla_e2e/run.py`. Contract: [hla-flight-sql-ballista.md](hla-flight-sql-ballista.md) (WU-7.3 / WU-7.5).

## What the harness now proves

| Scenario name | What runs |
|---|---|
| `bootstrap_three_nodes` | Two processes, then a primary log line. Spare is not in the initial quorum. |
| `ingest_and_query_union` | File JSONL → Iceberg parquet + WAL; `SELECT id` / `count(*)` unique via `clustered query` (Flight SQL). |
| `ballista_cluster_query` | Every ready `flight_addr` returns the same unique ids and `count(*)`; live nodes share `elected_scheduler=`. |
| `query_from_every_ready_replica` | One `clustered query` client; WAL may be behind. |
| `late_node_wal_catchup` | n3 start, SIGKILL replica, catch-up log, restore spare. |
| `primary_kill_failover` | SIGKILL primary, batch2, promote, unique ids. |
| `sigkill_during_ingest_no_duplicate_rows` | Restore three nodes, arm `after_prepared` on the primary, copy batch3, abort after local Prepared, promote, unique `{prior ids} ∪ {evt-9, evt-10}`. |

`HostId` is still the failure domain (`KUBERNETES_NODE_NAME=hla-host-{1,2,3}`). That is correct product behaviour.

---

## 1. Prepared/Commit crash in the e2e binary — closed

Named point `FailpointName::AfterPrepared`. The e2e binary arms it by writing that name to `{PipelinePaths.root}/failpoint`. The file is consumed once; unknown names abort. No cluster env knobs.

`commit_segment` / `commit_mutation` call `failpoint::hit(FailpointName::AfterPrepared, &self.paths.root)` after local Prepared and before replica Commit. In the release/e2e binary that aborts the process. In-process tests return `DurableError`.

**Tests:** `cluster::failpoint::*`, `python3 tests/hla_e2e/test_run.py`, scenario `sigkill_during_ingest_no_duplicate_rows` in `python3 tests/hla_e2e/run.py` (primary exit code -6 / SIGABRT). In-process `recover_unknown_prepared` remains in `tests/hla_cluster.rs`.

## 2. Dynamo offset read errors are not “never ingested” — closed

`Offsets::try_get` / `get` / `snapshot_value` / `validate` return `Result<_, OffsetsError>`. Store I/O is `OffsetsError::Store`, not `None`. Ingest `process_batch` fails the request on snapshot error. Offset-service `should_process` is false on `Err`. Source `partition_already_closed` fails closed when the offset service does not answer.

**Tests:** `store_read_error_is_not_missing`, `store_error_does_not_process`, `closed_check_fails_closed_on_offset_service_error`.

## 3. One Dynamo offset write protocol — closed

`insert` and `upsert` share `merge_offset` / `apply_offset_field` (in-place; Position insert uses max). `insert_dynamo` is gone. `put_bytes` get-merges offset SKs (`Closed` OR, Position/Filesize max) and preserves an existing `wal_epoch` / `wal_commit_index` fence under OCC. `publish_wal_commit` remains the fence-advancing WAL path.

**Tests:** `insert_position_preserves_closed`, `put_bytes_merges_offsets_so_closed_survives_position_write`, `merge_offset_or_closed_and_max_position`.

## 4. Harness Prepared-window crash — closed

`sigkill_during_ingest_no_duplicate_rows` arms `after_prepared`, copies `batch3.jsonl`, waits for the primary to abort, waits for promote, then asserts unique ids including `evt-9` and `evt-10`. It is not a renamed uniqueness re-check.

---

WU-7.3 (Arrow Flight SQL 58.3 GetFlightInfo/DoGet) and WU-7.5 (elected Ballista 53 cluster) are in tree. Live distributed proof is `python3 tests/hla_e2e/run.py` (`ballista_cluster_query`). Contract: [hla-flight-sql-ballista.md](hla-flight-sql-ballista.md).
