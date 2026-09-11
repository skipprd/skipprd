# HLA e2e checklist

**Architecture:** [hla-distributed-query-iceberg-catalog.md](hla-distributed-query-iceberg-catalog.md)  
**Flight SQL / Ballista:** [hla-flight-sql-ballista.md](hla-flight-sql-ballista.md)  
**WBS:** [hla-implementation-wbs.md](hla-implementation-wbs.md)  
**Closed red-team items:** [hla-e2e-implementation-gaps.md](hla-e2e-implementation-gaps.md)

Harness: `python3 tests/hla_e2e/run.py` (DynamoDB Local with two tables — `skippr-hla-e2e-offsets` and `skippr-hla-e2e-catalog` — plus `file://` Iceberg warehouse and three `skipprd` processes with `KUBERNETES_NODE_NAME=hla-host-{1,2,3}`). Query path is Arrow Flight SQL 58.3 (`query_flight_ids`). Clustered TCP is always mTLS; hello is protocol **2** hashed from `SKIPPR_CLUSTER_ID`. Live `run.py` over Flight SQL is the remaining process gate.

**This pass:** 2026-08-17 production close-out (cluster id, always mTLS, gossip HMAC required, Flight `TenantScope`, mesh JWT). Previous `run.py` pass is **stale** until re-run. Commands used `cargo` unless noted.

| Bucket | Ran | Outcome |
|---|---|---|
| Process harness (`run.py`) | not this pass | **stale** — must re-run after protocol 2 / mTLS / `SKIPPR_CLUSTER_ID` |
| Harness contract (`test_run.py`) | this pass | 14 tests (includes protocol-2 hello + cluster id) |
| Dynamo CAS (`ddb_cas.py`) | contract in `test_run.py` | live `python3 tests/hla_e2e/ddb_cas.py` still required against DynamoDB Local |
| WBS CI cargo tests / `check --all-features` | this pass | re-run after close-out |
| Host-boundary Python script | prior pass | **pass** (`cargo`; default host tree has no `aws-sdk-dynamodb`) |
| GitHub Actions HLA job | workflow in tree | `.github/workflows/hla-e2e.yml` exists. **Do not tick green** until an Actions run exists |
| Product runtime e2e | not this pass | needs AWS / `SKIPPR_API_KEY` |
| Preview three-host smoke | not this pass | `cloud/docs/internal/datalake-preview-smoke.md` |

---

## Process e2e

Re-run: `python3 tests/hla_e2e/run.py`. The 2026-08-17 pre-cutover pass (36 scenarios) is not proof of the mTLS / protocol 2 / cluster-id harness. Tick scenarios only after that re-run.

- [ ] `bootstrap_three_nodes` — n1+n2 start; one becomes primary; spare is not in the initial quorum
  - **Ran:** harness scenario. Copied `batch1.jsonl`; started n1 pid=45489, n2 pid=45490.
  - **Result:** pass (scenario advanced).
  - **Remark:** Does not assert “spare excluded from quorum” beyond starting only two processes. n3 is not started until catch-up.

- [ ] `ingest_and_query_union` — `batch1.jsonl` → Iceberg parquet + WAL; `SELECT id` / `count(*)` unique (`evt-1`…`evt-5`)
  - **Ran:** harness scenario.
  - **Result:** pass. Log: `queried 5 rows from Iceberg UNION live WAL`.
  - **Remark:** `clustered query` Flight SQL UNION. Ballista cluster proof is `ballista_cluster_query`.

- [ ] `ballista_cluster_query` — every ready `flight_addr` returns the same unique ids and `count(*)`; live nodes share `elected_scheduler=`
  - **Harness:** after ingest, n1+n2 still ingesting; `query_flight_ids` + `query_flight_count` on each ready membership `flight_addr`.
  - **Ran:** 2026-08-17 `run.py`. Log: `shared elected_scheduler=127.0.0.1:56868`; both Flight addrs returned ids `evt-1`…`evt-5` count=5.
  - **Result:** pass.

- [ ] `query_from_every_ready_replica` — clustered query succeeds (WAL may be behind)
  - **Ran:** harness scenario.
  - **Result:** pass (no disagreement vs prior ids).
  - **Remark:** Still one client, not a socket per replica. The per-replica item below remains open.

- [ ] `late_node_wal_catchup` — start n3; SIGKILL sync replica; catch-up log; restore spare; ids unchanged
  - **Ran:** harness scenario. Started n3 pid=45783; SIGKILL n2; restarted n2 pid=45834.
  - **Result:** pass.
  - **Remark:** Catch-up is incremental (late node after five rows), not a snapshot-boundary catch-up.

- [ ] `primary_kill_failover` — SIGKILL primary; `batch2.jsonl`; survivor promotes; unique `{evt-1`…`evt-8}`
  - **Ran:** harness scenario. SIGKILL n1 pid=45489; promoted n2.
  - **Remark:** Old primary is dead, not partitioned. Does not prove late Dynamo write from a live old primary.

- [ ] `sigkill_during_ingest_no_duplicate_rows` — arm `{pipeline root}/failpoint` = `after_prepared`; `batch3.jsonl`; primary abort; promote; unique `{evt-1`…`evt-10}`
  - **Ran:** harness scenario. Armed `/tmp/skippr-hla-e2e/node2/clustered/hla-e2e/local/hla_events/failpoint`; n2 exit code `-6` (SIGABRT); promoted n3.
  - **Result:** pass.
  - **Remark:** Covers **after local Prepared, before replica send**. Later commit-boundary crashes are separate harness scenarios (`after_replica_ack`, `after_local_commit`, `after_offsets_published`, `replica_after_prepared`).

Harness contract (not a cluster):

- [ ] `python3 tests/hla_e2e/test_run.py`
  - **Result:** pass, 7 tests, 0.001s.

---

## Supporting tests (not process e2e)

- [x] `cargo test -p skipprd --features offset-store-dynamodb --test hla_cluster`
  - **Result:** pass, 22 tests, 0.21s.
  - **Remark:** Includes `prepared_without_commit_recovers_from_replica` (in-process replica apply) and `crash_after_commit_before_publish_recovers_committed_prefix` (**state-machine model**, not a subprocess abort).

- [x] `cargo test -p skipprd --features offset-store-dynamodb --lib failpoint`
  - **Result:** pass, 4 tests.

- [x] `insert_position_preserves_closed`
  - **Result:** pass.

- [x] `store_read_error_is_not_missing` / `store_error_does_not_process` / `closed_check_fails_closed`
  - **Result:** `store_error_does_not_process` and `closed_check_fails_closed` pass (re-run this pass: runtime-sdk closed_check pass). `store_read_error_is_not_missing` was pass on the prior pass; not re-filtered this pass (same binary, offsets tests still compiled).
  - **Remark:** `commit_segment_publishes_closed_offsets` pass inside `buffer::durable::` (22 tests).

- [x] `cargo test -p skippr-offset-store-dynamodb`
  - **Result:** pass, 4 tests (merge Closed/Position; SK formats). No live Dynamo I/O.

- [x] `cargo test -p skippr-runtime-sdk closed_check_fails_closed`
  - **Result:** pass.

---

## Additional process e2e

Each item: ran the closest existing test if any. `[ ]` means the **process** scenario is still missing.

### Durable commit boundaries (WU-2.3 / WU-3.4)

- [ ] After replica Ack, before local Committed
  - **Ran:** `after_replica_ack` failpoint + `batch5.jsonl`; primary abort; promote; unique ids through `evt-14`.
  - **Result:** pass.

- [ ] After local Committed / apply, before Dynamo offset publish
  - **Ran:** `after_local_commit` failpoint + `batch4.jsonl`; primary abort; promote; unique ids through `evt-12`.
  - **Result:** pass. In-process `crash_after_commit_before_publish_recovers_committed_prefix` remains as the integer model.

- [ ] After offset publish, before ingest Ack
  - **Ran:** `after_offsets_published` failpoint + `batch6.jsonl`; unique ids through `evt-16`.
  - **Result:** pass.

- [ ] Replica SIGKILL during payload replication (primary keeps Prepared; recovery vs uncommitted)
  - **Ran:** `replica_after_prepared` failpoint on the replica + `batch7.jsonl`; replica abort; promote; unique ids through `evt-18`.
  - **Result:** pass.

- [ ] Truncated / STATE-mismatch mutation log on restart
  - **Harness:** `truncated_log_restart` (2026-08-17 `run.py` reached this scenario).
  - **Result:** pass (harness advanced). In-process `truncated_tail_is_dropped` remains.

- [ ] ENOSPC (or equivalent fail-closed disk-full) during Prepared
  - **Harness:** `enospc_during_prepared` (2026-08-17 `run.py`).
  - **Result:** pass (harness advanced). Failpoint is fail-closed, not a real ENOSPC errno.

### Failover and fencing (WU-4.2 / WU-4.4)

- [ ] Partitioned old primary after promote: in-flight flush and late Dynamo write must not overwrite the new epoch
  - **Ran:** `sigstop_old_primary` — SIGSTOP primary, promote, `batch9.jsonl`, unique through `evt-22`; SIGCONT then query must not duplicate; SIGKILL in `finally`.
  - **Result:** pass. Does not inject a late Dynamo write from the frozen process; uniqueness after resume is the process assertion.

- [ ] Promotion with no replacement replica available (two live nodes): writes stop until a third process restores quorum
  - **Ran:** `two_node_quorum_stall` — SIGKILL both non-primaries, copy `batch10.jsonl`, assert `evt-23`/`evt-24` absent, start one spare, catch-up, unique through `evt-24`.
  - **Result:** pass.

- [ ] Hash-conflict / initialized head loss fails closed (no implicit majority)
  - **Harness:** `hash_conflict_no_majority` (2026-08-17 `run.py`).
  - **Result:** pass (harness advanced).

- [ ] Restart generation: killed node returns with a new process UUID and is not treated as the old replica
  - **Ran:** `restart_generation` (2026-08-17 `run.py`).
  - **Result:** pass (harness advanced).

### Catch-up and replacement (WU-4.1 / WU-4.3)

- [ ] Catch-up across a snapshot boundary (requester predates donor base)
  - **Ran:** `snapshot_catchup_fourth_node` (2026-08-17 `run.py`).
  - **Result:** pass (harness advanced).

- [ ] Donor failure mid catch-up, then retry
  - **Harness:** `donor_kill_mid_catchup` (2026-08-17 `run.py`).
  - **Result:** pass (harness advanced).

- [ ] Corrupt replica stops ads and purges only its pipeline
  - **Harness:** `corrupt_replica_purge` (2026-08-17 `run.py`).
  - **Result:** pass (harness advanced).

### Compaction / Iceberg sink (WU-2.6 / WU-6.3)

- [ ] Compaction crash after sink success, before `PutCompaction(Acked)`; restart is `AlreadyApplied`, no duplicate parquet
  - **Ran:** `after_compaction_sink` failpoint + `batch8.jsonl`; parquet count after recovery is not a duplicate-file blow-up; unique ids through `evt-20`.
  - **Result:** pass.

- [ ] Compaction crash before sink; replay once
  - **Ran:** `before_compaction_sink_replay` (2026-08-17 `run.py`).
  - **Result:** pass (harness advanced).

- [ ] Clustered AtLeastOnce / NonRetryable sink rejected at startup (process-level, not only unit)
  - **Ran:** `clustered_rejects_at_least_once` starts `skipprd clustered sync` with a Stdout sink; non-zero exit; message includes idempotent-replay rejection.
  - **Result:** pass. Unit `clustered_rejects_at_least_once_sink` remains.

### Query consistency (WU-7.2 / WU-7.4)

- [ ] Query socket on each ready replica (WAL may differ; Iceberg is the snapshot)
  - **Harness:** `query_each_replica_socket` (2026-08-17 `run.py`).
  - **Result:** pass (harness advanced).

- [ ] Pointer-before-completion and completion-before-query: UNION has each source row once
  - **Ran:** ingest UNION uniqueness throughout the harness (pass), including immediately after compaction of `batch10`. Iceberg snapshot ancestry now carries `skippr.wal-segment-ids`; live WAL excludes those segments. `select_live_ordinals` skips ledger-complete and Iceberg-named segments.
  - **Remark:** WAL may be behind. Eventual uniqueness after compaction is `wait_unique_ids`.

- [ ] Query skips an unreachable replica; Iceberg-only is allowed
  - **Harness:** `query_retry_lagging` (2026-08-17 `run.py`).
  - **Result:** pass (harness advanced).

- [ ] Schema evolution: new column null-filled on live WAL, no double-count
  - **Ran:** `schema_evolution` — `SELECT id, region FROM hla_events` after `batch4`; unique ids, no double-count.
  - **Result:** pass.

### Query protocol (WU-7.3 / WU-7.5 in tree; live `run.py` still the gate)

- [ ] Arrow Flight SQL 58.3 `GetFlightInfo` / `DoGet` (WU-7.3) — unit/lib tests in `query_flight::`
  - **Remark:** Framed `query_socket` is deleted. Live process proof is `python3 tests/hla_e2e/run.py` via `query_flight_ids`.

- [ ] Ballista 53 elected cluster UNION (WU-7.5) — codec + `start(advertised_ip)` + UNION-before-aggregate unit tests
  - **Remark:** No standalone Ballista OS processes. Live distributed proof is `ballista_cluster_query` in `python3 tests/hla_e2e/run.py`.

### Lifecycle (WU-8.1 / WU-8.2)

- [ ] SIGTERM at idle, ingest, quorum, sink, and catch-up; lease released only after drain
  - **Ran:** `sigterm_drain` SIGTERM of the live primary after stall recovery; process exits; remaining nodes still query unique ids.
  - **Result:** pass for idle-after-ingest. Does not matrix SIGTERM during in-flight ingest/quorum/sink/catch-up.

- [ ] Failed drain leaves the lease held
  - **Harness:** `failed_drain_holds_lease` (2026-08-17 `run.py`).
  - **Result:** pass (harness advanced).

- [ ] Two pipelines: at most one ActivePrimary; replica/query continue for the other
  - **Harness:** `two_pipelines` (2026-08-17 `run.py`).
  - **Result:** pass (harness advanced).

- [ ] Rolling replica replacement across current/previous protocol
  - **Harness:** `rolling_protocol_replacement` injects Dynamo membership `protocol_min=max=3`; dummy addr is never `AssignReplica`; protocol-2 spare is. `CURRENT_PROTOCOL` is 2. `mixed_protocol_handshake` remains Hello reject.
  - **Result:** stale until re-run after protocol-2 / mTLS cutover.

### Discovery / membership (WU-5)

- [ ] Cold start from Dynamo membership only (no gossip seeds)
  - **Harness:** `cold_start_membership` is Dynamo-seeded.
  - **Ran:** 2026-08-17 `run.py` (full matrix, exit 0).
  - **Result:** pass. Gossip is authenticated Chitchat (WU-5.1).

- [ ] Cross-cluster / incompatible-protocol handshake rejected
  - **Ran:** `cross_cluster_handshake` in 2026-08-17 `run.py`; also `mismatched_cluster_identity_is_rejected` and `protocol_intersection_requires_overlap`.
  - **Result:** pass.

- [ ] Same-host replica exclusion asserted by the harness (`HostId`), not only by config
  - **Ran:** `same_host_exclusion` starts two processes with `KUBERNETES_NODE_NAME=hla-same-host`; neither pair forms primary+replica (`AssignReplica` absent on both).
  - **Result:** pass. Unit `same_host_is_never_selected` remains.

---

## CI / contract commands

- [x] `cargo test -p skippr-lease`
  - **Result:** pass, 17 tests (lease create/renew/steal/fence, paths). Memory store, not Dynamo lease CAS.

- [x] `cargo test -p skipprd --features offset-store-dynamodb --lib cluster::`
  - **Result:** pass, 52 tests.

- [x] `cargo test -p skipprd --features offset-store-dynamodb --lib buffer::durable::`
  - **Result:** pass, 22 tests.

- [x] `cargo test -p skipprd --features offset-store-dynamodb --lib query_flight::`

- [x] `cargo test -p skipprd --features offset-store-dynamodb --lib sqlrt::wal_table::`
  - **Result:** pass, 2 tests (`provider_is_pinned_to_namespace_and_commit_cut`, `empty_wal_scan_honors_projection`). No live UNION race.

- [x] `cargo check --all-features`
  - **Result:** pass, ~17s (`cargo check --all-features`).

- [x] `python3 .github/scripts/check_host_dependency_boundaries.py`
  - **Result:** pass. Script invokes `cargo` (or `$CARGO`). Default host tree excludes `aws-sdk-dynamodb`.

- [ ] Deterministic DynamoDB Local job for lease, membership, offset CAS, and Iceberg catalog OCC
  - **Harness:** `python3 tests/hla_e2e/ddb_cas.py` (offset table `skippr-hla-e2e-offsets`, catalog table `skippr-hla-e2e-catalog`; no skipprd ingest). Workflow step is wired before `run.py`.
  - **Remark:** Re-run required after membership PK `cluster#{id}` cutover. First GitHub Actions HLA run is still deferred.

- [ ] `python3 tests/hla_e2e/run.py` in GitHub Actions
  - **Ran:** workflow at `.github/workflows/hla-e2e.yml` (`workflow_dispatch` + HLA-path PRs). Not hidden behind `fast_release`.
  - **Remark:** Do not tick until an Actions run exists. Pre-cutover local `run.py` (plaintext protocol 1) is stale.

Product runtime e2e (`python3 .github/scripts/runtime_e2e_harness.py`, GitHub release `doctor` / `discover` / `sync`) was not run; it needs AWS credentials and is a separate bar.
