# HLA: Arrow Flight SQL 58.3 and Ballista 53

**Status:** implemented in `skipprd` (Flight SQL 58.3 + in-process Ballista 53; Iceberg Rust). HLA e2e over Flight SQL is the remaining live gate.  
**Architecture index:** [`hla-distributed-query-iceberg-catalog.md`](./hla-distributed-query-iceberg-catalog.md)  
**Work units:** WU-7.3 and WU-7.5 in [`hla-implementation-wbs.md`](./hla-implementation-wbs.md)  
**Style:** [`AGENTS.md`](../../../AGENTS.md)

This file is the requirements and design contract for replacing the framed-TCP query socket with Arrow Flight SQL 58.3 and executing Iceberg∪WAL UNION on Ballista 53 **inside every clustered `skipprd` process**. Iceberg Rust (vendored `third_party/iceberg`) is compiled into `skipprd`. Replica RPC `CURRENT_PROTOCOL` is **2** (cluster hello, not tenant hello).

Implementation steps: [`.cursor/plans/embedded_ballista_flight_sql_8f1a2c3d.plan.md`](../../../.cursor/plans/embedded_ballista_flight_sql_8f1a2c3d.plan.md).

Shipped: every clustered `skipprd` process serves Arrow Flight SQL 58.3 on membership `flight_addr`, joins one gossip-elected Ballista 53 cluster (scheduler + executor on every node), and plans Iceberg∪WAL UNION in-process. Iceberg Rust (vendored `third_party/iceberg`) is compiled into `skipprd`. Standalone `skippr-ballista-*` binaries are deleted. Dual query protocols are forbidden. `query-scheduler#` is not used.

## Requirements

### Functional

1. Every clustered node MUST serve Arrow Flight SQL 58.3 on the advertised `flight_addr` (wildcard bind, ephemeral port, no new address knobs).
2. Live WAL remains unpinned and eventually consistent. `select_live_ordinals` is the only selector. DoGet MUST re-select ordinals at execution time. Tickets and prepared handles MUST NOT freeze a segment/ordinal set.
3. The only Skippr SQL extension for live WAL is:

   ```sql
   live_wal_scan(tenant, workspace, pipeline, namespace, exclude_segment_ids)
   ```

   There is no public custom Flight ticket type, no `wal_scan` RPC, and no client-supplied head cut. `exclude_segment_ids` is the Iceberg snapshot property `skippr.wal-segment-ids` (semicolon- or comma-joined). The replica ignores unknown ids.
4. Flight SQL is read-only. INSERT, UPDATE, DELETE, CREATE, DROP, ALTER, MERGE, and CommandStatementUpdate MUST fail closed.
5. `live_wal_scan` tenant/workspace MUST equal the Flight session `TenantScope` (`Authorization: Basic {tenant}/{workspace}` on **every** RPC). Foreign identity is a typed rejection, not an empty scan. Unknown pipeline is a typed rejection. Cluster membership is **not** a tenant.
6. Iceberg catalog listing for UNION registration MUST keep only tables whose names match that pipeline sink's `table_prefix` (see `catalog_table_to_namespace`). A pipeline with no catalog tables yet MUST NOT fail registration of another pipeline in the same session.
7. UNION is a dumb concat of Iceberg snapshot + live WAL. It MUST NOT DISTINCT, wait, or pin. Missing WAL files skip that ordinal. Unreachable WAL is Iceberg-only (complete Iceberg result, not a partial UNION).
8. Ballista 53 MUST execute that UNION **before** aggregate, join, sort, or limit that is not pushed into a scan. Iceberg parquet MAY run on any executor in the elected cluster (shared warehouse; `IcebergScanExec` reloads from `catalog_json`). Live WAL is always a one-partition `FlightSqlExec` DoGet to the advertised `flight_addr` of the picker winner (best-effort max `committed_index`, including when the winner is this process). `WalTableProvider` / `WalScanExec` MUST NOT enter a Ballista plan (`paths_root` is owner-local). `WalScanExec` runs only inside the owner's `live_wal_scan` DoGet.
9. Every clustered `skipprd` node is a query node: in-process Ballista scheduler+executor bind first, then Flight SQL on `flight_addr`, then gossip. Executors and `query_context()` connect to the gossip-elected scheduler (minimum `NodeId` among ads that published `scheduler`, including self; replica `ready` is not a gate). Clustered SELECT fails closed if Ballista is not connected. `clustered query` opens Flight SQL on any ready `flight_addr`. The serving node plans UNION. `SK=query-scheduler#{generation}` is **not** used.
10. Iceberg metadata is loaded with in-process Iceberg Rust `Catalog` on the serving node. If load/scan setup fails, retry Iceberg scan via another ready node's Flight SQL. Never fall back to parquet listing.
11. Partial distributed results are forbidden. Executor or replica failure either retries a typed alternative (next WAL head, peer Iceberg Flight, or Iceberg-only) or fails the query. Returning a subset of UNION partitions is not allowed.

### Non-functional

- Versions are the workspace pins: Arrow **58.3** (`arrow-flight` with `flight-sql`), DataFusion **53.1**, Ballista **53** matching that DataFusion, Iceberg Rust via `third_party/iceberg`. Do not take a published Iceberg-DataFusion crate that breaks those pins.
- No query/lease/quorum/TTL/bind/peer-list knobs. Flight SQL, Ballista executor, replica, and gossip all bind ephemeral ports.
- Default host `skipprd` builds still MUST NOT pull `aws-sdk-dynamodb`. `arrow-flight`, `ballista`, and `iceberg` MAY be default-host dependencies. Dynamo catalog stays `offset-store-dynamodb`.
- Clustered replica TCP, Flight SQL, and Ballista gRPC MUST use mTLS (SAN `skippr-cluster`). Gossip UDP uses HMAC with required `SKIPPR_CLUSTER_GOSSIP_KEY`.
- Every Flight SQL RPC MUST carry `Authorization: Basic {tenant}/{workspace}`. Cloud JWT is verified only at the Cloud gateway (D31); skipprd enforces the resulting `TenantScope`.
- Query must not nack `ReclaimSegment` or hold pins. In-flight DoGet that loses a file skips that ordinal.
- Metrics already named `cluster_flight_request` / `cluster_flight_fail` apply to Flight SQL (keep the names).

### Non-goals

- Custom public WAL Flight tickets.
- Pinning WAL to a query snapshot / delaying reclaim.
- Guaranteeing the globally latest WAL (advertised max is best effort).
- Historical Iceberg time-travel UNION with reclaimed WAL.
- Standalone `skippr-ballista-scheduler` / `skippr-ballista-executor` processes.
- Configurable executor count, batch size, or scheduler port.
- Chitchat (WU-5.1 remains separate).
- Iceberg REST facade.

## Shipped baseline (must keep)

| Piece | Location | Rule |
|---|---|---|
| Iceberg cold scan | `src/sqlrt/iceberg_table.rs` | Current snapshot, fail closed if none; schema/field IDs authoritative |
| Live ordinals | `src/query_flight/live_wal.rs` `select_live_ordinals` | Snapshot segments ∪ suffix `CommitSegment`, minus reclaim, minus Iceberg named ids; skip unreadable ordinals. Ledger-complete ordinals stay visible until Iceberg lists the segment. |
| In-process UNION | `IcebergWalUnionProvider` | `UnionExec` + `GlobalLimitExec` on the union; WAL is always `FlightSqlExec` to the picker winner's advertised `flight_addr` |
| Client | `Session.query` → `clustered_query_collect` | Flight SQL to ready `flight_addr`s in `node_id` contact order; Iceberg-only is inside the serving node; never takes an ingest lease |
| Catalog prefix | `catalog_table_to_namespace` | Shared Dynamo namespace, per-sink prefix |

Move `select_live_ordinals` with the Flight SQL module. Do not fork a second selector.

### Best-effort WAL head

Gossip `GossipAd.wal_heads` is a bounded hint (`pipeline`, `committed_index`, cap 32). Status RPC remains head truth when the picker confirms.

Picker for pipeline P:

1. Ready ads with a hint for P, plus local committed if this process holds a log.
2. Choose max `committed_index`. Tie → prefer local, else lowest `node_id`.
3. If the winner is unreachable, try the next-highest. Optional Status fill-in when no hints exist.
4. If nothing reachable → omit WAL (Iceberg-only).

This is not a quorum and does not pin. Stale ads are allowed.

## WU-7.3 — Arrow Flight SQL 58.3

### Cutover

Hard-replace `src/query_socket` framed protocol with `src/query_flight`:

- Delete length-prefixed SQL request / Arrow IPC response.
- Keep `select_live_ordinals` and DDL rejection semantics.
- Membership field remains `flight_addr` (now a Flight SQL gRPC bind).
- `QuerySocketServer` / `QuerySocketTableProvider` / `fetch_query_socket` names go away in the same change.
- HLA e2e `query_flight_ids` and framed handshake helpers switch to Flight SQL; do not keep a framed compatibility mode.

Replica/Flight/Ballista `CURRENT_PROTOCOL` is 2. Flight SQL is not a mutation-log protocol bump; it is always mTLS.

### Server

Each clustered `sync` process starts one Flight SQL service on `0.0.0.0:0` with tonic mTLS, advertises it on the node membership row, and drains it on fence/SIGTERM after in-flight DoGet.

Implement Arrow Flight SQL 58.3 read-only commands:

| Command | Behavior |
|---|---|
| Handshake / GetFlightInfo / DoGet / metadata | **Require** `Authorization: Basic {tenant}/{workspace}`. Missing header is `Unauthenticated`. Scope is the session `TenantScope`, not `ClusterId`. |
| GetSqlInfo / GetCatalogs / GetSchemas / GetTables / GetTableTypes | Catalogs/schemas/tables for Iceberg pipelines **in the session tenant/workspace only**, prefix-filtered. |
| CommandStatementQuery | SELECT (and `live_wal_scan` TVF) under that scope. |
| CommandStatementUpdate | Reject. |
| CreatePreparedStatement / ClosePreparedStatement | Opaque handle is the statement SQL. It does **not** encode a live ordinal snapshot. Tenant is re-checked at execute from request metadata. |

SQL size and Flight message size MUST sit under `CONTROL_FRAME_MAX_BYTES` (16 MiB) or fail closed.

`live_wal_scan` is an **internal** statement form, not a customer table. Flight SQL classifies a statement as internal only when the sole `FROM` item is `live_wal_scan(...)` or `iceberg_scan(...)` (after comments and string literals). A user `SELECT` that mentions those names in a string is not intercepted. Argument forms:

- five args: tenant, workspace, pipeline, namespace, exclude ids (tenant/workspace must match session `TenantScope`);
- two/three args: pipeline, namespace, optional exclude ids (session supplies tenant/workspace).

Foreign session is a typed rejection. The serving node does not register a DataFusion TVF; the internal form executes as a local `WalTableProvider` / Iceberg scan so user SQL cannot hijack it.

DoGet for `live_wal_scan`:

1. resolve `PipelineDurableStore` or replica-registry session paths for that `PipelineKey`;
2. open `MutationLog`;
3. `select_live_ordinals(log, exclude_segment_ids)`;
4. stream `WalTableProvider` batches projected/cast/null-filled to Iceberg schema;
5. skip missing files and unreadable ledgers; unknown pipeline fails the stream.

### Host-side scan node

`IcebergWalUnionProvider` remote WAL side MUST become a physical `FlightSqlExec` leaf (crate `skippr-query-ballista`), not an eager collect into `StreamingBatchesExec`.

`FlightSqlExec` fields:

- `endpoint`: `host:port` of a ready replica `flight_addr` (gRPC `https://`, SAN `skippr-cluster`);
- `prepared_handle`: statement SQL bytes for `live_wal_scan(...)` (no ordinal snapshot);
- `schema`: Iceberg-aligned Arrow schema;
- `authorization`: the same `Basic {tenant}/{workspace}` header the planner's session used (executors MUST NOT widen scope).

`FlightSqlExec::execute` streams GetFlightInfo+DoGet of that SQL. Collecting in `TableProvider::scan` is forbidden.

The UNION WAL child is always that `FlightSqlExec`, including when the picker winner is this process (loopback to the advertised `flight_addr`, not `0.0.0.0` and not `WalTableProvider` in the UNION). `WalTableProvider` is only the owner's `live_wal_scan` DoGet. That DoGet MUST NOT use Ballista (`classify_sql` sole-FROM), so loopback cannot deadlock.

### Tests (WU-7.3)

- Flight SQL metadata lists prefix-matching Iceberg tables only.
- SELECT against Iceberg∪WAL unique ids (existing e2e assertion, new transport).
- `live_wal_scan` foreign tenant rejected; unknown pipeline rejected; DDL/update rejected.
- Prepared handle DoGet re-selects ordinals (a reclaim between GetFlightInfo and DoGet skips the file, does not fail closed unless every path fails).
- Empty `flight_addr` / no ready member → Iceberg-only, not a hung client.
- Oversized SQL rejected.
- `cargo test -p skipprd --lib query_flight::` replaces `query_socket::`.
- Harness framed-TCP helpers deleted; `query_each_replica_socket` talks Flight SQL GetFlightInfo/DoGet on every ready `flight_addr`.

## WU-7.5 — Ballista 53 inside every clustered node

### Process model

Ballista **is** compiled into `skipprd` and **does** run inside `clustered sync`. Delete `skippr-ballista-scheduler` and `skippr-ballista-executor` binaries.

On `run_clustered`, bind in-process Ballista scheduler+executor first, then replica + Flight SQL + gossip. `query_context()` stays fail-closed until the process connects to the elected scheduler (self is enough with no peers).

| In-process service | Bind | Advertised / registered as |
|---|---|---|
| Ballista 53 scheduler | `0.0.0.0:0` | gossip `scheduler` = advertised IP + port |
| Ballista 53 executor | `0.0.0.0:0` | `ExecutorRegistration.host` = advertised IP (not `127.0.0.1`) |
| Arrow Flight SQL 58.3 | `0.0.0.0:0` | membership / gossip `flight_addr` |
| Query frontend | same process | `SessionContext::remote` to the elected scheduler |

Election: minimum `NodeId` (UUID string order) among ads with `scheduler: Some(_)`, including self. Replica `ready` is not a gate; a spare MAY run the scheduler. A late node with a lower `NodeId` steals the scheduler. Unreachable elected addrs are skipped so a dead min-UUID cannot pin the cluster. Reconnect poll_loop + session without draining the local scheduler process; a serving node also reconnects before executing User SQL if the elected scheduler is dead. No sticky winner. No `BALLISTA_*` knobs. `query-scheduler#` is not written or read. Gossip `executor` is not used.

### Iceberg Rust

`skipprd` links vendored Iceberg (`third_party/iceberg`). Serving node:

1. `Catalog::load_table` + current snapshot in-process (Dynamo pointer when clustered).
2. Scan parquet via Iceberg Rust `TableScan` (shared warehouse).
3. If load/scan setup fails: send Iceberg scan to another ready `flight_addr`. If none succeed, fail closed. Never `ListingTable`.

### Discovery (`clustered query`)

1. Membership: ready `node#` rows.
2. Flight SQL to a ready `flight_addr` (try next on failure).
3. That node runs Iceberg local (+ peer fallback) and the WAL picker. Client does not pick WAL.

Iceberg-only is an outcome **inside** the serving node when the picker returns `None`.

### Physical plan

```text
user SQL
  → Iceberg Rust scan (catalog JSON; parquet on any cluster executor)
  → FlightSqlExec (1 partition) to picker winner advertised flight_addr
  → UnionExec
  → Aggregate / Join / Sort / Limit
```

Ballista stages MUST place `UnionExec` (and its children) before downstream relational operators. Limit on the union stays `GlobalLimitExec` after concat.

WAL endpoint: `pick_wal_endpoint` returns `Option<SocketAddr>` (max advertised `committed_index`, local on tie **and this process holds WAL paths** → this process's advertised `flight_addr`). Unreachable winner → next. None → Iceberg-only. Do not vote row counts.

### `FlightSqlExec::execute`

On the executor:

1. Open a Flight SQL client to `endpoint`.
2. Execute the statement SQL in `prepared_handle` (GetFlightInfo + streaming DoGet).
3. Stream `SendableRecordBatchStream` with the encoded schema.
4. Map replica/transport errors to `DataFusionError::Execution`. Do not return empty success.

`FlightSqlExec::execute` streams DoGet; it MUST NOT return `NotImplemented` or collect the full result first.

### Codec

Replace ad-hoc `FLSQ` length-prefixed bytes with a prost message (compile-time fields, no `unwrap` on slices):

```text
message FlightSqlExecNode {
  string endpoint = 1;
  bytes prepared_handle = 2;
  bytes arrow_schema_ipc = 3;
}
```

`SkipprHostCodec` (in `skipprd`) implements `PhysicalExtensionCodec`:

- encode/decode `FlightSqlExec` via that message (`SkipprPhysicalCodec` in `skippr-query-ballista` is the FlightSql helper);
- encode/decode `IcebergScanExec` via tagged prost;
- **fail closed** if asked to encode `WalScanExec` (WAL must not leave the owner as a Ballista leaf);
- delegate every other node to Ballista `BallistaPhysicalExtensionCodec`;
- schema lives in `arrow_schema_ipc` because these leaves have no children.

`SkipprLogicalCodec` implements `LogicalExtensionCodec` (wrapping `BallistaLogicalExtensionCodec`) so `SessionContext::remote` can serialize registered tables:

- encode/decode `FlightSqlTableProvider`, `IcebergScanTableProvider`, and `IcebergWalUnionProvider`;
- **fail closed** if asked to encode `WalTableProvider` (clustered WAL is always Flight SQL to the picker winner);
- Iceberg decode reconstructs a provider that reloads from `catalog_json` on scan (same as `IcebergScanExec::from_node`).

### Tests (WU-7.5)

- Prost codec round-trip including schema; reject truncated/unknown tags.
- Distributed plan: UNION children are Iceberg scan + `FlightSqlExec`; aggregate/join are parents of UNION. No `WalScanExec` in the Ballista plan.
- Higher advertised peer head → `FlightSqlExec` to that `flight_addr`; local tie → `FlightSqlExec` to this process's advertised `flight_addr`.
- Host codec rejects `WalScanExec` encode. Election: min `NodeId` among `scheduler` ads; `ready: false` still eligible. Unreachable elected scheduler is skipped so the next live min UUID (including self) takes over without draining the local scheduler.
- Executor registration host is the advertised IP; scheduler bind is `0.0.0.0`.
- Peer SIGSTOP/kill retries next head or Iceberg-only; no Iceberg snapshot fails closed.
- Iceberg catalog load failure uses peer Flight Iceberg scan.
- No `BALLISTA_*` / port env knobs.
- `cargo test -p skippr-query-ballista` covers codec + execute against a test Flight SQL server.
- Process e2e: `ballista_cluster_query` after ingest — every ready `flight_addr` returns the same unique ids and `count(*)`; all live nodes log the same `elected_scheduler=`. No extra Ballista OS processes. Iceberg-only when WAL sockets are stopped.

## Client SQL surface

Product SQL stays `sde query` / `skipprd query --sql`. Clustered mode:

- User writes `SELECT ... FROM <iceberg namespace>`.
- The session registers UNION views per pipeline namespace (prefix-stripped catalog names).
- `live_wal_scan` is internal to the WAL leaf (sole-FROM statement). It is not a documented customer table.

`STREAM ... FROM ...` (single-process WAL tail) is unchanged and is not Ballista.

## Security and lifecycle

- Replica RPC and Flight SQL v1 assume a trusted routed network and restrictive security groups.
- Every `live_wal_scan` verifies tenant/workspace.
- Query exposes only configured cluster pipelines.
- Fence of ingest does not stop Flight SQL or replica services.
- SIGTERM: drain in-flight DoGet, then stop Flight SQL and Ballista executor, then replica/gossip, matching the architecture lifecycle list.

## Success criteria (these units only)

- No framed query socket remains in `skipprd`.
- Iceberg Rust, Flight SQL 58.3, and Ballista 53 are linked into `skipprd`. No standalone Ballista OS processes.
- Every clustered node accepts user `SELECT` on `flight_addr` and runs UNION of Iceberg parquet + best-effort latest advertised WAL.
- Ballista executes UNION before downstream relational operators.
- Iceberg-only remains a complete stand-in when no WAL head is reachable.
- No new cluster configuration knobs.
- HLA harness query paths use Flight SQL against `skipprd` nodes only.

## Implementation status

| Item | Tree today |
|---|---|
| Framed query socket | Deleted (`src/query_flight` only) |
| `select_live_ordinals` | Shipped in `src/query_flight/live_wal.rs` |
| In-process `UnionExec` | Shipped; WAL is always `FlightSqlExec` to the picker winner |
| `FlightSqlExec` | Prost codec + streaming DoGet execute |
| `SkipprHostCodec` | Tagged prost (`FlightSqlExec` / Iceberg) plus Ballista shuffle; `WalScanExec` encode fails closed |
| `SkipprLogicalCodec` | Tagged prost for UNION / Flight SQL / Iceberg table providers; `WalTableProvider` encode fails closed |
| Scheduler/executor bins | Deleted; elected in-process Ballista cluster inside `run_clustered` |
| `query-scheduler#` | Deleted from the membership store; not a query-client gate |
| `clustered_query_collect` | Flight SQL client to ready `flight_addr`s; `Session.query` is the product entry |
