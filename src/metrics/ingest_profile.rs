#![allow(dead_code)]
use std::sync::atomic::{AtomicU64, Ordering};

use once_cell::sync::Lazy;

/// Cumulative nanoseconds spent in ingest hot-path phases (process-wide).
pub static INGEST_DECODE_NS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static INGEST_FAST_PATH_NS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static INGEST_SLOW_PATH_NS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static INGEST_ARROW_JSON_NS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static INGEST_WAL_ENQUEUE_NS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));

pub static INGEST_EXACT_ARROW_ROWS: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static INGEST_EXACT_ARROW_FALLBACK_ROWS: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static INGEST_LEGACY_NORMALIZED_ROWS: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static INGEST_EXACT_PLAN_NS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static INGEST_EXACT_APPEND_NS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static INGEST_EXACT_FINISH_NS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static INGEST_PARTITION_NS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static INGEST_METADATA_LOAD_NS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static INGEST_UNWRAP_NS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static INGEST_BUFFER_OFFSET_NS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));

#[inline]
pub fn add_decode_ns(ns: u64) {
    INGEST_DECODE_NS_TOTAL.fetch_add(ns, Ordering::Relaxed);
}

#[inline]
pub fn add_fast_path_ns(ns: u64) {
    INGEST_FAST_PATH_NS_TOTAL.fetch_add(ns, Ordering::Relaxed);
}

#[inline]
pub fn add_slow_path_ns(ns: u64) {
    INGEST_SLOW_PATH_NS_TOTAL.fetch_add(ns, Ordering::Relaxed);
}

#[inline]
pub fn add_arrow_json_ns(ns: u64) {
    INGEST_ARROW_JSON_NS_TOTAL.fetch_add(ns, Ordering::Relaxed);
}

#[inline]
pub fn add_wal_enqueue_ns(ns: u64) {
    INGEST_WAL_ENQUEUE_NS_TOTAL.fetch_add(ns, Ordering::Relaxed);
}

#[inline]
pub fn add_metadata_load_ns(ns: u64) {
    INGEST_METADATA_LOAD_NS_TOTAL.fetch_add(ns, Ordering::Relaxed);
}

#[inline]
pub fn add_partition_ns(ns: u64) {
    INGEST_PARTITION_NS_TOTAL.fetch_add(ns, Ordering::Relaxed);
}

#[inline]
pub fn add_unwrap_ns(ns: u64) {
    INGEST_UNWRAP_NS_TOTAL.fetch_add(ns, Ordering::Relaxed);
}

#[inline]
pub fn add_buffer_offset_ns(ns: u64) {
    INGEST_BUFFER_OFFSET_NS_TOTAL.fetch_add(ns, Ordering::Relaxed);
}

#[inline]
pub fn add_exact_finish_ns(ns: u64) {
    INGEST_EXACT_FINISH_NS_TOTAL.fetch_add(ns, Ordering::Relaxed);
}

#[inline]
pub fn add_exact_plan_ns(ns: u64) {
    INGEST_EXACT_PLAN_NS_TOTAL.fetch_add(ns, Ordering::Relaxed);
}

#[inline]
pub fn add_exact_append_ns(ns: u64) {
    INGEST_EXACT_APPEND_NS_TOTAL.fetch_add(ns, Ordering::Relaxed);
}

#[inline]
pub fn add_exact_arrow_rows(n: u64) {
    INGEST_EXACT_ARROW_ROWS.fetch_add(n, Ordering::Relaxed);
}

#[inline]
pub fn add_exact_arrow_fallback_rows(n: u64) {
    INGEST_EXACT_ARROW_FALLBACK_ROWS.fetch_add(n, Ordering::Relaxed);
}

#[inline]
pub fn add_legacy_normalized_rows(n: u64) {
    INGEST_LEGACY_NORMALIZED_ROWS.fetch_add(n, Ordering::Relaxed);
}

/// Reset profile counters (benchmark harness only).
/// When true, ingest skips the exact-Arrow path (benchmark baseline only).
#[cfg(test)]
pub static FORCE_LEGACY_INGEST_FOR_BENCHMARK: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
pub fn set_force_legacy_ingest_for_benchmark(force: bool) {
    FORCE_LEGACY_INGEST_FOR_BENCHMARK.store(force, Ordering::Relaxed);
}

pub fn reset_profile_counters() {
    INGEST_DECODE_NS_TOTAL.store(0, Ordering::Relaxed);
    INGEST_FAST_PATH_NS_TOTAL.store(0, Ordering::Relaxed);
    INGEST_SLOW_PATH_NS_TOTAL.store(0, Ordering::Relaxed);
    INGEST_ARROW_JSON_NS_TOTAL.store(0, Ordering::Relaxed);
    INGEST_WAL_ENQUEUE_NS_TOTAL.store(0, Ordering::Relaxed);
    INGEST_EXACT_ARROW_ROWS.store(0, Ordering::Relaxed);
    INGEST_EXACT_ARROW_FALLBACK_ROWS.store(0, Ordering::Relaxed);
    INGEST_LEGACY_NORMALIZED_ROWS.store(0, Ordering::Relaxed);
    INGEST_EXACT_PLAN_NS_TOTAL.store(0, Ordering::Relaxed);
    INGEST_EXACT_APPEND_NS_TOTAL.store(0, Ordering::Relaxed);
    INGEST_EXACT_FINISH_NS_TOTAL.store(0, Ordering::Relaxed);
    INGEST_PARTITION_NS_TOTAL.store(0, Ordering::Relaxed);
    INGEST_METADATA_LOAD_NS_TOTAL.store(0, Ordering::Relaxed);
    INGEST_UNWRAP_NS_TOTAL.store(0, Ordering::Relaxed);
    INGEST_BUFFER_OFFSET_NS_TOTAL.store(0, Ordering::Relaxed);
}

#[derive(Debug, Clone, Copy)]
pub struct IngestProfileSnapshot {
    pub decode_ns: u64,
    pub fast_path_ns: u64,
    pub slow_path_ns: u64,
    pub arrow_json_ns: u64,
    pub wal_enqueue_ns: u64,
    pub exact_arrow_rows: u64,
    pub exact_arrow_fallback_rows: u64,
    pub legacy_normalized_rows: u64,
    pub exact_plan_ns: u64,
    pub exact_append_ns: u64,
    pub exact_finish_ns: u64,
    pub partition_ns: u64,
    pub metadata_load_ns: u64,
    pub unwrap_ns: u64,
    pub buffer_offset_ns: u64,
}

pub fn snapshot() -> IngestProfileSnapshot {
    IngestProfileSnapshot {
        decode_ns: INGEST_DECODE_NS_TOTAL.load(Ordering::Relaxed),
        fast_path_ns: INGEST_FAST_PATH_NS_TOTAL.load(Ordering::Relaxed),
        slow_path_ns: INGEST_SLOW_PATH_NS_TOTAL.load(Ordering::Relaxed),
        arrow_json_ns: INGEST_ARROW_JSON_NS_TOTAL.load(Ordering::Relaxed),
        wal_enqueue_ns: INGEST_WAL_ENQUEUE_NS_TOTAL.load(Ordering::Relaxed),
        exact_arrow_rows: INGEST_EXACT_ARROW_ROWS.load(Ordering::Relaxed),
        exact_arrow_fallback_rows: INGEST_EXACT_ARROW_FALLBACK_ROWS.load(Ordering::Relaxed),
        legacy_normalized_rows: INGEST_LEGACY_NORMALIZED_ROWS.load(Ordering::Relaxed),
        exact_plan_ns: INGEST_EXACT_PLAN_NS_TOTAL.load(Ordering::Relaxed),
        exact_append_ns: INGEST_EXACT_APPEND_NS_TOTAL.load(Ordering::Relaxed),
        exact_finish_ns: INGEST_EXACT_FINISH_NS_TOTAL.load(Ordering::Relaxed),
        partition_ns: INGEST_PARTITION_NS_TOTAL.load(Ordering::Relaxed),
        metadata_load_ns: INGEST_METADATA_LOAD_NS_TOTAL.load(Ordering::Relaxed),
        unwrap_ns: INGEST_UNWRAP_NS_TOTAL.load(Ordering::Relaxed),
        buffer_offset_ns: INGEST_BUFFER_OFFSET_NS_TOTAL.load(Ordering::Relaxed),
    }
}
