use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use once_cell::sync::Lazy;

pub static UPLOAD_CONCURRENCY_TARGET: Lazy<AtomicUsize> = Lazy::new(|| AtomicUsize::new(16));
pub static MULTIPART_PART_CONCURRENCY_TARGET: Lazy<AtomicUsize> = Lazy::new(|| AtomicUsize::new(2));
pub static S3_DOWNLOAD_CONCURRENCY_TARGET: Lazy<AtomicUsize> = Lazy::new(|| AtomicUsize::new(256));
pub static ATHENA_GLUE_CP_TARGET: Lazy<AtomicUsize> = Lazy::new(|| AtomicUsize::new(2));

pub static PARQUET_PERSISTED_BYTES_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static PARQUET_PERSISTED_ROWS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static PARQUET_PERSISTED_OBJECTS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static UPLOADS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static UPLOAD_LATENCY_NS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static UPLOADS_IN_FLIGHT: Lazy<AtomicUsize> = Lazy::new(|| AtomicUsize::new(0));
pub static GLUE_RETRIES_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));

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
    let _ = UPLOADS_IN_FLIGHT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_sub(1))
    });
}
#[inline]
pub fn add_glue_retry(n: u64) {
    GLUE_RETRIES_TOTAL.fetch_add(n, Ordering::Relaxed);
}
