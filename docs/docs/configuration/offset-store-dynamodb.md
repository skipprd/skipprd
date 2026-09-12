# DynamoDB offset store

Optional durable offset and checkpoint index for short-lived runtimes (for example AWS Lambda), and the required offset/lease table for `WAL_STORAGE=clustered`.

## When to use

| Backend | Use case |
|---------|----------|
| **sled** (default) | Long-lived hosts with a persistent `DATA_DIR` (`WAL_STORAGE=disk`) |
| **dynamodb** | Ephemeral disks; resume across Lambda invocations (`WAL_STORAGE=s3`) |
| **dynamodb** (required) | `WAL_STORAGE=clustered`: leases, membership, and fenced offsets/checkpoints share `SKIPPR_OFFSET_DYNAMODB_TABLE` |

DynamoDB offset rows are a **materialized index**. In `s3` mode, if a row is missing after a committed WAL write, the next run rebuilds from S3 WAL via `wal_recover_s3`. In `clustered` mode, offsets are published only after WAL quorum and are fenced by `(wal_epoch, wal_commit_index, payload_sha256)`. Clustered mode never opens a hidden sled cache.

## Build

Published `skipprd` release binaries are built with `--features offset-store-dynamodb`. Local
`cargo build -p skipprd` without that feature rejects `SKIPPR_OFFSET_STORE=dynamodb` and
`WAL_STORAGE=clustered` at runtime
(host/plugin dependency boundary: `aws-sdk-dynamodb` must not link into default host builds).

## Configuration

Environment variables (also available as `skippr.yml` under `skippr:` and as CLI flags):

| Variable | Values | Description |
|----------|--------|-------------|
| `SKIPPR_OFFSET_STORE` | `sled` (default), `dynamodb` | Offset/checkpoint backend. `clustered` selects DynamoDB when this is unset; an explicit non-DynamoDB value fails startup. |
| `SKIPPR_OFFSET_DYNAMODB_TABLE` | table name | Required when store is `dynamodb` or `WAL_STORAGE=clustered` |
| `WAL_STORAGE` | `disk`, `s3`, `clustered` | `s3` for Lambda resume; `clustered` for multi-node disk WAL |
| `SKIPPR_WAL_S3_BUCKET` | bucket name | Dedicated WAL bucket (recommended for `s3`) |
| `SKIPPR_S3_BUCKET` | bucket name | Datalake / metadata bucket (unchanged) |

`skippr.yml` example:

```yaml
skippr:
  wal_s3_bucket: acme-skipprd-wal
  offset_store: dynamodb
  offset_dynamodb_table: console-skipprd-offsets-prod
```

CLI example:

```bash
export WAL_STORAGE=s3
export SKIPPR_WAL_S3_BUCKET=acme-skipprd-wal
export SKIPPR_OFFSET_STORE=dynamodb
export SKIPPR_OFFSET_DYNAMODB_TABLE=console-skipprd-offsets-prod
skipprd sync --once --pipeline google_analytics
```

Clustered example:

```bash
export WAL_STORAGE=clustered
export SKIPPR_OFFSET_DYNAMODB_TABLE=console-skipprd-offsets-prod
skipprd sync --pipeline google_analytics
```

## Table schema (`console-skipprd-offsets-{env}`)

Single-table design. Partition key is **derived** from tenant, workspace, and pipeline (no override env var).

| Attribute | Type | Description |
|-----------|------|-------------|
| `PK` | String | `{tenant}#{workspace}#{pipeline}` for offsets/leases, or `cluster#{cluster_id}` for membership |
| `SK` | String | `offset#{namespace}#{partition}`, `checkpoint#{logical_key}`, `lease`, or `node#{uuid}` |
| `payload_b64` | String | Base64-encoded sled-compatible offset bytes or bincode checkpoint envelope |
| `wal_epoch` | Number | Clustered WAL lease epoch (offsets/checkpoints) |
| `wal_commit_index` | Number | Clustered commit index |
| `payload_sha256` | String | Hash of the published payload |
| `owner_node` / `epoch` / `heartbeat` / `released` / `initialized` | mixed | Lease item (`SK=lease`). Safety does not use `lease_until < now`. |
| `updated_at` | String | ISO-8601 timestamp (debugging only) |

### Offset items

- **SK:** `offset#{namespace}#{partition}`
- **Value:** Same 24-byte `OffsetValue` layout as local sled (filesize, line, closed).
- Clustered writes merge Closed with OR and Position/Filesize with max, then conditionally write against the prior `(wal_epoch, wal_commit_index, payload_sha256)` tuple.

### Checkpoint items

- **SK:** `checkpoint#{logical_key}` (for example GA `last_completed_date` key passed to the host checkpoint API)
- **Value:** Bincode-serialized `CheckpointEnvelope`.

### Lease items

- **SK:** `lease`
- Clock-free steal: observe `(owner_node, epoch, heartbeat)` unchanged for 30 seconds of local monotonic time, then conditional update. Renew increments `heartbeat`. Rows are never deleted.

### Membership items

- **PK:** `cluster#{cluster_id}` (`SKIPPR_CLUSTER_ID`)
- **SK:** `node#{uuid}`
- Cold discovery uses `Query` on that PK; it never scans the table.

## IAM (minimum)

On the offset table:

- `dynamodb:GetItem`
- `dynamodb:PutItem`
- `dynamodb:UpdateItem`
- `dynamodb:DeleteItem` (membership generation cleanup only)
- `dynamodb:Query` (same PK)
- Conditional-write (`ConditionCheckFailed`) handling for lease CAS (`attribute_not_exists` / `owner_node`+`epoch`+`heartbeat` match), offset publish (`wal_epoch`+`wal_commit_index`+`payload_sha256`), and membership deletes (heartbeat unchanged for 30s **and** dial failed).

On the Iceberg catalog table (`catalog.table` when `type: skippr`):

- `dynamodb:GetItem`, `dynamodb:PutItem`, `dynamodb:UpdateItem`, `dynamodb:Query`
- `dynamodb:TransactWriteItems` (namespace/table count mutations)
- Conditional-write handling for catalog pointers (`generation`+`metadata_location`)

PK families on the offset/lease table:

- `{tenant}#{workspace}#{pipeline}` — offsets (`offset#…`), checkpoints (`checkpoint#…`), lease (`SK=lease`)
- `cluster#{cluster_id}` — membership (`node#{uuid}`, including `flight_addr` for Arrow Flight SQL 58.3). `clustered query` talks Flight SQL to a ready `flight_addr`; it does not use a `query-scheduler#` row. See [`../maintainers/hla-flight-sql-ballista.md`](../maintainers/hla-flight-sql-ballista.md).

Iceberg `catalog.type: skippr` uses a **separate** customer-created table (`catalog.table` on the Iceberg sink). Catalog pointer PKs are `catalog#{warehouse_hash}` on that table. `catalog.table` MUST NOT be `SKIPPR_OFFSET_DYNAMODB_TABLE`.

On the WAL bucket (when `WAL_STORAGE=s3`):

- `s3:GetObject`, `s3:PutObject`, `s3:ListBucket` on `{tenant}/{workspace}/{pipeline}/segments/*`

## Recovery

1. On startup, `wal_recover_s3` scans committed `.seg` objects and materializes offset keys (`s3` mode).
2. Clustered promotion reconciles DynamoDB offsets through the hash-consistent committed head before source listing.
3. Plugins load checkpoint envelopes from DynamoDB when present.
4. If DynamoDB write fails after WAL commit in `s3` mode, the next run repairs from WAL; no duplicate ownership for committed events.
5. If DynamoDB write fails after clustered quorum, the pipeline pauses and does not Ack ingest; promotion retries publication.

## Non-goals

- DynamoDB is not the sole exactly-once proof (WAL quorum is, in clustered mode).
- No `SKIPPR_OFFSET_NAMESPACE` override; identity is always derived.
- No sled lease backend. Production leases are DynamoDB only; tests use `MemoryLeaseStore`.
- No separate lease table or extra cluster knobs in v1.
