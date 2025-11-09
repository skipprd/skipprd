S3-backed WAL Segments
Config
Add WAL_STORAGE pipeline knob: local (default) or s3.
Add wal_segments_prefix to the pipeline manifest (e.g., s3://bucket/segments/<tenant>/<workspace>/<pipeline>/), persisted alongside existing entries.
Write path (S3 WAL)
For each segment:
MPU upload <id>.seg to wal_segments_prefix/p_year=YYYY/p_month=MM/p_day=DD/<id>.seg.
Compute sha256, parts_count, size (as today).
PUT <id>.seg.commit (60-byte binary header) next to the segment.
Offsets commit only after .seg.commit PUT succeeds.
Read/compaction path
WAL index/compactor list keys under wal_segments_prefix/.../ and only consider .seg with sibling .seg.commit.
Range GET per partition to stream Arrow data.
Exactly-once preserved: commit marker gates visibility; offsets are idempotent; no WAL interaction with deadletters.
Query integration (query.rs)
Resolve the WAL location from the manifest’s wal_segments_prefix.
Register a temporary MemTable by streaming committed segments (as today’s local WAL pathway), partition-pruned by Hive keys.
Keep Parquet output registration unchanged.
Determinism/consistency
S3 provides strong read-after-write for new objects and LIST, so readers will see .seg before .seg.commit; we always gate on .seg.commit.
Recovery: segments without commit are invisible; on startup we may optionally validate/publish or quarantine, but not required for correctness.
Tests
Unit: S3 client mock listing .seg and .seg.commit patterns; ensure only committed segments are consumed.
E2E: ingest with WAL_STORAGE=s3, confirm uploaded_rows == expected_msgs and no duplicates on replay.