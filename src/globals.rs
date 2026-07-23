use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use arrow::datatypes::Schema;
use once_cell::sync::Lazy;

use crate::discover::PipelineMetadata;
use crate::helpers::logger::Logger;
use crate::helpers::timed_rwlock::TimedRwLock;
use crate::metrics::Metrics;

/// Process-level runtime flags used by ingestion and other subsystems.
pub static RUNNING: Lazy<TimedRwLock<AtomicBool>> =
    Lazy::new(|| TimedRwLock::new("running".to_string(), AtomicBool::new(true)));

pub static OUTPUT_RUNNING: Lazy<TimedRwLock<AtomicBool>> =
    Lazy::new(|| TimedRwLock::new("output_running".to_string(), AtomicBool::new(false)));

pub static OUTPUT_GRACEFUL_SHUTDOWN_COMPLETE: Lazy<TimedRwLock<AtomicBool>> = Lazy::new(|| {
    TimedRwLock::new(
        "output_graceful_shutdown_complete".to_string(),
        AtomicBool::new(false),
    )
});

/// Set when DATA_DIR is exhausted and no reclaimable WAL remains to compact.
pub static DATA_DIR_CAPACITY_EXCEEDED: Lazy<AtomicBool> = Lazy::new(|| AtomicBool::new(false));
pub static DATA_DIR_INGEST_PAUSED: Lazy<AtomicBool> = Lazy::new(|| AtomicBool::new(false));

static DATA_DIR_CAPACITY_ERROR: Lazy<Mutex<Option<String>>> = Lazy::new(|| Mutex::new(None));

pub fn record_data_dir_capacity_error(message: String) {
    *DATA_DIR_CAPACITY_ERROR.lock().unwrap() = Some(message);
    DATA_DIR_CAPACITY_EXCEEDED.store(true, Ordering::SeqCst);
    RUNNING.write().store(false, Ordering::SeqCst);
}

pub fn data_dir_capacity_exceeded() -> bool {
    DATA_DIR_CAPACITY_EXCEEDED.load(Ordering::SeqCst)
}

pub fn take_data_dir_capacity_error() -> Option<String> {
    DATA_DIR_CAPACITY_ERROR.lock().unwrap().take()
}

pub fn data_dir_ingest_paused() -> bool {
    DATA_DIR_INGEST_PAUSED.load(Ordering::SeqCst)
}

pub fn set_data_dir_ingest_paused(paused: bool) {
    DATA_DIR_INGEST_PAUSED.store(paused, Ordering::SeqCst);
}

/// Reset process-wide ingest pause flags between benchmark / unit tests.
#[cfg(test)]
pub fn reset_data_dir_capacity_state_for_test() {
    use std::sync::atomic::Ordering;
    use crate::metrics::counters::{
        LAST_WAL_RECLAIM_PROGRESS_EPOCH_SECS, WAL_BYTES_RECLAIMED_TOTAL,
        WAL_SEGMENTS_RECLAIMED_TOTAL,
    };
    DATA_DIR_CAPACITY_EXCEEDED.store(false, Ordering::SeqCst);
    set_data_dir_ingest_paused(false);
    let _ = DATA_DIR_CAPACITY_ERROR.lock().unwrap().take();
    WAL_SEGMENTS_RECLAIMED_TOTAL.store(0, Ordering::SeqCst);
    WAL_BYTES_RECLAIMED_TOTAL.store(0, Ordering::SeqCst);
    LAST_WAL_RECLAIM_PROGRESS_EPOCH_SECS.store(0, Ordering::SeqCst);
}

pub static LOGGER: Lazy<Arc<tokio::sync::RwLock<Logger>>> = Lazy::new(|| Logger::new(100));
pub static METRICS: Lazy<Arc<TimedRwLock<Metrics>>> =
    Lazy::new(|| Arc::new(TimedRwLock::new("metrics".to_string(), Metrics::new())));

/// Publish pipeline metadata via ArcSwap; readers do lock-free loads.
pub static METADATA: Lazy<ArcSwap<PipelineMetadata>> =
    Lazy::new(|| ArcSwap::new(Arc::new(PipelineMetadata::new())));

/// Per-namespace Arrow schema snapshots and versions.
pub static ARROW_SCHEMA: Lazy<dashmap::DashMap<String, ArcSwap<Schema>>> =
    Lazy::new(|| dashmap::DashMap::new());
pub static ARROW_SCHEMA_VERSION: Lazy<dashmap::DashMap<String, AtomicU64>> =
    Lazy::new(|| dashmap::DashMap::new());

/// Monotonic latest-only schema version for the whole pipeline runtime.
pub static PIPELINE_SCHEMA_VERSION: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));

/// Serializes tests that mutate [`METADATA`] or other process-wide pipeline globals.
#[cfg(test)]
pub static METADATA_TEST_LOCK: Lazy<std::sync::Mutex<()>> = Lazy::new(|| std::sync::Mutex::new(()));

#[cfg(test)]
pub fn metadata_test_lock() -> std::sync::MutexGuard<'static, ()> {
    METADATA_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
