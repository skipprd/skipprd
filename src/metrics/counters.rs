#![allow(dead_code)]
use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicU64, Ordering};

// Lock-free, process-wide counters used across hot paths
pub static MESSAGES_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static DEADLETTERS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static INGESTED_SLOW_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static SOURCE_BYTES_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));

pub static WAL_WRITE_BYTES_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static WAL_WRITE_ROWS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static WAL_COMPACTED_BYTES_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static WAL_COMPACTED_FILES_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static WAL_COMPACTED_ROWS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static WAL_COMPACTIONS_STARTED: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static WAL_COMPACTIONS_COMPLETED: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static WAL_COMPACTION_TRANSACTIONS_STARTED: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static WAL_COMPACTION_TRANSACTIONS_COMPLETED: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static WAL_COMPACTION_TRANSACTIONS_FAILED: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static WAL_COMPACTION_REFS_TOMBSTONED: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static WAL_SEGMENTS_RECLAIMED_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static WAL_BYTES_RECLAIMED_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static LAST_WAL_RECLAIM_PROGRESS_EPOCH_SECS: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static WAL_COMPACTIONS_IN_FLIGHT: Lazy<std::sync::atomic::AtomicUsize> =
    Lazy::new(|| std::sync::atomic::AtomicUsize::new(0));

pub static PARQUET_PERSISTED_BYTES_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static PARQUET_PERSISTED_ROWS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static PARQUET_PERSISTED_OBJECTS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static LATEST_TIMESTAMP: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static QUARANTINED_PARTITIONS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));

// (removed unused LLM/semantic counters)

// Dynamic tuning targets (self-tuned by ingest; read by components)
pub static UPLOAD_CONCURRENCY_TARGET: Lazy<std::sync::atomic::AtomicUsize> =
    Lazy::new(|| std::sync::atomic::AtomicUsize::new(16));
pub static WAL_COMPACTION_CONCURRENCY_TARGET: Lazy<std::sync::atomic::AtomicUsize> =
    Lazy::new(|| std::sync::atomic::AtomicUsize::new(16));
pub static S3_DOWNLOAD_CONCURRENCY_TARGET: Lazy<std::sync::atomic::AtomicUsize> =
    Lazy::new(|| std::sync::atomic::AtomicUsize::new(256));

// WAL S3 error telemetry
pub static S3_WAL_RETRIES_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static S3_WAL_ERRORS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
// Exponential moving average of retries per tick, scaled by 100 (for decimals)
pub static S3_WAL_RETRY_EMA_X100: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));

// Upload telemetry
pub static UPLOADS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static UPLOAD_LATENCY_NS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static UPLOADS_IN_FLIGHT: Lazy<std::sync::atomic::AtomicUsize> =
    Lazy::new(|| std::sync::atomic::AtomicUsize::new(0));

// Ingest runtime telemetry
pub static ACTIVE_THREADS: Lazy<std::sync::atomic::AtomicUsize> =
    Lazy::new(|| std::sync::atomic::AtomicUsize::new(0));
pub static QUEUE_LENGTH: Lazy<std::sync::atomic::AtomicUsize> =
    Lazy::new(|| std::sync::atomic::AtomicUsize::new(0));

#[inline]
pub fn add_messages(n: u64) {
    MESSAGES_TOTAL.fetch_add(n, Ordering::Relaxed);
}
#[inline]
pub fn add_deadletters(n: u64) {
    DEADLETTERS_TOTAL.fetch_add(n, Ordering::Relaxed);
}
#[inline]
pub fn add_ingested_slow(n: u64) {
    INGESTED_SLOW_TOTAL.fetch_add(n, Ordering::Relaxed);
}
#[inline]
pub fn add_source_bytes(n: u64) {
    SOURCE_BYTES_TOTAL.fetch_add(n, Ordering::Relaxed);
}

#[inline]
pub fn add_wal_write_bytes(n: u64) {
    WAL_WRITE_BYTES_TOTAL.fetch_add(n, Ordering::Relaxed);
}
#[inline]
pub fn add_wal_write_rows(n: u64) {
    WAL_WRITE_ROWS_TOTAL.fetch_add(n, Ordering::Relaxed);
}
#[inline]
pub fn add_wal_compacted_bytes(n: u64) {
    WAL_COMPACTED_BYTES_TOTAL.fetch_add(n, Ordering::Relaxed);
}
#[inline]
pub fn add_wal_compacted_files(n: u64) {
    WAL_COMPACTED_FILES_TOTAL.fetch_add(n, Ordering::Relaxed);
}
#[inline]
pub fn add_wal_compacted_rows(n: u64) {
    WAL_COMPACTED_ROWS_TOTAL.fetch_add(n, Ordering::Relaxed);
}
#[inline]
pub fn add_wal_compaction_started(n: u64) {
    WAL_COMPACTIONS_STARTED.fetch_add(n, Ordering::Relaxed);
}
#[inline]
pub fn add_wal_compaction_completed(n: u64) {
    WAL_COMPACTIONS_COMPLETED.fetch_add(n, Ordering::Relaxed);
}
#[inline]
pub fn add_wal_compaction_transaction_started(n: u64) {
    WAL_COMPACTION_TRANSACTIONS_STARTED.fetch_add(n, Ordering::Relaxed);
}
#[inline]
pub fn add_wal_compaction_transaction_completed(n: u64) {
    WAL_COMPACTION_TRANSACTIONS_COMPLETED.fetch_add(n, Ordering::Relaxed);
}
#[inline]
pub fn add_wal_compaction_transaction_failed(n: u64) {
    WAL_COMPACTION_TRANSACTIONS_FAILED.fetch_add(n, Ordering::Relaxed);
}
#[inline]
pub fn add_wal_compaction_refs_tombstoned(n: u64) {
    WAL_COMPACTION_REFS_TOMBSTONED.fetch_add(n, Ordering::Relaxed);
}

pub fn record_wal_segment_reclaimed(bytes: u64) {
    WAL_SEGMENTS_RECLAIMED_TOTAL.fetch_add(1, Ordering::Relaxed);
    WAL_BYTES_RECLAIMED_TOTAL.fetch_add(bytes, Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    LAST_WAL_RECLAIM_PROGRESS_EPOCH_SECS.store(now, Ordering::Relaxed);
}
#[inline]
pub fn inc_wal_compactions_in_flight() {
    WAL_COMPACTIONS_IN_FLIGHT.fetch_add(1, Ordering::Relaxed);
}
#[inline]
pub fn dec_wal_compactions_in_flight() {
    let _ = WAL_COMPACTIONS_IN_FLIGHT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_sub(1))
    });
}

#[inline]
pub fn add_parquet_bytes(n: u64) {
    PARQUET_PERSISTED_BYTES_TOTAL.fetch_add(n, Ordering::Relaxed);
}
#[inline]
pub fn add_parquet_rows(n: u64) {
    PARQUET_PERSISTED_ROWS_TOTAL.fetch_add(n, Ordering::Relaxed);
}
#[inline]
pub fn add_parquet_objects(n: u64) {
    PARQUET_PERSISTED_OBJECTS_TOTAL.fetch_add(n, Ordering::Relaxed);
}
#[inline]
pub fn add_quarantined_partitions(n: u64) {
    QUARANTINED_PARTITIONS_TOTAL.fetch_add(n, Ordering::Relaxed);
}

#[inline]
pub fn update_latest_timestamp_max(ts: u64) {
    let mut current = LATEST_TIMESTAMP.load(Ordering::Relaxed);
    while ts > current {
        match LATEST_TIMESTAMP.compare_exchange(current, ts, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(prev) => current = prev,
        }
    }
}

#[inline]
pub fn add_upload(n: u64) {
    UPLOADS_TOTAL.fetch_add(n, Ordering::Relaxed);
}
#[inline]
pub fn add_upload_latency_ns(n: u64) {
    UPLOAD_LATENCY_NS_TOTAL.fetch_add(n, Ordering::Relaxed);
}
#[inline]
pub fn inc_uploads_in_flight() {
    UPLOADS_IN_FLIGHT.fetch_add(1, Ordering::Relaxed);
}
#[inline]
pub fn dec_uploads_in_flight() {
    UPLOADS_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
}
#[inline]
pub fn set_active_threads(n: usize) {
    ACTIVE_THREADS.store(n, Ordering::Relaxed);
}
#[inline]
pub fn set_queue_length(n: usize) {
    QUEUE_LENGTH.store(n, Ordering::Relaxed);
}

#[inline]
pub fn add_s3_wal_retry(n: u64) {
    S3_WAL_RETRIES_TOTAL.fetch_add(n, Ordering::Relaxed);
}
#[inline]
pub fn add_s3_wal_error(n: u64) {
    S3_WAL_ERRORS_TOTAL.fetch_add(n, Ordering::Relaxed);
}
#[inline]
pub fn set_s3_wal_retry_ema_x100(v: u64) {
    S3_WAL_RETRY_EMA_X100.store(v, Ordering::Relaxed);
}

// (removed unused LLM/semantic add helpers)
