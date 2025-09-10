use std::sync::atomic::{AtomicU64, Ordering};
use once_cell::sync::Lazy;

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

pub static PARQUET_PERSISTED_BYTES_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static PARQUET_PERSISTED_ROWS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static PARQUET_PERSISTED_OBJECTS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static LATEST_TIMESTAMP: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));

// Dynamic tuning targets (self-tuned by ingest; read by components)
pub static UPLOAD_CONCURRENCY_TARGET: Lazy<std::sync::atomic::AtomicUsize> = Lazy::new(|| std::sync::atomic::AtomicUsize::new(16));
pub static WAL_COMPACTION_CONCURRENCY_TARGET: Lazy<std::sync::atomic::AtomicUsize> = Lazy::new(|| std::sync::atomic::AtomicUsize::new(16));
pub static S3_DOWNLOAD_CONCURRENCY_TARGET: Lazy<std::sync::atomic::AtomicUsize> = Lazy::new(|| std::sync::atomic::AtomicUsize::new(256));

#[inline]
pub fn add_messages(n: u64) { MESSAGES_TOTAL.fetch_add(n, Ordering::Relaxed); }
#[inline]
pub fn add_deadletters(n: u64) { DEADLETTERS_TOTAL.fetch_add(n, Ordering::Relaxed); }
#[inline]
pub fn add_ingested_slow(n: u64) { INGESTED_SLOW_TOTAL.fetch_add(n, Ordering::Relaxed); }
#[inline]
pub fn add_source_bytes(n: u64) { SOURCE_BYTES_TOTAL.fetch_add(n, Ordering::Relaxed); }

#[inline]
pub fn add_wal_write_bytes(n: u64) { WAL_WRITE_BYTES_TOTAL.fetch_add(n, Ordering::Relaxed); }
#[inline]
pub fn add_wal_write_rows(n: u64) { WAL_WRITE_ROWS_TOTAL.fetch_add(n, Ordering::Relaxed); }
#[inline]
pub fn add_wal_compacted_bytes(n: u64) { WAL_COMPACTED_BYTES_TOTAL.fetch_add(n, Ordering::Relaxed); }
#[inline]
pub fn add_wal_compacted_files(n: u64) { WAL_COMPACTED_FILES_TOTAL.fetch_add(n, Ordering::Relaxed); }
#[inline]
pub fn add_wal_compacted_rows(n: u64) { WAL_COMPACTED_ROWS_TOTAL.fetch_add(n, Ordering::Relaxed); }

#[inline]
pub fn add_parquet_bytes(n: u64) { PARQUET_PERSISTED_BYTES_TOTAL.fetch_add(n, Ordering::Relaxed); }
#[inline]
pub fn add_parquet_rows(n: u64) { PARQUET_PERSISTED_ROWS_TOTAL.fetch_add(n, Ordering::Relaxed); }
#[inline]
pub fn add_parquet_objects(n: u64) { PARQUET_PERSISTED_OBJECTS_TOTAL.fetch_add(n, Ordering::Relaxed); }

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


