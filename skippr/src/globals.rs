use std::sync::{Arc};
use std::sync::atomic::{AtomicBool, AtomicU64};

use arc_swap::ArcSwap;
use arrow::datatypes::Schema;
use once_cell::sync::Lazy;

use crate::helpers::logger::Logger;
use crate::helpers::timed_rwlock::TimedRwLock;
use crate::metrics::Metrics;
use crate::discover::PipelineMetadata;

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

