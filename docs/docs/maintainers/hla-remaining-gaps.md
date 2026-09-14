# HLA remaining gaps (scratch notes)

Working notes for finishing the multi-node implementation. The architecture spec remains [hla-distributed-query-iceberg-catalog.md](hla-distributed-query-iceberg-catalog.md), Flight SQL / Ballista [hla-flight-sql-ballista.md](hla-flight-sql-ballista.md), and the WBS [hla-implementation-wbs.md](hla-implementation-wbs.md). Do not treat Cursor plan todos as done.

**As of 2026-08-17 (production cluster close-out):** Cluster identity is `SKIPPR_CLUSTER_ID` (membership `PK=cluster#{id}`). Clustered replica / Flight SQL / Ballista gRPC are always mTLS. Gossip HMAC (`SKIPPR_CLUSTER_GOSSIP_KEY`) is required. Wire protocol is **2**. Flight SQL requires `Authorization: Basic tenant/workspace` on every RPC. Skippr Cloud clustered path sets `SKIPPR_OFFSET_STORE=cloud-tables` and uses Skippr Cloud default credentials.

HLA process e2e (`python3 tests/hla_e2e/run.py`) must be re-run after this cutover (mTLS + protocol 2 + cluster id). Do not treat the pre-cutover 2026-08-17 pass as current.

**Still deferred:** first GitHub Actions HLA run; Preview three-host smoke (`cloud/docs/internal/datalake-preview-smoke.md`). Do not treat unit codec round-trip as a substitute for `run.py`.

Process e2e already run vs still required: [hla-e2e-checklist.md](hla-e2e-checklist.md).

---

## Definition of done (do not mark a phase complete until)

- Clustered `sync` on three processes: one primary, one sync replica, one spare; kill primary; writes resume on the promoted replica after a new replica is assigned. **Three-process failover e2e exists.** Same-host processes still cannot be replicas (`HostId` is the failure domain). **Harness `same_host_exclusion` pass.**
- Iceberg sink create/load/commit works against DynamoDB Local + object store FileIO. **Local catalog table (`skippr-hla-e2e-catalog`) + file:// warehouse e2e exists.** Offsets/leases use `skippr-hla-e2e-offsets`.
- `SELECT` UNION of Iceberg snapshot + live WAL returns each source row once. **e2e asserts unique ids and matching count over Flight SQL.** Iceberg ancestry carries compacted WAL segment ids. Contract: [`hla-flight-sql-ballista.md`](hla-flight-sql-ballista.md).
- Crash between Prepared and local Commit recovers from the replica without duplicating offsets. **e2e binary aborts on `{pipeline root}/failpoint` = `after_prepared`; harness copies batch3 and asserts unique ids. In-process `recover_unknown_prepared` remains.** Additional failpoints (`after_replica_ack`, `after_local_commit`, `after_offsets_published`, `replica_after_prepared`, `after_compaction_sink`) have process scenarios.
- Default host build still has no DynamoDB SDK. **True** (`check_host_dependency_boundaries.py` pass via local-react wrapper).
