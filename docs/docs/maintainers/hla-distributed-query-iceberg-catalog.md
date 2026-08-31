# HLA: Multi-Node Skippr — Clustered WAL, Flight SQL, and Iceberg Catalog

**Status:** architecture decisions locked  
**Implementation:** [`hla-implementation-wbs.md`](./hla-implementation-wbs.md)  
**Gaps:** [`hla-remaining-gaps.md`](./hla-remaining-gaps.md)  
**Flight SQL / Ballista:** [`hla-flight-sql-ballista.md`](./hla-flight-sql-ballista.md)  
**Style:** [`AGENTS.md`](../../../AGENTS.md) and [`repository-map.md`](./repository-map.md)

## Scope

This architecture adds:

- one exclusive ingest primary per pipeline;
- synchronous local-disk WAL and compaction-state replication;
- DynamoDB lease fencing and cluster-visible offsets/checkpoints;
- peer bootstrap, catch-up, promotion, and replica replacement;
- DDB cold discovery plus gossip membership/fence rumors;
- an embedded Iceberg DynamoDB catalog backed by R2/S3 metadata;
- Iceberg cold scans UNION live WAL on in-process Ballista, served over Arrow Flight SQL 58.3 (contract: [`hla-flight-sql-ballista.md`](./hla-flight-sql-ballista.md)).

The architecture preserves the current single-process ingest engine and runtime-plugin boundary. It does not allow concurrent writers for one pipeline.

## Locked decisions

### One configuration switch

`WAL_STORAGE` is a typed enum:

```text
disk       local WAL via PipelineDurableStore (PipelinePaths::legacy_disk), sled offsets, no cluster services
s3         existing S3 WAL behavior, no peer quorum or lease
clustered  local WAL via the same PipelineDurableStore + ClusteredWalStore, synchronous replica + DDB lease/offsets
```

`DiskWalStore` does not exist. Disk and clustered share one local-disk writer. Disk still starts no replica, gossip, or lease services.

`WAL_STORAGE=clustered` is the only cluster-mode switch. It reuses the existing `SKIPPR_OFFSET_DYNAMODB_TABLE` for offsets, leases, and membership. Product env (not impl knobs): `SKIPPR_CLUSTER_ID`, `SKIPPR_CLUSTER_GOSSIP_KEY`, cluster TLS PEM contents, and Flight session tenant/workspace. No lease, quorum, TTL, bind-address, or peer-list knobs. Iceberg `catalog.type: skippr` requires a separate customer-created `catalog.table`.

Cluster constants:

- lease timeout: 30 seconds;
- lease/membership heartbeat: 10 seconds;
- durable copies: two;
- write quorum: local prepared copy plus one caught-up remote Ack;
- replica TCP, Flight SQL, and Ballista gRPC: ephemeral ports, **always mTLS** (SAN `skippr-cluster`);
- gossip UDP: ephemeral port on the advertised IP (not `0.0.0.0` in `ChitchatId`).

Unknown `WAL_STORAGE` values fail startup. `clustered` requires a clustered offset backend: self-hosted `offset-store-dynamodb` (DynamoDB / DynamoDB Local) or Cloud `offset-store-cloud-tables`. An explicit offset backend that is neither DynamoDB nor Cloud tables conflicts with `clustered` and fails startup.

### Cloud vs OSS catalog and control state

| Concern | OSS / HLA harness | Skippr Cloud |
|---------|-------------------|--------------|
| Offsets, leases, membership | DynamoDB table `SKIPPR_OFFSET_DYNAMODB_TABLE` (DynamoDB Local in HLA) | Cloud **tables** (`offset-store-cloud-tables`) |
| Iceberg catalog pointers | Separate DynamoDB `catalog.table` | Separate Cloud **tables** table (MUST NOT equal the offset table) |
| Iceberg warehouse / parquet | `file://` or S3 | **objects** / R2 |
| Replica RPC, Flight SQL, Ballista gRPC | **Always mTLS** (HLA mints a throwaway CA; SAN `skippr-cluster`) | Same PEMs; mesh only |
| Gossip | Authenticated Chitchat (WU-5.1); `SKIPPR_CLUSTER_GOSSIP_KEY` required | Same; no tenant-string fallback |
| Query tenant | Flight `Authorization: Basic {tenant}/{workspace}` on every RPC | Cloud **gateway** maps JWT `tenant_id` (D31) to that header; skipprd does not verify JWTs |
| Cloud tables mesh auth | n/a (DynamoDB Local) | Host GuestCredentialBroker (`CLOUD_SYSTEM_BROKER_CONFIG`). MUST NOT `CLOUD_TABLES_ACCESS_TOKEN` / `CLOUD_ACCESS_TOKEN` |

DynamoDB Local remains the OSS HLA harness. Cloud guests MUST NOT hairpin public `*.cloud.skippr.io` (D37).

### Process model

- One process runs at most one ActivePrimary ingest pipeline at a time.
- A process may hold durable replicas and serve query sockets for many pipelines.
- Ingest looks up the ActivePrimary `PipelineDurableStore` (else the sole installed store). It does not resolve the store by `Config::get_pipeline_name()`. Clustered `engine::run_sync_pipeline` sets `PIPELINE_NAME` for that lease only.
- Replica/query/catch-up code is keyed by explicit `(tenant, workspace, pipeline)` and never reads process-global `PIPELINE_NAME` or `Config::get_data_dir()`.
- File sources keep the lease after scan complete (mtime wait). Other finite sources drain, `release_after_drain`, and let the scheduler try the next pipeline. An indefinite source retains the lease until fence.
- One clustered process owns one `DATA_DIR`. Multiple processes on a host require separate existing `DATA_DIR` values.
- Process identity is a UUID v4 generation. Host failure-domain identity is derived from Kubernetes/ECS/EC2 metadata, `/etc/machine-id`, then hostname.
- A synchronous replica must have a different host identity.
- Continued writes after one node fails require three live processes: promoted replica plus a new synchronous replica. Two nodes preserve data but cannot restore quorum after either is lost.

`clustered sync --once` and clustered discover are rejected. `clustered sync` is the long-lived cluster node. `clustered query` is a query client and never acquires an ingest lease.

### Authority boundaries

- DynamoDB lease item: ownership and fencing authority.
- Mutation-log quorum: WAL and compaction hard-state authority.
- DynamoDB offset/checkpoint items: cluster-wide source ingest gate after quorum.
- Shared object storage: schema and Iceberg data/metadata.
- Iceberg catalog pointer CAS: lakehouse table authority.
- DDB membership: cold endpoint discovery only.
- Gossip: live membership, suspicion, endpoint hints, fence rumors. Transport is authenticated Chitchat (`src/cluster/gossip.rs`, WU-5.1). Cluster id is `SKIPPR_CLUSTER_ID` (not tenant/workspace). HMAC key is `SKIPPR_CLUSTER_GOSSIP_KEY` (required).
- Logical tenancy: `PipelineKey` `{tenant, workspace, pipeline}` on every lease/offset/WAL/replica frame. Flight SQL session `TenantScope` on every RPC. Cluster membership is **not** a tenant.
- Status RPC: peer head/health truth. `StatusOk.head_hash` is empty (genesis) or exactly 32 bytes; other lengths are skipped / `ProtocolMismatch`.
- Auth workspace run-lock: CLI product concern only; never ingest ownership.

## Implementation status

Clustered ingest, replica protocol, Dynamo leases/offsets/catalog, in-process Iceberg∪WAL UNION, and the three-process DynamoDB Local harness are in tree. Process checklist ticks stay in [`hla-e2e-checklist.md`](hla-e2e-checklist.md) until `python3 tests/hla_e2e/run.py` is actually run. Dynamo CAS ticks wait on `python3 tests/hla_e2e/ddb_cas.py`.

**Still deferred:** first GitHub Actions HLA run. Flight SQL 58.3, in-process Ballista 53, and authenticated Chitchat (WU-5.1) are in tree; live process proof is `python3 tests/hla_e2e/run.py`. Contract: [`hla-flight-sql-ballista.md`](./hla-flight-sql-ballista.md).

Live membership is Chitchat. The e2e red-team items from 2026-08-13 are closed in [`hla-e2e-implementation-gaps.md`](hla-e2e-implementation-gaps.md).

## Cluster identity and DDB layout

### Pipeline rows

```text
PK = {tenant}#{workspace}#{pipeline}

SK = lease
  owner_node
  epoch
  heartbeat
  released
  initialized

SK = offset#{namespace}#{partition}
  payload_b64
  payload_sha256
  wal_epoch
  wal_commit_index

SK = checkpoint#{logical_key}
  payload_b64
  payload_sha256
  wal_epoch
  wal_commit_index
```

Lease rows are never deleted, so epochs never reset.

### Cluster membership rows

Cold discovery is cluster-scoped. One skipprd cluster serves many tenants.

```text
PK = cluster#{cluster_id}

SK = node#{node_uuid}
  host_id
  host_label
  replica_addr
  flight_addr
  gossip_addr
  heartbeat
  protocol_min
  protocol_max
  capabilities
```

Shipped: `flight_addr` on each membership `node#` row is Arrow Flight SQL 58.3. There is no `query-scheduler#` SK. `clustered query` opens Flight SQL on a ready `flight_addr`; the serving node plans Iceberg∪WAL UNION on the gossip-elected Ballista cluster. Stale membership rows are verified by mTLS + `cluster_hash(SKIPPR_CLUSTER_ID)`, never a DDB clock.

Cold discovery queries the exact `cluster#{cluster_id}` PK. It never scans the table. Stale rows are verified by handshake and ignored.

## Clock-free lease protocol

DynamoDB does not expose a server clock for condition expressions. The lease must not use `lease_until < client_now`.

### Acquire

- Missing item: conditional create, epoch 1, heartbeat 1, `initialized=false`.
- Released item: conditional owner swap, epoch increment, heartbeat reset.
- Held item: strongly read `(owner, epoch, heartbeat)`, observe it unchanged for the full 30-second timeout using local monotonic time, strongly read again, then conditional-swap the exact observed tuple.
- Any concurrent renew changes heartbeat and makes the steal fail.

### Renew

- Conditional on owner, epoch, and `released=false`.
- Increment heartbeat.
- Local lease deadline is measured from the monotonic instant when the request starts, not when its response arrives.
- A late/lost response may shorten ownership but never extends it unsafely.

### Release

- Release is conditional and preserves epoch.
- It is allowed only after source, compactor, sink calls, WAL commits, and offset publication are quiescent.
- On uncertain shutdown, do not release; stop renew and allow unchanged-heartbeat takeover.

### Local fencing

`LeaseGuard` states are Idle, OwnerElect, ActivePrimary, and Replica. ActivePrimary contains either single-node authority or a leased session. Every clustered durable write checks its local monotonic deadline. A higher observed epoch demotes Active/Elect and updates replica `epoch_seen`.

Demotion stops source intake, rejects new compaction/sink work, and blocks WAL/offset commits. Replica and query services remain available.

## Clustered offset/checkpoint contract

DynamoDB offsets are required in clustered mode because immutable sources such as S3 consult `Closed=1` before reading an object. A new primary cannot safely use an empty local sled database.

Commit order:

1. local epoch check;
2. local WAL prepare;
3. remote durable apply/Ack;
4. local commit/apply;
5. conditional DynamoDB offset/checkpoint publication;
6. ingest Ack.

Never publish `Closed` before WAL quorum.

Clustered offset publication:

- strongly read the prior item;
- merge Closed with OR and Position/Filesize with max;
- conditionally write against the exact prior WAL tuple/hash;
- retry on conditional race;
- same tuple/same hash is idempotent;
- same tuple/different hash is corruption;
- older tuple cannot overwrite newer.

Partial multi-key publication is recoverable. Ingest is not acknowledged until all keys publish. Startup/promotion reconciles through the committed WAL head with strongly consistent reads before constructing the source plugin.

## Durable mutation log

### Envelope

Every mutation carries:

- protocol version;
- pipeline key;
- lease epoch;
- per-pipeline commit index;
- previous entry hash;
- typed body;
- payload hash when present.

Entry hash is SHA-256 over previous hash, canonical protobuf envelope, and payload hash. Repeated fields are sorted. This creates a verifiable chain. `entry_hash()` returns `Result`; encode failure is `DurableError`, never empty placeholder bytes.

### Mutations

```text
CommitSegment
  segment descriptor and payload hash/length
  sorted offsets
  checkpoint envelopes

PutCompaction
  complete portable transaction
  Pending, Sent, Acked, or Tombstoned

CompleteSlices
  compaction id
  segment ids and ordinals

ReclaimSegment
  segment id
```

There is no separate Append mutation. Segment payload and `CommitSegment` form one logical operation.

There is no schema or Glue catalog-outbox mutation.

### Portable WAL identity

Clustered disk refs use:

```text
wal://{tenant}/{workspace}/{pipeline}/{segment_id}
```

Absolute disk paths are never serialized or included in clustered compaction IDs. Local paths are resolved from explicit `PipelinePaths` rooted at:

```text
{DATA_DIR}/clustered/{encoded_tenant}/{encoded_workspace}/{encoded_pipeline}
```

Legacy `disk` layout remains `{DATA_DIR}/segment_buffer/...` via `PipelinePaths::legacy_disk`. Disk writes go through the same `PipelineDurableStore` as clustered. First clustered migration installs a tenant-scoped baseline under the new root.

### Durable states

- Prepared: payload and mutation record fsynced locally.
- Committed: quorum decision fsynced.
- Applied: segment marker/manifest/ledger/reclaim side effect completed.
- OffsetPublished: all offset/checkpoint rows reconciled.
- IngestAcked: source-facing success returned.

The primary fsyncs Prepared before sending. Prepared/Committed/STATE/segment/commit-marker durability I/O is Direct I/O (`O_DIRECT` on Linux, `F_NOCACHE` on macOS) plus `fdatasync`. Recovery and compaction read those files the same way, so commit/ACK/sink never trust the kernel page cache. `fsync`/`fdatasync` failure poisons that file handle (no retry on the same fd) and fails the persist: clustered fences ingest; disk does not ACK the source. Cloud skipprd guests (XFS) must succeed at Direct I/O — clustered WAL volumes do not fall back to the page cache. SQL live WAL is a separate buffered `File::open` path and may be served from the page cache; it is not a durability vote.

The replica verifies payload, appends/fsyncs Prepared and Committed, applies, then Acks. The primary records Committed, applies, publishes offsets, then Acks ingest.

A prepared record with unknown outcome is retained. Recovery queries peer Status/hash; matching remote commit finalizes it locally. If no peer proves the Prepared hash, keep the record, return `QuorumLost`, and do not activate ingest (`abort_pending_prepared` is not used for this case). Orphan segment bytes alone are never promoted to committed state.

Log `compare` of a missing historical envelope at `base < index < committed` is `Diverged`, not `AlreadyAppliedSameHash`. Pruned indexes at or below `base_index` may still be treated as already applied.

## Compaction and sink safety

Clustered pipelines require a sink capability whose `SinkRetrySemantics` supports idempotent replay.

Compaction sequence:

1. quorum `PutCompaction(Pending)`;
2. build grouped stream;
3. quorum `PutCompaction(Sent)`;
4. call sink with deterministic compaction ID/idempotency key/portable refs;
5. quorum `PutCompaction(Acked)` after sink success;
6. quorum `CompleteSlices`;
7. quorum Tombstoned and Reclaim mutations.

On ambiguous Sent recovery, sink preflight decides whether the apply already happened. AtLeastOnce and NonRetryable sinks are rejected in clustered mode because a crash after external success but before Acked cannot provide an exactly-once guarantee.

`CompactionIndex` and in-memory caches are a cache of the snapshot-plus-suffix-log SoT, not leftover compaction JSON. Complete transaction manifests, completion ordinals, and reclaim decisions are replicated.

## Replication protocol

Transport is TCP with protobuf control frames and streamed raw payload chunks.

- Control/mutation metadata maximum: 16 MiB.
- Payload chunk: 1 MiB.
- Oversized metadata causes the ingest buffer to split before commit.
- Payload is streamed to a temp file; no full segment allocation.
- Length, hash, deadlines, bounded queues, and partial-frame handling are mandatory.
- One mutation per pipeline is in flight.

RPCs:

- handshake (`ClientHello` / `ServerHello`);
- Status;
- AssignReplica;
- InstallSnapshot;
- FetchEntries;
- Replicate;
- DropReplica.

`CURRENT_PROTOCOL` / `PROTOCOL_MIN` / `PROTOCOL_MAX` are **2**. `ClientHello` carries cluster hash (SHA-256 of `ClusterId`), node id, and protocol range. Tenant/workspace are not on hello (`reserved 5, 6, 7`). The TCP connection is mTLS. The server verifies `cluster_hash` and protocol overlap. Each later frame carries a `PipelineKey`. The client verifies `HelloOk.cluster_hash` and `HelloOk.protocol` is in `[PROTOCOL_MIN, PROTOCOL_MAX]`.

Replica assign:

- `ReplicaSession::ready` starts `false`.
- Assign records `assigned_primary` and `observe_epoch` so `epoch_seen` tracks the assigned primary epoch.
- Unknown pipeline Assign: Nack, leave `ready=false`.
- Unparseable `primary_endpoint`: Nack, leave `ready=false`.
- Catch-up success sets `ready=true`; catch-up failure leaves `ready=false`.

Replica acceptance:

- not assigned: reject;
- stale epoch (`epoch < epoch_seen` or `epoch != assigned epoch`): reject;
- gap: NotCaughtUp;
- same index/same hash: idempotent Ack;
- same index/different hash: Diverged and quarantine;
- next index with matching previous hash: persist/apply/Ack.

The local fsynced Prepared record counts as one vote; one caught-up remote Ack completes quorum two.

Live replicate Timeout, I/O, or other non-definite errors keep the Prepared record and fence ingest (`abort_pending_prepared` is not used). Definite replica rejects (stale epoch, not caught up, disk full, pinned, diverged) abort Prepared.

## Replica placement, snapshots, and catch-up

- Target one remote replica per pipeline.
- Choose by rendezvous hash over pipeline and healthy node generation.
- Exclude self, same host ID, incompatible protocol, insufficient disk, lagging/corrupt state.
- Unmeasurable volume (`available_bytes` is `None`) is under pressure and is not a placement target.
- Membership ads are hints; Status proves eligibility.
- In-flight commits capture an immutable peer-set snapshot.

Catch-up: if local `committed_index` is ahead of the donor, fail `DurableError::Diverged` (do not treat “ahead” as success). Empty `head_hash` is genesis; non-empty must be 32 bytes (`replica_status_hash`). Install a snapshot when the requester predates donor `base_index`.

Reclaimed payloads prevent infinite log replay. A state snapshot contains:

- base index/hash;
- live segment inventory and hashes;
- live payloads;
- non-tombstoned compaction transactions;
- completion bitmaps and compaction/ref mapping for live segments;
- latest offset/checkpoint values with commit tuples;
- schema fingerprints on live segment descriptors (coverage against `metadata.json` at promote; not a second schema SoT).

If a requested index predates retained history, bootstrap installs a verified state snapshot and live payloads in a staging directory, fsyncs, then atomically replaces pipeline state. Incremental entries follow. After prune, that snapshot is the mutation log; the compaction planner reads the same snapshot-plus-suffix-log SoT.

A corrupt/unusable replica stops advertising and purges only that pipeline. A healthy replica does not voluntarily leave until replacement is caught up.

## Promotion

1. Acquire/steal lease and enter OwnerElect.
2. Gossip higher epoch.
3. Poll reachable Status endpoints.
4. Catch up to the highest hash-consistent committed head. When that donor has an RPC endpoint, catch-up always hash-checks (equal index still verifies hash; local committed ahead of the donor is `Diverged`). Non-32-byte Status hashes are skipped; empty may be genesis.
5. Reject same-index hash disagreement.
6. Reconcile DynamoDB offsets/checkpoints.
7. Load object-store `metadata.json` (steal already waited one `LEASE_TIMEOUT` observing the previous epoch; do not wait again), install plugin schema state, and refuse activate if the document is missing on an initialized pipeline or any live WAL namespace is absent from it.
8. Assign/bootstrap a new synchronous replica.
9. Enter ActivePrimary and start source/compactor.

An initialized pipeline with no reachable hard state fails `NoHeadAvailable`. Head zero is allowed only while the lease item has `initialized=false`.

The first clustered owner creates a baseline snapshot from existing drained disk state, seeds DDB offsets/checkpoints, bootstraps a remote, writes the cluster-format marker, and then conditionally marks the lease initialized.

## Membership and gossip

DDB / Cloud tables membership provides cold seeds. Live membership is authenticated Chitchat (`src/cluster/gossip.rs`, WU-5.1): liveness/suspicion, bounded ads, endpoints, protocol, replica head hint, capacity/readiness, and fence rumors.

Gossip never:

- grants a lease;
- commits a mutation;
- advances an offset;
- applies hard state.

Suspicion starts lease observation. It does not immediately promote.

## Schema

Skippr pipeline metadata JSON (`{tenant}/{workspace}/{pipeline}/metadata/metadata.json` on the configured object store) is the schema source of truth. It is not replicated in the mutation log. Live segment descriptors carry schema fingerprints; promotion decodes live `.seg` files (the snapshot-plus-suffix-log inventory) for namespaces and checks those namespaces against the document.

Promotion cannot enter ActivePrimary until:

1. **Previous writer is gone.** Steal is that wait: `acquire_pipeline` observes the previous `(owner, epoch, heartbeat)` unchanged for one `LEASE_TIMEOUT` before CAS. That is the previous epoch’s TTL. Promotion MUST NOT sleep another `LEASE_TIMEOUT` before GET `metadata.json`. Catch-up MAY run after steal and before GET. First create, self-renew, and acquire-after-release have no previous writer.
2. **The document is installed.** Load `metadata.json`, store it as in-memory pipeline metadata, build `ARROW_SCHEMA` from it, and install `RuntimeSchemaState` so runtime sources/sinks receive that document before the source or compactor start. WAL Arrow IPC is a decode check of live `.seg` files, not the schema SoT.
3. **Live WAL is covered.** Every namespace in live WAL MUST appear in `metadata.json`. An initialized pipeline, or any pipeline with live WAL, fails closed (`SchemaMissing`) if the document is missing or unreadable. Unreadable live `.seg` files also fail closed. Uninitialized genesis with no live WAL MAY activate without the object.

WAL fingerprints need not equal the current JSON fingerprint after schema evolution: JSON is a superset. The gate is namespace coverage plus decode, not hex equality with the latest Arrow schema.

In-flight Iceberg `Sent` apply uses that installed document. A process-local `schema_version` stamp on a pending receipt MUST NOT block commit after promote.

## Iceberg DynamoDB catalog

YAML `catalog.type` is `skippr` (Skippr-managed Iceberg catalog). DynamoDB is the storage implementation, not the product name. Customers create and name two tables: `SKIPPR_OFFSET_DYNAMODB_TABLE` (offsets, leases, membership) and Iceberg `catalog.table` (catalog pointers). Those are distinct product tables. HLA e2e creates two DynamoDB Local tables (`skippr-hla-e2e-offsets` and `skippr-hla-e2e-catalog`). Catalog pointer keys remain `catalog#{warehouse_hash}` on the catalog table.

Writer path uses an in-process implementation of the complete `iceberg::Catalog` trait. A REST facade is not part of v1.

Catalog table key shape:

```text
PK = catalog#{sha256(normalized warehouse URI)}
SK = namespace#{encoded namespace}
  properties
  table_count

PK = catalog#{warehouse hash}#namespace#{encoded namespace}
SK = table#{table name}
  table_uuid
  metadata_location
  previous_metadata_location
  generation
```

Namespace/table names use reversible length-prefixed encoding.

Catalog requirements:

- all namespace methods;
- all table methods;
- transactional create/drop/rename with namespace table count;
- register existing metadata;
- validate every `TableRequirement`;
- apply every `TableUpdate`;
- write new metadata JSON to R2 before conditional pointer CAS;
- bounded OCC retry;
- tolerate orphan metadata after lost CAS.

Iceberg snapshot summaries preserve Skippr compaction ID, idempotency key, schema fingerprint, WAL-ref fingerprint, and ref count.

## Query architecture

### Iceberg cold provider

Skippr implements a read-only DataFusion `TableProvider` directly over the vendored Iceberg `TableScan::to_arrow()` API. Published `iceberg-datafusion` versions compatible with the needed API exceed Rust 1.88 or target another DataFusion/Iceberg combination.

Iceberg catalog schema is authoritative. WAL batches project/cast into it; missing evolved columns are NULL-filled; incompatible evolution fails.

### Live WAL (unpinned, eventually consistent)

Iceberg is the only snapshot. WAL is a best-effort tail of that snapshot: it MAY omit not-yet-compacted rows. A later query MAY see more WAL. Writes never observe query state.

`select_live_ordinals` is the only selector. Live segments are `StateSnapshot.segments` ∪ suffix `CommitSegment`, minus `ReclaimSegment`, minus Iceberg `skippr.wal-segment-ids`. For each remaining segment, skip ledger-complete ordinals. If the ledger is unreadable or the file is gone, skip that ordinal (do not scan it, do not fail the query).

`CompleteSlices` is applied into the ordinal ledger; the selector does not re-parse those mutations. Iceberg-named segment ids close the window after the lake pointer commit and before completion quorum.

There are no query pins. `ReclaimSegment` un-owns by deleting `.seg.commit` first, then drops `.seg`, even if a scan is in flight; that scan skips the missing file. Readers MUST NOT nack reclaim.

### Query transport

**Shipped:** Arrow Flight SQL 58.3 on membership `flight_addr` (`src/query_flight`). Framed `QuerySocketServer` is deleted. Ballista 53 runs in-process on every clustered node. Contract: [`hla-flight-sql-ballista.md`](./hla-flight-sql-ballista.md).

Locked here so other HLA docs do not drift:

- Skippr SQL extension remains `live_wal_scan(tenant, workspace, pipeline, namespace, exclude_segment_ids)` as an internal sole-FROM statement, not a public custom ticket.
- DoGet re-selects `select_live_ordinals` at execution time (unpinned). Handles MUST NOT freeze ordinals.
- Flight SQL is read-only. Foreign tenant/workspace and unknown pipeline fail closed.
- `CURRENT_PROTOCOL` is **2** (cluster hello). There is no query-planner protocol on DynamoDB.
- No new address/port/scheduler knobs. Dual framed+Flight protocols are forbidden.
- Each clustered `skipprd` node runs Flight SQL plus a Ballista scheduler and executor. Gossip elects one scheduler (minimum `NodeId` among published `scheduler` addrs). `clustered query` talks Flight SQL to any ready `flight_addr`. WAL is a one-partition `FlightSqlExec` to the serving node's best-effort max advertised `committed_index` (local on tie if this process holds paths → this process's advertised `flight_addr`). Iceberg parquet may run on any executor.
- UNION stays a dumb concat before aggregate/join. Iceberg-only is a valid behind read.

### UNION

**Shipped:** `IcebergWalUnionProvider::scan` builds a DataFusion `UnionExec` of the Iceberg plan and the live WAL plan, with `GlobalLimitExec` on the union (not eager collect with Iceberg-only limit). UNION is a dumb concat: it MUST NOT DISTINCT, wait, or pin. Catalog listing keeps only prefix-matching tables (`catalog_table_to_namespace`); an empty second pipeline MUST NOT fail the first.

For each table:

1. load the Iceberg table and scan the current snapshot (fail closed if none);
2. pass `skippr.wal-segment-ids` from snapshot ancestry as WAL exclude ids;
3. scan live WAL ordinals via one `FlightSqlExec` DoGet to the picker winner's advertised `flight_addr`;
4. if no WAL replica is reachable, return Iceberg only.

Gossip ads are not a query cut. Replica WAL row counts are not voted. Iceberg-only is a valid behind read.

**Shipped (WU-7.5):** Ballista 53 **inside** each clustered node joins one elected scheduler and executes UNION before aggregate/join. WAL child is always `FlightSqlExec` to the picker winner. Prost codec carries endpoint, statement SQL, and Arrow schema. Details: [`hla-flight-sql-ballista.md`](./hla-flight-sql-ballista.md).

## Lifecycle

On fence or SIGTERM:

1. stop acquiring pipelines;
2. stop source intake;
3. reject new WAL/sink operations;
4. resolve/time out in-flight durable commits;
5. drain compactor and runtime plugin source;
6. drain catalog outbox;
7. release only after full quiescence, otherwise let lease observation expire (`release_after_drain` errors leave the lease held; do not retry-release);
8. stop query/replica/gossip services after in-flight requests drain.

Non-file finite sources that complete `Ok(())` drain, `release_after_drain`, and return so the scheduler can try the next pipeline. File sources keep today’s mtime wait and remain primary until fenced.

Lease renew is `renew_until_lost`. Clustered Dynamo lease connect failure fails `sync` startup.

Fence of one ingest pipeline does not stop replica or query services.

## Security

Replica RPC and the query socket v1 assume a trusted private routed network and restrictive security groups.

- Every handshake verifies tenant/workspace cluster identity and protocol range.
- Query exposes only configured cluster pipelines.
- TLS/token authentication is a later version and is not implied by ephemeral ports.

## Failure behavior

- DDB unavailable: renew fails, local deadline fences primary.
- Replica unavailable or Timeout: quorum lost; keep Prepared and fence ingest on Timeout/unknown RPC; definite Nacks abort Prepared. No offset publication until Committed.
- Offset publication fails after quorum: committed WAL remains; pipeline pauses/reconciles before source restart. Promotion reconciles offsets while still OwnerElect (`require_offset_epoch`, not WAL `require_active_epoch`), then activates.
- Old primary alive: local deadline/fence stops commits; replicas reject stale epoch.
- Hash disagreement: quarantine/fail promotion.
- All head copies lost: no promotion.
- Disk full/corruption: node becomes ineligible; primary replaces it before new writes. Unmeasurable free space is treated as disk pressure.
- Sink success with lost Acked mutation: idempotent preflight resolves.
- Query peer failure: skip that replica; use another Flight endpoint or Iceberg-only.
- Iceberg CAS conflict: reload/retry; orphan metadata is safe.
- Dynamo conditional writes detect `ConditionalCheckFailedException` via SDK error `code()`, not substring match.

## Success criteria

Shipped:

- Never two durable ActivePrimary writers for one pipeline.
- Every acknowledged clustered segment has two durable copies.
- No lease DDB operation occurs per flush.
- Every acknowledged segment has cluster-visible offsets/checkpoints.
- WAL, compaction transactions, completion state, and reclaim survive failover.
- Promotion requires hash-consistent state, schemas, reconciled offsets, and a synchronous replica.
- No absolute disk path appears in clustered durable identity.
- Iceberg snapshot reads are consistent. Live WAL is eventually consistent and unpinned; UNION must not double-count Iceberg-named or ledger-complete slices (in-process `UnionExec` over Iceberg Rust + local WAL or `FlightSqlExec`).
- Iceberg pipelines never fall back to Parquet listing.
- Disk and clustered share one local-disk writer (`PipelineDurableStore`).
- Deterministic failure, DDB contract (`tests/hla_e2e/ddb_cas.py`), subprocess crash, protocol-version, and query-consistency tests exist in the harness (process ticks wait on `run.py`).

Shipped for query (contract: [`hla-flight-sql-ballista.md`](./hla-flight-sql-ballista.md)):

- Live WAL uses Arrow Flight SQL 58.3 `GetFlightInfo`/`DoGet`; framed query socket is gone.
- Ballista 53 is compiled into `skipprd` and starts in `run_clustered`. UNION is planned before downstream aggregate/join.

## Non-goals

- Sled lease store.
- Concurrent primary ingest pipelines in one process.
- Configurable quorum/TTL/ports/node ID in v1.
- Clustered S3 WAL.
- DDB commit-index hint for peer-head selection.
- S3 WAL archive for disk catch-up.
- Schema JSON replication.
- Glue catalog-outbox replication.
- AtLeastOnce/NonRetryable sinks in clustered mode.
- Partial distributed query results.
- Iceberg listing fallback.
- Iceberg REST facade in v1.
- Historical Iceberg time-travel UNION with reclaimed WAL.
- TLS/token authentication in v1.
