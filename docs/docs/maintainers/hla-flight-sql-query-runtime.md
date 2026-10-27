# HLA: Arrow Flight SQL 59.2 and datafusion-distributed

**Status:** implemented in `skipprd` (Flight SQL 59.2 + in-process QueryRuntime (datafusion-distributed); Iceberg Rust). HLA e2e over Flight SQL is the remaining live gate.  
**Architecture index:** [`hla-distributed-query-iceberg-catalog.md`](./hla-distributed-query-iceberg-catalog.md)  
**Work units:** WU-7.3 and WU-7.5 in [`hla-implementation-wbs.md`](./hla-implementation-wbs.md)  
**Style:** [`AGENTS.md`](../../../AGENTS.md)

This file is the requirements and design contract for replacing the framed-TCP query socket with Arrow Flight SQL 59.2 and executing Iceberg∪WAL UNION on datafusion-distributed **inside every clustered `skipprd` process**. Iceberg Rust (vendored `third_party/iceberg`) is compiled into `skipprd`. Replica RPC `CURRENT_PROTOCOL` is **3** (cluster hello, not tenant hello).

Implementation: GitHub issue 16 (df-dist QueryRuntime spike).

Shipped: every clustered `skipprd` process serves Arrow Flight SQL 59.2 on membership `flight_addr`, starts one in-process QueryRuntime Worker, and plans Iceberg∪WAL UNION with the datafusion-distributed planner. Iceberg Rust (vendored `third_party/iceberg`) is compiled into `skipprd`. Standalone query-engine binaries are deleted. Dual query protocols are forbidden. `query-scheduler#` is not used.

## Requirements

### Functional

1. Every clustered node MUST serve Arrow Flight SQL 59.2 on the advertised `flight_addr` (wildcard bind, ephemeral port, no new address knobs).
2. Query pins a read boundary `(S, E, W)` at plan time in `IcebergWalTableProvider::scan`. Local: pin `W` (`WalCatalogReader::live_slices`), then Iceberg current snapshot `S`, then ancestry-wide exclude set `E` (`skippr.wal-slices` across snapshots, each `seg@start`), freeze `PinnedWal::Local`. Clustered: pick `flight_addr` at scan (`pick_wal_endpoint`), pin `W` (`skippr.live_wal_pin`), then `S`/`E`, freeze `PinnedWal::Clustered`. Catalog registration MUST NOT freeze `flight_addr`. `ReadBoundary` is `S ∪ (W \ E)`. No current Iceberg snapshot is `LakeSnapshot::Empty` (WAL rows still return). `WalScanExec` MUST NOT re-select live ordinals at execute. The pin is a query-local ordinal list, not a reclaim lease. Tickets MUST NOT nack `ReclaimSegment`. A missing payload after pin is `WalPinLost`. `query_collect` and `clustered_query_collect` replan at most `QUERY_PIN_RETRY_CEILING` (3) times, then fail. Every frozen boundary (local, clustered, and WAL-only `ReadBoundary::pin_wal`) emits one `query_pin` PERF event with `snapshot`, `wal_slices` (`|W \ E|`), and `excluded_slices` (`|E|`).
3. Clustered live WAL is a typed `LiveWalTicket` (prost) on `FlightSqlExec`, not SQL. The coordinator pins `W` with Flight action `skippr.live_wal_pin` (`LiveWalPinRequest` / `LiveWalPinResponse`), then loads Iceberg `S`/`E`, then DoGets exact slices. There is no `live_wal_scan` / `iceberg_scan` TVF, no SQL-string exclude list, and no client-supplied head cut. The old property `skippr.wal-segment-ids` is not read.
4. Flight SQL is read-only. INSERT, UPDATE, DELETE, CREATE, DROP, ALTER, MERGE, and CommandStatementUpdate MUST fail closed.
5. `LiveWalTicket` / `skippr.live_wal_pin` tenant/workspace MUST equal the Flight session `TenantScope` (`Authorization: Basic {tenant}/{workspace}` on **every** RPC). Foreign identity is a typed rejection, not an empty scan. Unknown pipeline is a typed rejection. Cluster membership is **not** a tenant.
6. SkipprLake catalog listing for UNION registration lists tables in that pipeline's `table_namespace` (see `catalog_table_to_namespace`). Isolation is the Iceberg namespace, not a table name prefix. A pipeline with no catalog tables yet MUST NOT fail registration of another pipeline in the same session.
7. One table provider (`IcebergWalTableProvider`) serves Iceberg plus uncompacted WAL. Append WAL is concat. CDC WAL collapses by Arrow-row business keys and `order_token` (max token wins; equal tokens break ties by `_skippr_cdc_seq` / ordinal / row). Query CDC keys are `query_namespace_business_keys` (`PipelineType::Cdc` only) → `CdcPipelineConfig::resolve_contracts` → `resolve_business_keys` on that pipeline's YAML `cdc` plus `cdc_source_key_columns` from `bound.get_metadata()` (`view.key`). Missing persisted metadata is empty `source_contracts`; YAML keys still apply. A CDC pipeline whose source or sink has not declared capabilities, or that resolves no business keys, fails ingest startup (no silent append). Runtime `source_contracts` publish reinstalls `PIPELINE_CDC_CONTRACTS` via `reinstall_resolved_contracts` before persisting metadata (empty resolve fails closed and leaves the prior map and prior metadata). Guarantees are copied only for the same default/named identity admitted at ingest start; a newly named namespace keeps `CdcEncoded`. `register_namespace_view` re-resolves keys every call (no skip when the table already exists). Query MUST NOT read `PIPELINE_CDC_CONTRACTS`. Compaction looks up `pipeline_cdc_contracts(&work.pipeline)` where `work.pipeline` is `pipeline_backend::pipeline_key(config)`. WAL null keys fail closed; lake null keys are kept. Unknown mutation codes fail closed. Compacted leftovers are excluded by Iceberg `E` at pin, not by dropping snapshot WAL in merge. Anti-join changed keys out of Iceberg, then union surviving insert/update/snapshot rows. Deletes suppress the key. v1 has no before-image: a mutated business key is correct only if the source emitted delete(old)+insert(new). Slice-done bits MUST NOT hide an ordinal until its `(segment_id, start)` is in `E` or the `SegmentRow` is reclaimed. A missing WAL file after pin is `WalPinLost`. Unreachable WAL fails the query.
8. datafusion-distributed MUST execute that merge **before** aggregate, join, sort, or limit that is not pushed into a scan. Iceberg parquet MAY run on any QueryRuntime Worker (shared warehouse; `IcebergScanExec` reloads from `catalog_json`). Live WAL is always a one-partition `FlightSqlExec` DoGet of a `LiveWalTicket` to the advertised `flight_addr` of the picker winner chosen at `IcebergWalTableProvider::scan` (best-effort max `catalog_seq_seen` with `payload_ready`, including when the winner is this process). Catalog registration MUST NOT freeze that address. The merge (`UnionExec` or `CdcReconcileExec`) sits on the serving node above those leaves. `WalTableProvider` / `WalScanExec` MUST NOT enter a distributed plan (catalog rows and payload bytes stay on the owner). `WalScanExec` runs only inside the owner's `LiveWalTicket` DoGet, which exact-gets Index+Segment per ticket ref (`WalPinLost` on miss) and MUST NOT re-select `live_slices`. Unsafe predicate/projection pushdown stays off for CDC until merge finishes.
9. Every clustered `skipprd` node is a query node: in-process QueryRuntime Worker bind first, then Flight SQL on `flight_addr`, then gossip. `query_context()` uses the distributed planner and `WorkerResolver` over gossip `worker` addrs (including self; replica `ready` is not a gate). Clustered SELECT fails closed if QueryRuntime is not started. `clustered query` opens Flight SQL on any ready `flight_addr`. The serving node plans the merge. `SK=query-scheduler#{generation}` is **not** used.
10. Iceberg metadata is loaded with in-process Iceberg Rust `Catalog` on the serving node. Load/scan setup failure fails the query. Never fall back to parquet listing or a peer Iceberg Flight.
11. Partial distributed results are forbidden. Executor or replica failure retries a typed alternative (next WAL head) or fails the query. Returning a subset of UNION partitions is not allowed.

### Non-functional

- Versions are the workspace pins: Arrow **59.2** (`arrow-flight` with `flight-sql`), DataFusion **55.2**, datafusion-distributed **4.0**, Iceberg Rust via `third_party/iceberg`. Do not take a published Iceberg-DataFusion crate that breaks those pins.
- No query/lease/quorum/TTL/bind/peer-list knobs. Flight SQL, QueryRuntime worker, replica, and gossip all bind ephemeral ports.
- Default host `skipprd` builds still MUST NOT pull `aws-sdk-dynamodb`. `arrow-flight`, `datafusion-distributed`, and `iceberg` MAY be default-host dependencies. Dynamo catalog stays `offset-store-dynamodb`.
- Clustered replica TCP, Flight SQL, and QueryRuntime worker gRPC MUST use mTLS (SAN `skippr-cluster`). Gossip UDP uses HMAC with required `SKIPPR_CLUSTER_GOSSIP_KEY`.
- Every Flight SQL RPC MUST carry `Authorization: Basic {tenant}/{workspace}`. Cloud JWT is verified only at the Cloud gateway (D31); skipprd enforces the resulting `TenantScope`.
- Query must not nack `ReclaimSegment`. The `(S, E, W)` pin is a query-local ordinal list, not a reclaim lease. In-flight DoGet that loses a file is `WalPinLost`.
- Metrics already named `cluster_flight_request` / `cluster_flight_fail` apply to Flight SQL (keep the names).

### Non-goals

- Custom public WAL Flight tickets.
- Holding a reclaim lease or nacking `ReclaimSegment` from query (plan-time ordinal lists are local to the scan).
- Guaranteeing the globally latest WAL (advertised max is best effort).
- Historical Iceberg time-travel UNION with reclaimed WAL.
- Standalone `skippr-ballista-scheduler` / `skippr-ballista-executor` processes.
- Configurable executor count, batch size, or scheduler port.
- Chitchat (WU-5.1 remains separate).
- Iceberg REST facade.

## Shipped baseline (must keep)

| Piece | Location | Rule |
|---|---|---|
| Iceberg cold scan | `src/sqlrt/iceberg_table.rs` | Current snapshot when present (`LakeSnapshot::Empty` if none; WAL rows still return); schema/field IDs authoritative |
| Live ordinals | `src/buffer/wal_reader.rs` `WalCatalogReader::live_slices` | Catalog `SliceDirEntry` rows from `wal#idx#` (`CatalogPrefix::Index`) joined with `wal#seg#`, minus Iceberg named slices `(segment_id, start)` at `ReadBoundary::new` (Iceberg∪WAL local) or `ReadBoundary::for_clustered_ticket` (clustered). WAL-only scan uses `ReadBoundary::pin_wal`. Skip Index rows whose `SegmentRow` is missing. Slice-complete ordinals stay visible until Iceberg lists that slice. Query opens this via `open_query_wal_reader` → `open_wal_catalog_reader_for_config`: sled is `SledWalCatalog::open_read`; DynamoDB / Cloud Tables are constructed as `WalCatalogRead` with no commit fence. Same-process ingest wraps `PipelineWalHandle::reader()` so query does not reopen sled. |
| In-process merge | `IcebergWalTableProvider` | Append: `UnionExec`. CDC: `CdcReconcileExec` above Iceberg + WAL leaves. Local WAL leaf is `WalScanExec`. Clustered WAL leaf is `FlightSqlExec` to the picker winner's advertised `flight_addr` chosen at scan. Pin `(S, E, W)` at plan. |
| Client | `Session.query` → `clustered_query_collect` | Flight SQL to ready `flight_addr`s in `node_id` contact order; unreachable WAL fails the query; never takes an ingest lease |
| Catalog listing | `catalog_table_to_namespace` | Iceberg table name is identity; isolation is `table_namespace` |

`WalCatalogReader::live_slices` is the only live-slice selector. Do not fork a second selector.

### Best-effort WAL head

Gossip `GossipAd.wal_heads` is a bounded hint (`pipeline`, `catalog_seq_seen`, `payload_ready`, cap 32). Status RPC remains head truth when the picker confirms.

Picker for pipeline P:

1. Ready ads with a hint for P, plus local catalog seq if this process holds payloads.
2. Choose max `catalog_seq_seen` among `payload_ready` nodes. Tie → prefer local, else lowest `node_id`.
3. If the winner is unreachable, try the next-highest. Optional Status fill-in when no hints exist.
4. If nothing reachable → fail the query.

This is not a quorum and does not pin. Stale ads are allowed.

## WU-7.3 — Arrow Flight SQL 59.2

### Cutover

Hard-replace `src/query_socket` framed protocol with `src/query_flight`:

- Delete length-prefixed SQL request / Arrow IPC response.
- Keep `WalCatalogReader::live_slices` and DDL rejection semantics.
- Membership field remains `flight_addr` (now a Flight SQL gRPC bind).
- `QuerySocketServer` / `QuerySocketTableProvider` / `fetch_query_socket` names go away in the same change.
- HLA e2e `query_flight_ids` and framed handshake helpers switch to Flight SQL; do not keep a framed compatibility mode.

Replica/Flight/QueryRuntime `CURRENT_PROTOCOL` is 3. Flight SQL is not a catalog protocol bump; it is always mTLS.

### Server

Each clustered `sync` process starts one Flight SQL service on `0.0.0.0:0` with tonic mTLS, advertises it on the node membership row, and drains it on fence/SIGTERM after in-flight DoGet.

Implement Arrow Flight SQL 59.2 read-only commands:

| Command | Behavior |
|---|---|
| Handshake / GetFlightInfo / DoGet / metadata | **Require** `Authorization: Basic {tenant}/{workspace}`. Missing header is `Unauthenticated`. Scope is the session `TenantScope`, not `ClusterId`. |
| GetSqlInfo / GetCatalogs / GetSchemas / GetTables / GetTableTypes | Catalogs/schemas/tables for Iceberg pipelines **in the session tenant/workspace only**, listed in that pipeline's `table_namespace`. |
| CommandStatementQuery | User SELECT under that scope. Live WAL is `LiveWalTicket` DoGet, not a SQL TVF. |
| CommandStatementUpdate | Reject. |
| CreatePreparedStatement / ClosePreparedStatement | Opaque handle is the statement SQL. It does **not** encode a live ordinal snapshot. Tenant is re-checked at execute from request metadata. |

SQL size and Flight message size MUST sit under `CONTROL_FRAME_MAX_BYTES` (16 MiB) or fail closed.

Live WAL is **not** a customer table and **not** a SQL TVF. `IcebergWalTableProvider::scan` on the serving node: local pins `W` with `WalCatalogReader::live_slices` then Iceberg `S`/`E` and builds `WalScanExec`; clustered picks `flight_addr` then pins `W` with `skippr.live_wal_pin`, then Iceberg `S`/`E`, then builds `FlightSqlExec` with a `LiveWalTicket` of `W \ E`. User SQL cannot name that leaf.

Owner DoGet for `LiveWalTicket`:

1. resolve the pipeline `WalCatalogReader` for that `PipelineKey`;
2. exact `get(Index)` + `get(Segment)` per ticket `PinnedSliceRef` (`WalPinLost` on miss; MUST NOT `live_slices`);
3. stream `WalScanExec` batches projected/cast/null-filled to Iceberg schema (plus CDC sidecar when the ticket says so);
4. missing payload is `WalPinLost`; unknown pipeline fails the stream. Foreign session is a typed rejection.

### Host-side scan node

`IcebergWalTableProvider` remote WAL side MUST become a physical `FlightSqlExec` leaf (crate `skippr-query-plan`), not an eager collect into `StreamingBatchesExec`.

`FlightSqlExec` fields:

- `endpoint`: `host:port` of a ready replica `flight_addr` (gRPC `https://`, SAN `skippr-cluster`);
- `ticket`: the pinned `LiveWalTicket`;
- `schema`: Iceberg-aligned Arrow schema;
- `authorization`: the same `Basic {tenant}/{workspace}` header the planner's session used (executors MUST NOT widen scope).

`FlightSqlExec::execute` streams DoGet of that ticket. User SELECT stays Flight SQL `CommandStatementQuery`, not a `FlightSqlExec` SQL leaf. Collecting in `TableProvider::scan` is forbidden.

The merge WAL child is always that `FlightSqlExec`, including when the picker winner is this process (loopback to the advertised `flight_addr`, not `0.0.0.0` and not `WalTableProvider` in the merge). `WalScanExec` is only the owner's `LiveWalTicket` DoGet. That DoGet MUST NOT use QueryRuntime, so loopback cannot deadlock. Clustered SELECT uses `request_context()` (fresh catalog list; shared QueryRuntime planner/codec/UDFs).

### Tests (WU-7.3)

- Flight SQL metadata lists Iceberg tables in that pipeline's `table_namespace` only.
- SELECT against Iceberg∪WAL unique ids (existing e2e assertion, new transport).
- `LiveWalTicket` foreign tenant rejected; unknown pipeline rejected; DDL/update rejected.
- Ticket DoGet uses the plan-time pinned ordinals (a reclaim between pin and DoGet is `WalPinLost`).
- Empty `flight_addr` / no ready member → fail closed (`no ready Flight SQL node`), not a hung client.
- Oversized SQL rejected.
- `cargo test -p skipprd --lib query_flight::` replaces `query_socket::`.
- Harness framed-TCP helpers deleted; `query_each_replica_socket` talks Flight SQL GetFlightInfo/DoGet on every ready `flight_addr`.

## WU-7.5 — datafusion-distributed inside every clustered node

### Process model

datafusion-distributed **is** compiled into `skipprd` and **does** run inside `clustered sync`. There is no scheduler/executor split and no gossip election.

On `run_clustered`, bind the in-process QueryRuntime Worker first, then replica + Flight SQL + gossip. `query_context()` stays fail-closed until QueryRuntime has started (self Worker is enough with no peers).

| In-process service | Bind | Advertised / registered as |
|---|---|---|
| QueryRuntime Worker | `0.0.0.0:0` | gossip `worker` = advertised IP + port |
| Arrow Flight SQL 59.2 | `0.0.0.0:0` | membership / gossip `flight_addr` |
| Query frontend | same process | coordinating `SessionContext` with distributed planner + `WorkerResolver` |

`WorkerResolver::get_urls` returns self plus gossip ads that published `worker`. Replica `ready` is not a gate. There is no elected coordinator. No `BALLISTA_*` knobs. `query-scheduler#` is not written or read. Gossip `scheduler` / `executor` are not used.

### Iceberg Rust

`skipprd` links vendored Iceberg (`third_party/iceberg`). Serving node:

1. `Catalog::load_table` + current snapshot in-process (Dynamo pointer when clustered).
2. Scan parquet via Iceberg Rust `TableScan` (shared warehouse).
3. If load/scan setup fails, fail the query. Never `ListingTable` and never a peer Iceberg Flight.

### Discovery (`clustered query`)

1. Membership: ready `node#` rows.
2. Flight SQL to a ready `flight_addr` (try next on failure).
3. That node loads Iceberg in-process and runs the WAL picker. Client does not pick WAL.

Picker `None` fails the query (`no WAL head for pipeline`).

### Physical plan

```text
user SQL
  → Iceberg Rust scan (catalog JSON; parquet on any cluster executor)
  → FlightSqlExec (1 partition) to picker winner advertised flight_addr
  → UnionExec
  → Aggregate / Join / Sort / Limit
```

Distributed stages MUST place `UnionExec` (and its children) before downstream relational operators. Limit on the union stays `GlobalLimitExec` after concat.

WAL endpoint: `IcebergWalTableProvider::scan` (`pin_clustered_leaves`) calls `pick_wal_endpoint` and returns `Option<SocketAddr>` (max advertised `catalog_seq_seen` with `payload_ready`, local on tie **and this process holds WAL paths** → this process's advertised `flight_addr`). The UNION view does not freeze `flight_addr` at registration. Unreachable winner → next. None → fail the query. Do not vote row counts.

### `FlightSqlExec::execute`

On the executor:

1. Open a Flight SQL client to `endpoint`.
2. DoGet of a protobuf `Any` wrapping `LiveWalTicket` (type URL `type.googleapis.com/skippr.query.LiveWalTicket`).
3. Stream `SendableRecordBatchStream` with the encoded schema.
4. Map replica/transport errors to `DataFusionError::Execution`. Do not return empty success. Owner DoGet fail-closes when the Iceberg namespace schema cannot be loaded.

`FlightSqlExec::execute` streams DoGet; it MUST NOT return `NotImplemented` or collect the full result first.

### Codec

Replace ad-hoc `FLSQ` length-prefixed bytes with a prost message (compile-time fields, no `unwrap` on slices):

```text
message FlightSqlExecNode {
  string endpoint = 1;
  bytes arrow_schema_ipc = 3;
  string authorization = 4;
  LiveWalTicket ticket = 5;
}
```

`SkipprHostCodec` (in `skipprd`) implements `PhysicalExtensionCodec`:

- encode/decode `FlightSqlExec` via that message (`SkipprPhysicalCodec` in `skippr-query-plan` is the FlightSql helper);
- encode/decode `IcebergScanExec` via tagged prost;
- encode/decode `CdcReconcileExec` as a tagged node with Iceberg and WAL children;
- **fail closed** if asked to encode `WalScanExec` (WAL must not leave the owner as a distributed leaf);
- delegate every other node to `DefaultPhysicalExtensionCodec`;
- schema lives in `arrow_schema_ipc` because these leaves have no children.

datafusion-distributed ships **physical** plans only. `SkipprHostCodec` is the sole user codec on QueryRuntime. There is no logical table-provider codec on the worker wire.

### Tests (WU-7.5)

- Prost codec round-trip including schema; reject truncated/unknown tags.
- Distributed plan: merge children are Iceberg scan + `FlightSqlExec`; aggregate/join are parents of the merge. No `WalScanExec` in the distributed plan.
- Higher advertised peer head → `FlightSqlExec` to that `flight_addr`; local tie → `FlightSqlExec` to this process's advertised `flight_addr`.
- Host codec rejects `WalScanExec` encode. `WorkerResolver` returns self plus gossip `worker` addrs. Replica `ready` is not a gate.
- Worker bind is `0.0.0.0`; advertised worker is the cluster IP + bound port.
- Peer SIGSTOP/kill retries the next Flight SQL node; no reachable node fails closed. No Iceberg snapshot is `LakeSnapshot::Empty` (WAL rows still return).
- Iceberg catalog load failure fails the query. Never a peer Iceberg Flight.
- No `BALLISTA_*` / port env knobs.
- `cargo test -p skippr-query-plan` covers codec + execute against a test Flight SQL server.
- Process e2e: `distributed_cluster_query` after ingest — every ready `flight_addr` returns the same unique ids and `count(*)`; each live node logs `QueryRuntime started` with its worker addr. No extra query-engine OS processes. All WAL sockets stopped fails the query.

## Client SQL surface

Product SQL stays `sde query` / `skipprd query --sql`. Clustered mode:

- User writes `SELECT ... FROM <iceberg namespace>`.
- The session registers Iceberg∪WAL views per pipeline namespace (Iceberg table names; isolation is `table_namespace`).
- Live WAL is internal to the WAL leaf (`LiveWalTicket`). It is not a documented customer table.

`STREAM ... FROM ...` is a single-process WAL tail on `WalTableProvider` registered on the QueryRuntime coordinating context.

## Security and lifecycle

- Replica RPC and Flight SQL v1 assume a trusted routed network and restrictive security groups.
- Every `LiveWalTicket` / `skippr.live_wal_pin` verifies tenant/workspace.
- Query exposes only configured cluster pipelines.
- Fence of ingest does not stop Flight SQL or replica services.
- SIGTERM: drain in-flight DoGet, then stop Flight SQL and QueryRuntime worker, then replica/gossip, matching the architecture lifecycle list.

## Success criteria (these units only)

- No framed query socket remains in `skipprd`.
- Iceberg Rust, Flight SQL 59.2, and datafusion-distributed are linked into `skipprd`. No standalone query-engine OS processes.
- Every clustered node accepts user `SELECT` on `flight_addr` and returns the current table (Iceberg parquet plus uncompacted WAL, CDC applied).
- QueryRuntime executes the merge before downstream relational operators.
- Unreachable WAL fails the query. `LakeSnapshot::Empty` is only “no current Iceberg snapshot.”
- No new cluster configuration knobs.
- HLA harness query paths use Flight SQL against `skipprd` nodes only.

## Implementation status

| Item | Tree today |
|---|---|
| Framed query socket | Deleted (`src/query_flight` only) |
| `WalCatalogReader::live_slices` | Shipped in `src/buffer/wal_reader.rs` |
| In-process merge | Shipped (`IcebergWalTableProvider`). Iceberg pipelines register that provider. Clustered WAL child is `FlightSqlExec` to the picker winner chosen at scan. Local `skipprd query` (no Flight bind) uses `WalScanExec` inside `IcebergWalTableProvider`. `WalTableProvider` is WAL-only pipelines. `WalScanExec` must not cross the worker wire |
| `FlightSqlExec` | Prost codec + streaming DoGet execute |
| `SkipprHostCodec` | Tagged prost (`FlightSqlExec` / Iceberg) plus distributed shuffle; `WalScanExec` encode fails closed |
| Scheduler/executor bins | Deleted; one QueryRuntime Worker per process inside `run_clustered` |
| `query-scheduler#` | Deleted from the membership store; not a query-client gate |
| `clustered_query_collect` | Flight SQL client to ready `flight_addr`s; `Session.query` is the product entry |
