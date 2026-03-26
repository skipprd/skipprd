# How Skippr Works

Skippr is a single-binary data pipeline tool. It reads from a source, discovers schemas, buffers data through a write-ahead log (WAL), compacts it into Parquet, and uploads to S3 with Glue catalog integration.

## Pipeline lifecycle

A pipeline moves through three phases:

### 1. Discover

```bash
skippr-el discover --pipeline my_pipeline --log
```

Connects to the configured data source, samples records, and infers the complete schema including nested fields. The schema is persisted as pipeline metadata in S3 (`SKIPPR_S3_BUCKET`).

Discovery detects:

- Field names and nesting (including arrays of structs)
- Data types (string, integer, long, double, boolean, timestamps)
- Namespace separation when `TRANSFORM_NAMESPACE_FIELDS` is configured (one schema per event type)

### 2. Sync

```bash
skippr-el sync --pipeline my_pipeline --log
```

The main ingestion loop:

1. **Read** — the input plugin reads batches from the source (S3, local files)
2. **Buffer** — records are written to the WAL as segments. Segments can live on local disk (`WAL_STORAGE=disk`, the default) or S3 (`WAL_STORAGE=s3`) for fully durable remote storage
3. **Compact** — the compactor service reads WAL segments, converts them to Snappy-compressed Parquet, and uploads to the destination S3 bucket
4. **Register** — Glue table and Hive-style partitions are created or updated automatically
5. **Checkpoint** — offsets are committed to the offsets database after successful upload, ensuring exactly-once delivery

On shutdown (or crash recovery), the WAL is replayed to resume from the last committed offset.

### 3. Query

```bash
skippr-el query --sql "SELECT * FROM my_pipeline LIMIT 10"
```

Runs SQL against the destination tables via Athena. Also supports pipeline management commands (`ENABLE PIPELINE`, `DROP PIPELINE`, `RESET PIPELINE`, etc.) and live streaming from the WAL (`STREAM ... FROM ...`).

## Key components

### Write-Ahead Log (WAL)

Every ingested record is first written to the WAL before any processing. This guarantees that data survives process crashes, including SIGKILL.

- **Local disk WAL** (`WAL_STORAGE=disk`) — segments written to `DATA_DIR`. Fast, but requires the same disk on restart for recovery.
- **S3 WAL** (`WAL_STORAGE=s3`) — segments written to `SKIPPR_S3_BUCKET`. Fully durable, no local state required. Enables truly stateless compute.

### Compactor

A background actor service that continuously processes WAL segments:

1. Groups segments by partition (time bucket + namespace)
2. Reads and merges segment data
3. Writes Parquet via multipart upload to S3
4. Registers Glue partitions
5. Deletes consumed segments (local files or S3 objects)
6. Records tombstones to prevent reprocessing

On pipeline shutdown, the compactor drains all remaining segments before the process exits.

### Offsets database

Tracks which source records have been successfully processed. Stored on local disk at `DATA_DIR` and used during WAL recovery to skip already-committed data. This is the mechanism that provides exactly-once semantics.

### Pipeline metadata

Stored in S3 at `{tenant}/{workspace}/{pipeline}/metadata.json`. Contains the discovered schema, field types, namespace definitions, and configuration. Updated on schema discovery and evolution.

## Data flow diagram

```
Source (S3 / file)
  │
  ▼
Input Plugin ─── reads batches ───▶ Ingest Buffer
                                        │
                                        ▼
                                   WAL Segments
                                   (disk or S3)
                                        │
                                        ▼
                                   Compactor Service
                                        │
                                   ┌────┴────┐
                                   ▼         ▼
                              Parquet    Glue Table
                              on S3      + Partitions
                                   │
                                   ▼
                              Athena SQL
```
