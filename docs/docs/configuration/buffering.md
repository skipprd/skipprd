# Buffering & WAL

## Buffer thresholds

Records are accumulated in memory before being flushed to the WAL. Flushing is triggered by whichever threshold is reached first.

### BUFFER_THRESHOLD_BYTES

| | |
|---|---|
| **Environment variable** | `BUFFER_THRESHOLD_BYTES` |
| **Default** | `10485760` (10 MB) |

Maximum buffer size in bytes before flush.

### BUFFER_THRESHOLD_SECONDS

| | |
|---|---|
| **Environment variable** | `BUFFER_THRESHOLD_SECONDS` |
| **Default** | `60` |

Maximum buffer age in seconds before flush.

## WAL configuration

The Write-Ahead Log provides durability and crash recovery. Every record passes through the WAL before it is compacted and uploaded to the destination.

### WAL_STORAGE

| | |
|---|---|
| **Environment variable** | `WAL_STORAGE` |
| **Default** | `disk` |
| **Values** | `disk`, `s3`, `clustered` |

- `disk` — WAL segments are stored in `DATA_DIR`. Fast, but requires the same disk on restart. Single-node ingest acquires a pipeline writer lease; there is no peer quorum.
- `s3` — WAL segments are stored in `SKIPPR_WAL_S3_BUCKET` when set, otherwise `SKIPPR_S3_BUCKET`. No local disk dependency. Enables fully stateless compute. Ingest acquires a pipeline writer lease; there is no peer quorum. See [DynamoDB offset store](offset-store-dynamodb.md) for Lambda resume with `SKIPPR_OFFSET_STORE=dynamodb`.
- `clustered` — local disk WAL plus synchronous peer replication (two durable copies), a clock-free DynamoDB lease, and DynamoDB offsets/checkpoints. Reuses `SKIPPR_OFFSET_DYNAMODB_TABLE`. Requires a release binary built with `--features offset-store-dynamodb`. `sync --once` and `discover` are rejected; `sync` runs the long-lived scheduler; `query` is query-only and never takes an ingest lease. Automatic failover with continued writes needs three live processes.

Unknown `WAL_STORAGE` values fail startup. There is no YAML `skippr.wal_storage` field.

### SKIPPR_WAL_S3_BUCKET

| | |
|---|---|
| **Environment variable** | `SKIPPR_WAL_S3_BUCKET` |
| **Default** | `SKIPPR_S3_BUCKET` |

Dedicated bucket for WAL segments only (recommended for Upfoundry). Keeps internal pipeline state separate from customer datalake objects.

### WAL_BYTES_PER_FILE

| | |
|---|---|
| **Environment variable** | `WAL_BYTES_PER_FILE` |
| **Default** | Auto-derived from `BUFFER_THRESHOLD_BYTES`, clamped to 4-64 MiB |

Optional target size override for each WAL segment file. By default the WAL target follows the pipeline buffer target so source batch, WAL, and compaction sizing stay aligned. Larger explicit overrides reduce S3 request counts but increase memory usage during compaction.

### WAL_MAX_DELAY_SECONDS

| | |
|---|---|
| **Environment variable** | `WAL_MAX_DELAY_SECONDS` |
| **Default** | `60` |

Coarse maximum age of a WAL segment before it is flushed, even if it hasn't reached the target size. Runtime ingest ACK latency is auto-tuned separately from observed throughput and persist latency, so this setting should rarely need adjustment.
