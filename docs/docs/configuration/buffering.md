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
| **Values** | `disk`, `s3` |

- `disk` — WAL segments are stored in `DATA_DIR`. Fast, but requires the same disk on restart.
- `s3` — WAL segments are stored in `SKIPPR_WAL_S3_BUCKET` when set, otherwise `SKIPPR_S3_BUCKET`. No local disk dependency. Enables fully stateless compute. See [DynamoDB offset store](offset-store-dynamodb.md) for Lambda resume with `SKIPPR_OFFSET_STORE=dynamodb`.

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
