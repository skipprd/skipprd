# SkipprStore

Durable KV store for offsets, checkpoints, leases, membership, and SkipprLake catalog pointers. One table, non-colliding PK/SK prefixes.

## When to use

| Type | Use case |
|------|----------|
| **sled** (default) | Long-lived hosts with a persistent `DATA_DIR` (`WAL_STORAGE=disk`) |
| **dynamodb** | Ephemeral disks; resume across Lambda invocations (`WAL_STORAGE=s3`) |
| **dynamodb** (self-hosted clustered) | `WAL_STORAGE=clustered`: leases, membership, offsets/checkpoints, and catalog pointers share `skippr.store.name` |
| **cloud-tables** (Skippr Cloud) | Same clustered control-plane rows in Skippr Cloud Tables |

DynamoDB offset rows are a **materialized index**. In `s3` mode, if a row is missing after a committed WAL write, the next run rebuilds from S3 WAL via `wal_recover_s3`. In `clustered` mode, offsets are published only after WAL quorum and are fenced by `(wal_epoch, wal_commit_index, payload_sha256)`. Clustered mode never opens a hidden sled cache.

## Configuration

| Key | Values | Description |
|-----|--------|-------------|
| `skippr.store.type` / `SKIPPR_STORE_TYPE` / `--store-type` | `sled` (default), `dynamodb`, `cloud-tables` | SkipprStore backend. `WAL_STORAGE=clustered` uses DynamoDB when this is unset, or Cloud Tables when you set `cloud-tables`. |
| `skippr.store.name` / `SKIPPR_STORE_NAME` / `--store-name` | table name | Required for `dynamodb`, `cloud-tables`, and `WAL_STORAGE=clustered`. Same name whether the backend is DynamoDB or Cloud Tables. |
| `WAL_STORAGE` | `disk`, `s3`, `clustered` | `s3` for Lambda resume; `clustered` for multi-node disk WAL |
| `SKIPPR_WAL_S3_BUCKET` | bucket name | Dedicated WAL bucket (recommended for `s3`) |
| `SKIPPR_S3_BUCKET` | bucket name | Datalake / metadata bucket (unchanged) |

`skippr.yml` example:

```yaml
skippr:
  wal_s3_bucket: acme-skipprd-wal
  store:
    type: dynamodb
    name: console-skipprd-offsets-prod
```

CLI example:

```bash
export WAL_STORAGE=s3
export SKIPPR_WAL_S3_BUCKET=acme-skipprd-wal
export SKIPPR_STORE_TYPE=dynamodb
export SKIPPR_STORE_NAME=console-skipprd-offsets-prod
skipprd sync --once --pipeline google_analytics
```

Clustered example (DynamoDB):

```bash
export WAL_STORAGE=clustered
export SKIPPR_STORE_NAME=console-skipprd-offsets-prod
skipprd sync --pipeline google_analytics
```

Skippr Cloud Tables example (`WAL_STORAGE` is environment-only):

```yaml
skippr:
  store:
    type: cloud-tables
    name: console-skipprd-offsets-prod
```

```bash
export WAL_STORAGE=clustered
export SKIPPR_STORE_TYPE=cloud-tables
export SKIPPR_STORE_NAME=console-skipprd-offsets-prod
skipprd sync --pipeline google_analytics
```

## Table schema (`console-skipprd-offsets-{env}`)

Single-table design. Partition key for offsets is **derived** from tenant, workspace, and pipeline (no override env var).

| Attribute | Type | Description |
|-----------|------|-------------|
| `PK` | String | `{tenant}#{workspace}#{pipeline}` for offsets/leases, `cluster#{cluster_id}` for membership, or `catalog#{warehouse_hash}` for SkipprLake pointers |
| `SK` | String | `offset#{namespace}#{partition}`, `checkpoint#{logical_key}`, `lease`, `node#{uuid}`, `namespace#…`, or `table#…` |
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

### SkipprLake catalog items

SkipprLake `catalog_table` MAY be this same SkipprStore table. Catalog pointer PKs are `catalog#{warehouse_hash}`; SK prefixes are `namespace#` and `table#`.

## IAM (minimum)

On the SkipprStore table:

- `dynamodb:GetItem`
- `dynamodb:PutItem`
- `dynamodb:UpdateItem`
- `dynamodb:DeleteItem` (membership generation cleanup only)
- `dynamodb:Query` (same PK)
- `dynamodb:TransactWriteItems` (namespace/table count mutations when the table also holds catalog pointers)
- Conditional-write (`ConditionCheckFailed`) handling for lease CAS (`attribute_not_exists` / `owner_node`+`epoch`+`heartbeat` match), offset publish (`wal_epoch`+`wal_commit_index`+`payload_sha256`), membership deletes (heartbeat unchanged for 30s **and** dial failed), and catalog pointers (`generation`+`metadata_location`)

PK families:

- `{tenant}#{workspace}#{pipeline}` — offsets (`offset#…`), checkpoints (`checkpoint#…`), lease (`SK=lease`)
- `cluster#{cluster_id}` — membership (`node#{uuid}`, including `flight_addr` for Arrow Flight SQL 58.3). `clustered query` talks Flight SQL to a ready `flight_addr`; it does not use a `query-scheduler#` row. See [`../maintainers/hla-flight-sql-ballista.md`](../maintainers/hla-flight-sql-ballista.md).
- `catalog#{warehouse_hash}` — SkipprLake Iceberg pointers when `catalog_table` is this table

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
