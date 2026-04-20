# How Skippr Works

Skippr is a host binary plus a published runtime plugin system. The host orchestrates discovery, sync, WAL recovery, compaction, and schema state, while runtime source, sink, and schema plugins are resolved from published manifests or explicit local overrides.

## Pipeline lifecycle

A pipeline moves through three phases:

### 1. Discover

```bash
skippr-el discover --pipeline my_pipeline --log
```

Connects to the configured data source, samples records, and infers the complete schema including nested fields. The schema is persisted as pipeline metadata in S3 (`SKIPPR_S3_BUCKET`).

Discovery detects:

- field names and nesting (including arrays of structs)
- data types (string, integer, long, double, boolean, timestamps)
- namespace separation when `TRANSFORM_NAMESPACE_FIELDS` is configured

### 2. Sync

```bash
skippr-el sync --pipeline my_pipeline --log
```

The main ingestion loop:

1. **Resolve plugins** — the host resolves the required runtime source, sink, and schema plugins from the published registry. Latest is the default; per-plugin version pins are optional.
2. **Connect runtime sessions** — plugins connect back to the host over a TCP control channel and a TCP data channel.
3. **Read** — runtime source plugins read external systems and send raw or prepared batches to the host.
4. **Durably buffer** — the host writes those batches to the WAL. Visible committed WAL state is the durable ingest boundary.
5. **Compact and write** — the host replays committed WAL work through sink and schema plugins using replay-safe compaction ids.
6. **Materialize resume state** — the host updates its offsets/checkpoint view from WAL-visible progress and provides that state back to sources on restart.

On shutdown or crash recovery, the host replays from committed WAL state.

### 3. Query

```bash
skippr-el query --sql "SELECT * FROM my_pipeline LIMIT 10"
```

Runs SQL against the destination tables via Athena. Also supports pipeline management commands (`ENABLE PIPELINE`, `DROP PIPELINE`, `RESET PIPELINE`, etc.) and live streaming from the WAL (`STREAM ... FROM ...`).

## Key components

### Write-Ahead Log (WAL)

Every ingested record is first written to the WAL before downstream compaction and destination writes. This guarantees that data survives process crashes, including SIGKILL.

- **Local disk WAL** (`WAL_STORAGE=disk`) — segments written under `DATA_DIR`
- **S3 WAL** (`WAL_STORAGE=s3`) — segments written to `SKIPPR_S3_BUCKET`

### Compactor

The compactor reads committed WAL work, groups data by output partition, produces Parquet, and drives replay-safe sink/schema operations. If compaction is replayed after a crash, the same logical `compaction_id` is reused.

### Offsets database

The offsets database is stored at `DATA_DIR`, opened only by the host process, and treated as a materialized view of committed WAL progress.

Runtime source plugins do not open the durable `sled` database directly. They ask the host to validate resume state and load checkpoints over the runtime protocol, while the host remains the only writer.

### Pipeline metadata

Stored in S3 at `{tenant}/{workspace}/{pipeline}/metadata.json`. Contains the discovered schema, field types, namespace definitions, and configuration. Updated on schema discovery and evolution.

### Runtime plugin registry

By default, runtime plugins are resolved from the latest published manifest index at `install.skippr.io`. Each plugin crate is versioned independently, and the host is not stamped with a shared plugin bundle version.

## Data flow diagram

```text
Published registry (`latest/manifest-index.json`)
  │
  ▼
Host (`skippr-el`)
  │
  ├── TCP control/data sessions
  │
  ├── Runtime Source Plugin ──▶ Host WAL writer
  │                               │
  │                               ▼
  │                          WAL Segments
  │                          (disk or S3)
  │                               │
  │                               ▼
  │                          Compaction replay
  │                               │
  │                          ┌────┴────┐
  │                          ▼         ▼
  ├── Runtime Sink Plugin ▶ Parquet   Destination writes
  │
  ├── Runtime Schema Plugin ─▶ Glue/catalog updates
  │
  └── Host-owned offsets/checkpoint view
```
