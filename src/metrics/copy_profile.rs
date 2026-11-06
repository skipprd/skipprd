//! Cumulative COPY hot-path timings (process-wide).
//!
//! Off unless `skippr.perf_profile`, `SKIPPR_PERF_PROFILE`, or `--log perf`.
//! Mirrors `ingest_profile` so PERF_EVENT can split ingest encode vs compact
//! decode/re-encode/put without a debugger. Reset at sync start.
#![allow(dead_code)]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use once_cell::sync::Lazy;

static ENABLED: AtomicBool = AtomicBool::new(false);

pub static COPY_INGEST_DECODE_NS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static COPY_INGEST_KEY_NS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static COPY_INGEST_NS_PARSE_NS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static COPY_INGEST_PARTITION_PARSE_NS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static COPY_INGEST_TIME_PARSE_NS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static COPY_INGEST_ENCODE_NS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static COPY_INGEST_WAL_SUBMIT_NS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static COPY_INGEST_BATCH_NS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static COPY_INGEST_RECORDS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static COPY_INGEST_KEYS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static COPY_INGEST_BATCHES_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));

pub static COPY_COMPACT_DECODE_NS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static COPY_COMPACT_ENCODE_NS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static COPY_COMPACT_PUT_NS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static COPY_COMPACT_OBJECTS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static COPY_COMPACT_ROWS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static COPY_COMPACT_WAL_PARTS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static COPY_COMPACT_BYTES_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));

#[inline]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

#[inline]
pub fn add_ingest_decode_ns(ns: u64) {
    if !enabled() {
        return;
    }
    COPY_INGEST_DECODE_NS_TOTAL.fetch_add(ns, Ordering::Relaxed);
}

#[inline]
pub fn add_ingest_key_ns(ns: u64) {
    if !enabled() {
        return;
    }
    COPY_INGEST_KEY_NS_TOTAL.fetch_add(ns, Ordering::Relaxed);
}

#[inline]
pub fn add_ingest_ns_parse_ns(ns: u64) {
    if !enabled() {
        return;
    }
    COPY_INGEST_NS_PARSE_NS_TOTAL.fetch_add(ns, Ordering::Relaxed);
}

#[inline]
pub fn add_ingest_partition_parse_ns(ns: u64) {
    if !enabled() {
        return;
    }
    COPY_INGEST_PARTITION_PARSE_NS_TOTAL.fetch_add(ns, Ordering::Relaxed);
}

#[inline]
pub fn add_ingest_time_parse_ns(ns: u64) {
    if !enabled() {
        return;
    }
    COPY_INGEST_TIME_PARSE_NS_TOTAL.fetch_add(ns, Ordering::Relaxed);
}

#[inline]
pub fn add_ingest_encode_ns(ns: u64) {
    if !enabled() {
        return;
    }
    COPY_INGEST_ENCODE_NS_TOTAL.fetch_add(ns, Ordering::Relaxed);
}

#[inline]
pub fn add_ingest_wal_submit_ns(ns: u64) {
    if !enabled() {
        return;
    }
    COPY_INGEST_WAL_SUBMIT_NS_TOTAL.fetch_add(ns, Ordering::Relaxed);
}

#[inline]
pub fn add_ingest_batch_ns(ns: u64) {
    if !enabled() {
        return;
    }
    COPY_INGEST_BATCH_NS_TOTAL.fetch_add(ns, Ordering::Relaxed);
    COPY_INGEST_BATCHES_TOTAL.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub fn add_ingest_records(n: u64) {
    if !enabled() {
        return;
    }
    COPY_INGEST_RECORDS_TOTAL.fetch_add(n, Ordering::Relaxed);
}

#[inline]
pub fn add_ingest_keys(n: u64) {
    if !enabled() {
        return;
    }
    COPY_INGEST_KEYS_TOTAL.fetch_add(n, Ordering::Relaxed);
}

#[inline]
pub fn add_compact_decode_ns(ns: u64) {
    if !enabled() {
        return;
    }
    COPY_COMPACT_DECODE_NS_TOTAL.fetch_add(ns, Ordering::Relaxed);
}

#[inline]
pub fn add_compact_encode_ns(ns: u64) {
    if !enabled() {
        return;
    }
    COPY_COMPACT_ENCODE_NS_TOTAL.fetch_add(ns, Ordering::Relaxed);
}

#[inline]
pub fn add_compact_put_ns(ns: u64) {
    if !enabled() {
        return;
    }
    COPY_COMPACT_PUT_NS_TOTAL.fetch_add(ns, Ordering::Relaxed);
}

#[inline]
pub fn record_compact_object(rows: u64, wal_parts: u64, bytes: u64) {
    if !enabled() {
        return;
    }
    COPY_COMPACT_OBJECTS_TOTAL.fetch_add(1, Ordering::Relaxed);
    COPY_COMPACT_ROWS_TOTAL.fetch_add(rows, Ordering::Relaxed);
    COPY_COMPACT_WAL_PARTS_TOTAL.fetch_add(wal_parts, Ordering::Relaxed);
    COPY_COMPACT_BYTES_TOTAL.fetch_add(bytes, Ordering::Relaxed);
}

pub fn reset_profile_counters() {
    COPY_INGEST_DECODE_NS_TOTAL.store(0, Ordering::Relaxed);
    COPY_INGEST_KEY_NS_TOTAL.store(0, Ordering::Relaxed);
    COPY_INGEST_NS_PARSE_NS_TOTAL.store(0, Ordering::Relaxed);
    COPY_INGEST_PARTITION_PARSE_NS_TOTAL.store(0, Ordering::Relaxed);
    COPY_INGEST_TIME_PARSE_NS_TOTAL.store(0, Ordering::Relaxed);
    COPY_INGEST_ENCODE_NS_TOTAL.store(0, Ordering::Relaxed);
    COPY_INGEST_WAL_SUBMIT_NS_TOTAL.store(0, Ordering::Relaxed);
    COPY_INGEST_BATCH_NS_TOTAL.store(0, Ordering::Relaxed);
    COPY_INGEST_RECORDS_TOTAL.store(0, Ordering::Relaxed);
    COPY_INGEST_KEYS_TOTAL.store(0, Ordering::Relaxed);
    COPY_INGEST_BATCHES_TOTAL.store(0, Ordering::Relaxed);
    COPY_COMPACT_DECODE_NS_TOTAL.store(0, Ordering::Relaxed);
    COPY_COMPACT_ENCODE_NS_TOTAL.store(0, Ordering::Relaxed);
    COPY_COMPACT_PUT_NS_TOTAL.store(0, Ordering::Relaxed);
    COPY_COMPACT_OBJECTS_TOTAL.store(0, Ordering::Relaxed);
    COPY_COMPACT_ROWS_TOTAL.store(0, Ordering::Relaxed);
    COPY_COMPACT_WAL_PARTS_TOTAL.store(0, Ordering::Relaxed);
    COPY_COMPACT_BYTES_TOTAL.store(0, Ordering::Relaxed);
}

#[derive(Debug, Clone, Copy)]
pub struct CopyProfileSnapshot {
    pub ingest_decode_ns: u64,
    pub ingest_key_ns: u64,
    pub ingest_ns_parse_ns: u64,
    pub ingest_partition_parse_ns: u64,
    pub ingest_time_parse_ns: u64,
    pub ingest_encode_ns: u64,
    pub ingest_wal_submit_ns: u64,
    pub ingest_batch_ns: u64,
    pub ingest_records: u64,
    pub ingest_keys: u64,
    pub ingest_batches: u64,
    pub compact_decode_ns: u64,
    pub compact_encode_ns: u64,
    pub compact_put_ns: u64,
    pub compact_objects: u64,
    pub compact_rows: u64,
    pub compact_wal_parts: u64,
    pub compact_bytes: u64,
}

pub fn snapshot() -> CopyProfileSnapshot {
    CopyProfileSnapshot {
        ingest_decode_ns: COPY_INGEST_DECODE_NS_TOTAL.load(Ordering::Relaxed),
        ingest_key_ns: COPY_INGEST_KEY_NS_TOTAL.load(Ordering::Relaxed),
        ingest_ns_parse_ns: COPY_INGEST_NS_PARSE_NS_TOTAL.load(Ordering::Relaxed),
        ingest_partition_parse_ns: COPY_INGEST_PARTITION_PARSE_NS_TOTAL.load(Ordering::Relaxed),
        ingest_time_parse_ns: COPY_INGEST_TIME_PARSE_NS_TOTAL.load(Ordering::Relaxed),
        ingest_encode_ns: COPY_INGEST_ENCODE_NS_TOTAL.load(Ordering::Relaxed),
        ingest_wal_submit_ns: COPY_INGEST_WAL_SUBMIT_NS_TOTAL.load(Ordering::Relaxed),
        ingest_batch_ns: COPY_INGEST_BATCH_NS_TOTAL.load(Ordering::Relaxed),
        ingest_records: COPY_INGEST_RECORDS_TOTAL.load(Ordering::Relaxed),
        ingest_keys: COPY_INGEST_KEYS_TOTAL.load(Ordering::Relaxed),
        ingest_batches: COPY_INGEST_BATCHES_TOTAL.load(Ordering::Relaxed),
        compact_decode_ns: COPY_COMPACT_DECODE_NS_TOTAL.load(Ordering::Relaxed),
        compact_encode_ns: COPY_COMPACT_ENCODE_NS_TOTAL.load(Ordering::Relaxed),
        compact_put_ns: COPY_COMPACT_PUT_NS_TOTAL.load(Ordering::Relaxed),
        compact_objects: COPY_COMPACT_OBJECTS_TOTAL.load(Ordering::Relaxed),
        compact_rows: COPY_COMPACT_ROWS_TOTAL.load(Ordering::Relaxed),
        compact_wal_parts: COPY_COMPACT_WAL_PARTS_TOTAL.load(Ordering::Relaxed),
        compact_bytes: COPY_COMPACT_BYTES_TOTAL.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_stay_zero_until_perf_profile_is_enabled() {
        set_enabled(false);
        reset_profile_counters();
        add_ingest_decode_ns(10);
        add_ingest_records(3);
        record_compact_object(4, 1, 8);
        let off = snapshot();
        assert_eq!(off.ingest_decode_ns, 0);
        assert_eq!(off.ingest_records, 0);
        assert_eq!(off.compact_objects, 0);

        set_enabled(true);
        add_ingest_decode_ns(10);
        add_ingest_records(3);
        record_compact_object(4, 1, 8);
        let on = snapshot();
        assert_eq!(on.ingest_decode_ns, 10);
        assert_eq!(on.ingest_records, 3);
        assert_eq!(on.compact_objects, 1);
        assert_eq!(on.compact_rows, 4);

        set_enabled(false);
        reset_profile_counters();
    }
}
