# DynamoDB offset store

Optional durable offset and checkpoint index for short-lived runtimes (for example AWS Lambda). Use with **`WAL_STORAGE=s3`** so committed WAL segments remain the ownership proof.

## When to use

| Backend | Use case |
|---------|----------|
| **sled** (default) | Long-lived hosts with a persistent `DATA_DIR` |
| **dynamodb** | Ephemeral disks; resume across Lambda invocations |

DynamoDB rows are a **materialized index**. If a row is missing after a committed WAL write, the next run rebuilds from S3 WAL via `wal_recover_s3`.

## Configuration

Environment variables (also available as `skippr.yml` under `skippr:` and as CLI flags):

| Variable | Values | Description |
|----------|--------|-------------|
| `SKIPPR_OFFSET_STORE` | `sled` (default), `dynamodb` | Offset/checkpoint backend |
| `SKIPPR_OFFSET_DYNAMODB_TABLE` | table name | Required when store is `dynamodb` |
| `WAL_STORAGE` | `s3` | Required for Lambda resume |
| `SKIPPR_WAL_S3_BUCKET` | bucket name | Dedicated WAL bucket (recommended) |
| `SKIPPR_S3_BUCKET` | bucket name | Datalake / metadata bucket (unchanged) |

`skippr.yml` example:

```yaml
skippr:
  wal_s3_bucket: upfoundry-prod-skipprd-wal
  offset_store: dynamodb
  offset_dynamodb_table: console-skipprd-offsets-prod
```

CLI example:

```bash
export WAL_STORAGE=s3
export SKIPPR_WAL_S3_BUCKET=upfoundry-prod-skipprd-wal
export SKIPPR_OFFSET_STORE=dynamodb
export SKIPPR_OFFSET_DYNAMODB_TABLE=console-skipprd-offsets-prod
skipprd sync --once --pipeline google_analytics
```

## Table schema (`console-skipprd-offsets-{env}`)

Single-table design. Partition key is **derived** from tenant, workspace, and pipeline (no override env var).

| Attribute | Type | Description |
|-----------|------|-------------|
| `PK` | String | `{tenant}#{workspace}#{pipeline}` |
| `SK` | String | `offset#{namespace}#{partition}` or `checkpoint#{logical_key}` |
| `payload_b64` | String | Base64-encoded sled-compatible offset bytes or bincode checkpoint envelope |
| `updated_at` | String | ISO-8601 timestamp (debugging) |

### Offset items

- **SK:** `offset#{namespace}#{partition}`
- **Value:** Same 24-byte `OffsetValue` layout as local sled (filesize, line, closed).

### Checkpoint items

- **SK:** `checkpoint#{logical_key}` (for example GA `last_completed_date` key passed to the host checkpoint API)
- **Value:** Bincode-serialized `CheckpointEnvelope`.

## IAM (minimum)

On the offset table:

- `dynamodb:GetItem`
- `dynamodb:PutItem`
- `dynamodb:UpdateItem`
- `dynamodb:Query` (same PK)

On the WAL bucket (when `WAL_STORAGE=s3`):

- `s3:GetObject`, `s3:PutObject`, `s3:ListBucket` on `{tenant}/{workspace}/{pipeline}/segments/*`

## Recovery

1. On startup, `wal_recover_s3` scans committed `.seg` objects and materializes offset keys.
2. Plugins load checkpoint envelopes from DynamoDB when present.
3. If DynamoDB write fails after WAL commit, the next run repairs from WAL; no duplicate ownership for committed events.

## Non-goals

- DynamoDB is not the sole exactly-once proof (WAL is).
- No `SKIPPR_OFFSET_NAMESPACE` override; identity is always derived.
- No separate lease item in v1 (Upfoundry uses pipeline run status + continuation events).
