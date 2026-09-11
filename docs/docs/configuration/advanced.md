# Advanced Configuration

## State & storage

### SKIPPR_S3_BUCKET

The S3 bucket used by Skippr for all internal state: pipeline metadata, offsets, WAL segments (when `WAL_STORAGE=s3`), deadletters, config uploads, and query manifests.

| | |
|---|---|
| **Environment variable** | `SKIPPR_S3_BUCKET` |
| **Default** | *(required for production use)* |

S3 layout within this bucket:

```
{tenant}/{workspace}/{pipeline}/
  metadata.json           # Pipeline metadata and schema
  config/                 # Uploaded run config
  segments/               # WAL segments (when WAL_STORAGE=s3)
  manifest/               # Query manifest
deadletters/
  {tenant}/{workspace}/{pipeline}/...
```

### SKIPPRD_EL_STORAGE_MODE

Controls where skipprd extract/load metadata and namespace stats are persisted.

| | |
|---|---|
| **Environment variable** | `SKIPPRD_EL_STORAGE_MODE` |
| **YAML** | `skippr.skipprd_el_storage_mode` |
| **Default** | `s3` |
| **Values** | `s3`, `local` |

When set to `local`, metadata is read from and written to `{DATA_DIR}/metadata.json` (atomic write via temp + rename), and stats are stored under `{DATA_DIR}/stats/`.

When set to `s3` (default), S3-based persistence is used.

This internal development/testing setting only affects where skipprd EL state (metadata, stats) is persisted. It does not control `sde model` dbt project storage, React thread logs, or vector storage.

Use `local` when running skipprd without an S3 bucket (e.g. in skippr-dbt orchestration on a developer machine).

### DATA_DIR

Local directory for WAL segments (when `WAL_STORAGE=disk`) and the offsets database.

| | |
|---|---|
| **Environment variable** | `DATA_DIR` |
| **Default** | `./data` |

Created automatically if it doesn't exist. Supports absolute and relative paths.

## Operational

### SKIPPR_ENV

| | |
|---|---|
| **Environment variable** | `SKIPPR_ENV` |
| **Default** | `prod` |

Environment label. Used for logging and metadata context.

### SKIPPR_CHAOS_MODE

| | |
|---|---|
| **Environment variable** | `SKIPPR_CHAOS_MODE` |
| **Default** | `no` |
| **Values** | `yes` / `no` |

When enabled, the process exits with SIGKILL at a random point between 15 and 60 seconds into the run. Used to test exactly-once delivery guarantees under failure conditions.

### SCHEMA_AUTO_APPROVE

| | |
|---|---|
| **Environment variable** | `SCHEMA_AUTO_APPROVE` |
| **Default** | `true` |

Automatically approve schema changes during discovery and evolution. When `false`, schema changes require manual approval.

### RESET_OFFSETS

| | |
|---|---|
| **Environment variable** | `RESET_OFFSETS` |
| **Default** | `false` |

Reset the offsets database on startup. This causes the pipeline to re-ingest all data from the beginning.

### RESET_METADATA

| | |
|---|---|
| **Environment variable** | `RESET_METADATA` |
| **Default** | `false` |

Reset pipeline metadata on startup. The schema will be re-discovered from scratch.

### SYNC_FREQUENCY

| | |
|---|---|
| **Environment variable** | `SYNC_FREQUENCY` |
| **Default** | *(unset)* |

When set, sync runs repeatedly at this interval (in seconds) instead of running once and exiting.
