use chrono::DateTime;
use core::time::Duration;
use std::collections::HashMap;
use std::fmt::Debug;
use std::ops::Sub;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use crate::buffer::ingest_buffer::WalIndexMetrics;
use serde_derive::{Deserialize, Serialize};
use serde_json::json;
use tokio::runtime;

use crate::helpers::configuration::Config;
use crate::helpers::timed_rwlock::TimedRwLock;
use crate::helpers::Helpers;
use crate::{METRICS, RUNNING};
use tracing::{error, info};

pub mod counters;
pub mod ingest_profile;

pub static LAST_MESSAGES_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static LAST_FIXED_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static LAST_DEADLETTERS_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static LAST_SOURCE_BYTES_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static LAST_WAL_WRITE_BYTES_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static LAST_WAL_WRITE_ROWS_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static LAST_WAL_COMPACTED_BYTES_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static LAST_WAL_COMPACTED_FILES_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static LAST_PRINT_WAL_COMPACTIONS_STARTED: AtomicU64 = AtomicU64::new(0);
pub static LAST_PRINT_WAL_COMPACTIONS_COMPLETED: AtomicU64 = AtomicU64::new(0);
pub static LAST_PARQUET_PERSISTED_BYTES_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static LAST_PARQUET_PERSISTED_ROWS_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static LAST_PARQUET_PERSISTED_OBJECTS_TOTAL: AtomicU64 = AtomicU64::new(0);
// Printer-only delta state (separate from upload deltas)
pub static LAST_PRINT_MESSAGES_TOTAL: AtomicU64 = AtomicU64::new(0);

#[cfg(unix)]
fn data_dir_disk_usage() -> Option<(u64, f64)> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    let data_dir = Config::get_data_dir();
    let path = Path::new(&data_dir);
    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), stats.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    let stats = unsafe { stats.assume_init() };
    let block_size = u128::from(if stats.f_frsize > 0 {
        stats.f_frsize
    } else {
        stats.f_bsize
    });
    let total_bytes = u64::try_from(u128::from(stats.f_blocks).checked_mul(block_size)?).ok()?;
    let avail_bytes = u64::try_from(u128::from(stats.f_bavail).checked_mul(block_size)?).ok()?;
    if total_bytes == 0 {
        return None;
    }
    let used_pct = 100.0 - ((avail_bytes as f64 * 100.0) / total_bytes as f64);
    Some((avail_bytes, used_pct))
}

#[cfg(not(unix))]
fn data_dir_disk_usage() -> Option<(u64, f64)> {
    None
}

fn average_duration_ms(total_ns: u64, count: u64) -> f64 {
    if count == 0 {
        0.0
    } else {
        total_ns as f64 / count as f64 / 1_000_000.0
    }
}

const VERSION: Option<&str> = option_env!("CARGO_PKG_VERSION");

#[derive(Debug, Deserialize, Serialize)]
struct MetricsEnvConfig {
    data_source_plugin_name: String,
    data_output_plugin_name: String,
    schema_output_plugin_name: String,
    data_source_batch_size_bytes: i64,
    data_source_batch_size_seconds: i64,
    buffer_threshold_bytes: u64,
    buffer_threshold_seconds: u64,
    transform_namespace_fields: String,
    transform_batch_partition_fields: String,
    transform_flatten_events: String,
    transform_batch_time_fields: String,
    transform_batch_time_units: String,
    transform_batch_order_fields: String,
    config_dependency_valid: bool,
    config_dependency_error_count: usize,
    data_dir: String,
    chaos_mode: String,
    input_format: String,
    output_format: String,
}

impl MetricsEnvConfig {
    // default()
    fn new() -> Self {
        Self {
            data_source_plugin_name: Config::get_pipeline_input_plugin_name(),
            data_output_plugin_name: Config::get_pipeline_output_plugin_name(),
            schema_output_plugin_name: Config::get_pipeline_schema_plugin_name(),
            data_source_batch_size_bytes: match Config::get_pipeline_input_plugin_config() {
                Ok(config) => config.batch_size_bytes().or(Some(0)).unwrap(),
                Err(_) => 0,
            },
            data_source_batch_size_seconds: match Config::get_pipeline_input_plugin_config() {
                Ok(config) => config.batch_size_seconds().or(Some(0)).unwrap(),
                Err(_) => 0,
            },
            buffer_threshold_bytes: Config::get_pipeline_buffer_threshold_bytes() as u64,
            buffer_threshold_seconds: Config::get_pipeline_buffer_threshold_seconds() as u64,
            transform_namespace_fields: Config::get_transform_namespace_fields(),
            transform_batch_partition_fields: Config::get_transform_batch_partition_fields(),
            transform_flatten_events: Config::get_transform_flatten_events().to_string(),
            transform_batch_time_fields: Config::get_transform_batch_time_fields(),
            transform_batch_time_units: Config::get_transform_batch_time_unit(),
            transform_batch_order_fields: Config::get_transform_batch_order_fields(),
            config_dependency_valid: Config::config_dependencies_valid(),
            config_dependency_error_count: Config::get_config_dependency_violations().len(),
            data_dir: Config::get_pipeline_data_dir(),
            chaos_mode: Config::get_pipeline_chaos_mode().to_string(),
            input_format: match Config::get_pipeline_input_plugin_config() {
                Ok(config) => config.format().to_string(),
                Err(_) => String::from(""),
            },
            output_format: match Config::get_pipeline_output_plugin_config() {
                Ok(config) => config.format().to_string(),
                Err(_) => String::from(""),
            },
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum MetricsStatus {
    Running,
    Stopped,
    Finishing,
    Completed,
    Error,
    Unknown,
}

impl MetricsStatus {
    pub fn name(&self) -> &'static str {
        match self {
            MetricsStatus::Running => "Running",
            MetricsStatus::Stopped => "Stopped",
            MetricsStatus::Finishing => "Finishing",
            MetricsStatus::Completed => "Completed",
            MetricsStatus::Error => "Error",
            MetricsStatus::Unknown => "Unknown",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Metrics {
    pub source_bytes_total: u64,

    pub messages_total: u64,
    pub deadletters_total: u64,
    pub ingeted_slow_total: u64,

    pub wal_index_namespaces_total: u64,
    pub wal_index_partitions_total: u64,
    pub wal_index_files_total: u64,
    pub wal_index_bytes_total: u64,
    pub wal_index_metrics: WalIndexMetrics,

    pub wal_write_bytes_total: u64,
    pub wal_write_rows_total: u64,

    pub wal_compacted_bytes_total: u64,
    pub wal_compacted_rows_total: u64,
    pub wal_compacted_files_total: u64,

    pub parquet_persisted_bytes_total: u64,
    pub parquet_persisted_rows_total: u64,
    pub parquet_persisted_objects_total: u64,

    pub latest_timestamp: u64,

    pub offset_db_size: u64,

    pub start_time: DateTime<chrono::Utc>,
    pub status: MetricsStatus,
    pub run_id: String,
}

impl Metrics {
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self {
            source_bytes_total: 0,

            messages_total: 0,
            deadletters_total: 0,
            ingeted_slow_total: 0,

            wal_index_namespaces_total: 0,
            wal_index_partitions_total: 0,
            wal_index_files_total: 0,
            wal_index_bytes_total: 0,
            wal_index_metrics: WalIndexMetrics::new(),

            wal_write_bytes_total: 0,
            wal_write_rows_total: 0,

            wal_compacted_bytes_total: 0,
            wal_compacted_rows_total: 0,
            wal_compacted_files_total: 0,

            parquet_persisted_bytes_total: 0,
            parquet_persisted_rows_total: 0,
            parquet_persisted_objects_total: 0,

            latest_timestamp: 0,

            offset_db_size: 0,

            start_time: DateTime::<chrono::Utc>::from(SystemTime::now()),
            status: MetricsStatus::Unknown,
            run_id: Helpers::random_password(32),
        }
    }

    pub fn reset(&mut self) {
        self.source_bytes_total = 0;

        self.messages_total = 0;
        self.deadletters_total = 0;
        self.ingeted_slow_total = 0;

        self.wal_index_namespaces_total = 0;
        self.wal_index_partitions_total = 0;
        self.wal_index_files_total = 0;
        self.wal_index_bytes_total = 0;
        self.wal_index_metrics = WalIndexMetrics::new();

        self.wal_write_bytes_total = 0;
        self.wal_write_rows_total = 0;

        self.wal_compacted_bytes_total = 0;
        self.wal_compacted_rows_total = 0;
        self.wal_compacted_files_total = 0;

        self.parquet_persisted_bytes_total = 0;
        self.parquet_persisted_rows_total = 0;
        self.parquet_persisted_objects_total = 0;

        self.latest_timestamp = 0;

        self.offset_db_size = 0;

        self.start_time = DateTime::<chrono::Utc>::from(SystemTime::now());
        self.status = MetricsStatus::Unknown;

        LAST_WAL_WRITE_BYTES_TOTAL.store(0, Ordering::SeqCst);
        LAST_WAL_WRITE_ROWS_TOTAL.store(0, Ordering::SeqCst);
        LAST_WAL_COMPACTED_BYTES_TOTAL.store(0, Ordering::SeqCst);
        LAST_WAL_COMPACTED_FILES_TOTAL.store(0, Ordering::SeqCst);
        LAST_PARQUET_PERSISTED_BYTES_TOTAL.store(0, Ordering::SeqCst);
        LAST_PARQUET_PERSISTED_ROWS_TOTAL.store(0, Ordering::SeqCst);
        LAST_PARQUET_PERSISTED_OBJECTS_TOTAL.store(0, Ordering::SeqCst);
        LAST_SOURCE_BYTES_TOTAL.store(0, Ordering::SeqCst);
        LAST_MESSAGES_TOTAL.store(0, Ordering::SeqCst);
        LAST_FIXED_TOTAL.store(0, Ordering::SeqCst);
        LAST_DEADLETTERS_TOTAL.store(0, Ordering::SeqCst);
    }

    pub async fn send_metrics<'a>(exit_code: Option<i8>) -> Result<(), Box<dyn std::error::Error>> {
        let metrics: Metrics;
        {
            metrics = METRICS.read().clone();
        }

        let workspace = Config::get_workspace_name();
        let pipeline = Config::get_pipeline_name();

        let tenant = Config::get_tenant();

        // Merge counters (atomics) into snapshot before computing deltas
        use crate::metrics::counters;
        let messages_total_counter = counters::MESSAGES_TOTAL.load(Ordering::Relaxed);
        let deadletters_total_counter = counters::DEADLETTERS_TOTAL.load(Ordering::Relaxed);
        let ingested_slow_total_counter = counters::INGESTED_SLOW_TOTAL.load(Ordering::Relaxed);
        let source_bytes_total_counter = counters::SOURCE_BYTES_TOTAL.load(Ordering::Relaxed);
        let wal_write_bytes_total_counter = counters::WAL_WRITE_BYTES_TOTAL.load(Ordering::Relaxed);
        let wal_write_rows_total_counter = counters::WAL_WRITE_ROWS_TOTAL.load(Ordering::Relaxed);
        let wal_compacted_bytes_total_counter =
            counters::WAL_COMPACTED_BYTES_TOTAL.load(Ordering::Relaxed);
        let wal_compacted_files_total_counter =
            counters::WAL_COMPACTED_FILES_TOTAL.load(Ordering::Relaxed);
        let wal_compacted_rows_total_counter =
            counters::WAL_COMPACTED_ROWS_TOTAL.load(Ordering::Relaxed);
        let parquet_persisted_bytes_total_counter =
            counters::PARQUET_PERSISTED_BYTES_TOTAL.load(Ordering::Relaxed);
        let parquet_persisted_rows_total_counter =
            counters::PARQUET_PERSISTED_ROWS_TOTAL.load(Ordering::Relaxed);
        let parquet_persisted_objects_total_counter =
            counters::PARQUET_PERSISTED_OBJECTS_TOTAL.load(Ordering::Relaxed);

        let mut metrics_snapshot = metrics.clone();
        metrics_snapshot.messages_total += messages_total_counter;
        metrics_snapshot.deadletters_total += deadletters_total_counter;
        metrics_snapshot.ingeted_slow_total += ingested_slow_total_counter;
        metrics_snapshot.source_bytes_total += source_bytes_total_counter;
        metrics_snapshot.wal_write_bytes_total += wal_write_bytes_total_counter;
        metrics_snapshot.wal_write_rows_total += wal_write_rows_total_counter;
        metrics_snapshot.wal_compacted_bytes_total += wal_compacted_bytes_total_counter;
        metrics_snapshot.wal_compacted_files_total += wal_compacted_files_total_counter;
        metrics_snapshot.wal_compacted_rows_total += wal_compacted_rows_total_counter;
        metrics_snapshot.parquet_persisted_bytes_total += parquet_persisted_bytes_total_counter;
        metrics_snapshot.parquet_persisted_rows_total += parquet_persisted_rows_total_counter;
        metrics_snapshot.parquet_persisted_objects_total += parquet_persisted_objects_total_counter;

        let wal_write_bytes_total = LAST_WAL_WRITE_BYTES_TOTAL.load(Ordering::SeqCst);
        let wal_write_bytes_current = metrics_snapshot
            .wal_write_bytes_total
            .saturating_sub(wal_write_bytes_total);
        LAST_WAL_WRITE_BYTES_TOTAL.store(metrics_snapshot.wal_write_bytes_total, Ordering::SeqCst);

        let wal_write_rows_total = LAST_WAL_WRITE_ROWS_TOTAL.load(Ordering::SeqCst);
        let wal_write_rows_current = metrics_snapshot
            .wal_write_rows_total
            .saturating_sub(wal_write_rows_total);
        LAST_WAL_WRITE_ROWS_TOTAL.store(metrics_snapshot.wal_write_rows_total, Ordering::SeqCst);

        let wal_compacted_bytes_total = LAST_WAL_COMPACTED_BYTES_TOTAL.load(Ordering::SeqCst);
        let wal_compacted_bytes_current = metrics_snapshot
            .wal_compacted_bytes_total
            .saturating_sub(wal_compacted_bytes_total);
        LAST_WAL_COMPACTED_BYTES_TOTAL
            .store(metrics_snapshot.wal_compacted_bytes_total, Ordering::SeqCst);

        let wal_compacted_files_total = LAST_WAL_COMPACTED_FILES_TOTAL.load(Ordering::SeqCst);
        let wal_compacted_files_current = metrics_snapshot
            .wal_compacted_files_total
            .saturating_sub(wal_compacted_files_total);
        LAST_WAL_COMPACTED_FILES_TOTAL
            .store(metrics_snapshot.wal_compacted_files_total, Ordering::SeqCst);

        // Now compute parquet deltas from merged snapshot
        let parquet_persisted_bytes_total =
            LAST_PARQUET_PERSISTED_BYTES_TOTAL.load(Ordering::SeqCst);
        let parquet_persisted_bytes_current =
            metrics_snapshot.parquet_persisted_bytes_total - parquet_persisted_bytes_total;
        LAST_PARQUET_PERSISTED_BYTES_TOTAL.store(
            metrics_snapshot.parquet_persisted_bytes_total,
            Ordering::SeqCst,
        );

        let parquet_persisted_rows_total = LAST_PARQUET_PERSISTED_ROWS_TOTAL.load(Ordering::SeqCst);
        let parquet_persisted_rows_current =
            metrics_snapshot.parquet_persisted_rows_total - parquet_persisted_rows_total;
        LAST_PARQUET_PERSISTED_ROWS_TOTAL.store(
            metrics_snapshot.parquet_persisted_rows_total,
            Ordering::SeqCst,
        );

        let parquet_persisted_objects_total =
            LAST_PARQUET_PERSISTED_OBJECTS_TOTAL.load(Ordering::SeqCst);
        let parquet_persisted_objects_current =
            metrics_snapshot.parquet_persisted_objects_total - parquet_persisted_objects_total;
        LAST_PARQUET_PERSISTED_OBJECTS_TOTAL.store(
            metrics_snapshot.parquet_persisted_objects_total,
            Ordering::SeqCst,
        );

        let source_bytes_total = LAST_SOURCE_BYTES_TOTAL.load(Ordering::SeqCst);
        let source_bytes_current = metrics_snapshot.source_bytes_total - source_bytes_total;
        LAST_SOURCE_BYTES_TOTAL.store(metrics_snapshot.source_bytes_total, Ordering::SeqCst);

        let last_messages_total = LAST_MESSAGES_TOTAL.load(Ordering::SeqCst);
        let ingested_current = metrics_snapshot.messages_total - last_messages_total;
        LAST_MESSAGES_TOTAL.store(metrics_snapshot.messages_total, Ordering::SeqCst);

        let last_fixed_total = LAST_FIXED_TOTAL.load(Ordering::SeqCst);
        let fixed_current = metrics_snapshot.ingeted_slow_total - last_fixed_total;
        LAST_FIXED_TOTAL.store(metrics_snapshot.ingeted_slow_total, Ordering::SeqCst);

        let last_deadletters_total = LAST_DEADLETTERS_TOTAL.load(Ordering::SeqCst);
        let deadletters_current = metrics_snapshot.deadletters_total - last_deadletters_total;
        LAST_DEADLETTERS_TOTAL.store(metrics_snapshot.deadletters_total, Ordering::SeqCst);

        // Merge latest timestamp from atomics as well
        metrics_snapshot.latest_timestamp = std::cmp::max(
            metrics_snapshot.latest_timestamp,
            crate::metrics::counters::LATEST_TIMESTAMP.load(Ordering::Relaxed),
        );

        let start_time_utc_str = metrics_snapshot
            .start_time
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        // get runtime in seconds from metrics.start_time
        let current_time = chrono::Utc::now();
        let run_time_seconds = (current_time - metrics_snapshot.start_time).num_seconds();

        let total_times: Vec<(String, Duration)> = TimedRwLock::<()>::get_total_wait_times();
        let wait_times: HashMap<String, Duration> = total_times.iter().cloned().collect();
        let flush_metrics = counters::flush_metrics_snapshot();
        let flush_budget = crate::ingest::tuner::current_flush_budget();
        let flush_budget_metrics = json!({
            "generation": flush_budget.generation,
            "reason": flush_budget.reason.as_str(),
            "scheduler_jobs": flush_budget.scheduler_jobs,
            "decode_jobs": flush_budget.decode_jobs,
            "sink_sessions": flush_budget.sink_sessions,
            "upload_sessions": flush_budget.upload_sessions,
            "multipart_parts": flush_budget.multipart_parts,
            "catalog_operations": flush_budget.catalog_operations,
            "ingest_reserved_cores": flush_budget.ingest_reserved_cores,
        });

        let mut data = json!({
            "metrics": {
                "ingested_total": metrics_snapshot.messages_total,
                "fixed_total": metrics_snapshot.ingeted_slow_total,
                "deadletters_total": metrics_snapshot.deadletters_total,
                "ingested_current": ingested_current,
                "fixed_current": fixed_current,
                "deadletters_current": deadletters_current,
                "latest_timestamp": metrics_snapshot.latest_timestamp,
                "offset_db_size": metrics_snapshot.offset_db_size,
                "run_time_seconds": run_time_seconds,
                "bytes_current": source_bytes_current,
                "wal_index_namespaces_total": metrics_snapshot.wal_index_namespaces_total,
                "wal_index_partitions_total": metrics_snapshot.wal_index_partitions_total,
                "wal_index_files_total": metrics_snapshot.wal_index_files_total,
                "wal_index_bytes_total": metrics_snapshot.wal_index_bytes_total,
                "wal_write_bytes_total": metrics_snapshot.wal_write_bytes_total,
                "wal_index_metrics": metrics_snapshot.wal_index_metrics,
                "bytes_total": metrics_snapshot.source_bytes_total,
                "wal_write_bytes_total": metrics_snapshot.wal_write_bytes_total,
                "wal_write_bytes_current": wal_write_bytes_current,
                "wal_write_rows_total": metrics_snapshot.wal_write_rows_total,
                "wal_write_rows_current": wal_write_rows_current,
                "wal_compacted_bytes_total": metrics_snapshot.wal_compacted_bytes_total,
                "wal_compacted_bytes_current": wal_compacted_bytes_current,
                "wal_compacted_files_total": metrics_snapshot.wal_compacted_files_total,
                "wal_compacted_files_current": wal_compacted_files_current,
                "parquet_persisted_bytes_total": metrics_snapshot.parquet_persisted_bytes_total,
                "parquet_persisted_bytes_current": parquet_persisted_bytes_current,
                "parquet_persisted_rows_total": metrics_snapshot.parquet_persisted_rows_total,
                "parquet_persisted_rows_current": parquet_persisted_rows_current,
                "parquet_persisted_objects_total": metrics_snapshot.parquet_persisted_objects_total,
                "parquet_persisted_objects_current": parquet_persisted_objects_current,
                // Flush stage and resource telemetry
                "compaction_planner_cycles_total": flush_metrics.compaction_planner_cycles_total,
                "compaction_planner_duration_ns_total": flush_metrics.compaction_planner_duration_ns_total,
                "compaction_planner_segments_examined_total": flush_metrics.compaction_planner_segments_examined_total,
                "compaction_planner_slices_examined_total": flush_metrics.compaction_planner_slices_examined_total,
                "cdc_metadata_segment_scans_total": flush_metrics.cdc_metadata_segment_scans_total,
                "cdc_metadata_segment_bytes_examined_total": flush_metrics.cdc_metadata_segment_bytes_examined_total,
                "compaction_manifest_directory_scans_total": flush_metrics.compaction_manifest_directory_scans_total,
                "compaction_manifest_entries_examined_total": flush_metrics.compaction_manifest_entries_examined_total,
                "compaction_planner_ready_work_count": flush_metrics.compaction_planner_ready_work_count,
                "compaction_inflight_slice_count": flush_metrics.compaction_inflight_slice_count,
                "wal_snapshot_ready_count": flush_metrics.wal_snapshot_ready_count,
                "compaction_active_jobs": flush_metrics.compaction_active_jobs,
                "compaction_active_by_sink": counters::compaction_active_by_sink_snapshot(),
                "compaction_decode_permit_acquires_total": flush_metrics.compaction_decode_permit_acquires_total,
                "compaction_decode_permit_wait_ns_total": flush_metrics.compaction_decode_permit_wait_ns_total,
                "compaction_sink_permit_acquires_total": flush_metrics.compaction_sink_permit_acquires_total,
                "compaction_sink_permit_wait_ns_total": flush_metrics.compaction_sink_permit_wait_ns_total,
                "compaction_scheduler_top_ups_total": flush_metrics.compaction_scheduler_top_ups_total,
                "compaction_idle_slots_with_ready_work_total": flush_metrics.compaction_idle_slots_with_ready_work_total,
                "wal_writer_pending_count": crate::buffer::wal_writer::pending_count(),
                "wal_writer_pending_bytes": crate::buffer::wal_writer::pending_bytes(),
                "wal_writer_queue_capacity": crate::buffer::wal_writer::queue_capacity(),
                "wal_compactions_in_flight": counters::WAL_COMPACTIONS_IN_FLIGHT.load(Ordering::Relaxed),
                "sink_work_in_flight_count": crate::buffer::compaction_progress::sink_work_in_flight_count(),
                "grouped_stream_eager_builds_total": flush_metrics.grouped_stream_eager_builds_total,
                "grouped_stream_streaming_builds_total": flush_metrics.grouped_stream_streaming_builds_total,
                "grouped_stream_build_duration_ns_total": flush_metrics.grouped_stream_build_duration_ns_total,
                "runtime_sink_pool_acquires_total": flush_metrics.runtime_sink_pool_acquires_total,
                "runtime_sink_pool_acquire_wait_ns_total": flush_metrics.runtime_sink_pool_acquire_wait_ns_total,
                "runtime_sink_pool_waiter_count": flush_metrics.runtime_sink_pool_waiter_count,
                "runtime_sink_ipc_bytes_total": flush_metrics.runtime_sink_ipc_bytes_total,
                "runtime_sink_ipc_chunks_total": flush_metrics.runtime_sink_ipc_chunks_total,
                "runtime_schema_state_installs_sent_total": flush_metrics.runtime_schema_state_installs_sent_total,
                "runtime_schema_state_publications_skipped_total": flush_metrics.runtime_schema_state_publications_skipped_total,
                "sink_apply_calls_total": flush_metrics.sink_apply_calls_total,
                "sink_apply_duration_ns_total": flush_metrics.sink_apply_duration_ns_total,
                "compaction_completion_ledger_writes_total": flush_metrics.compaction_completion_ledger_writes_total,
                "compaction_tombstone_writes_total": flush_metrics.compaction_tombstone_writes_total,
                "wal_segment_closures_total": flush_metrics.wal_segment_closures_total,
                "wal_segment_closure_latency_ns_total": flush_metrics.wal_segment_closure_latency_ns_total,
                // Upload telemetry
                "uploads_total": crate::metrics::counters::UPLOADS_TOTAL.load(Ordering::SeqCst),
                "uploads_in_flight": crate::metrics::counters::UPLOADS_IN_FLIGHT.load(Ordering::SeqCst),
                "upload_latency_ns_total": crate::metrics::counters::UPLOAD_LATENCY_NS_TOTAL.load(Ordering::SeqCst),
                // Tuning targets and runtime state
                "upload_concurrency_target": crate::metrics::counters::UPLOAD_CONCURRENCY_TARGET.load(Ordering::SeqCst),
                "wal_compaction_concurrency_target": crate::metrics::counters::WAL_COMPACTION_CONCURRENCY_TARGET.load(Ordering::SeqCst),
                "wal_compactions_per_sink_target": crate::metrics::counters::WAL_COMPACTIONS_PER_SINK_TARGET.load(Ordering::SeqCst),
                "runtime_sink_pool_target": crate::metrics::counters::RUNTIME_SINK_POOL_TARGET.load(Ordering::SeqCst),
                "athena_glue_cp_target": crate::metrics::counters::ATHENA_GLUE_CP_TARGET.load(Ordering::SeqCst),
                "s3_download_concurrency_target": crate::metrics::counters::S3_DOWNLOAD_CONCURRENCY_TARGET.load(Ordering::SeqCst),
                "active_threads": crate::metrics::counters::ACTIVE_THREADS.load(Ordering::SeqCst),
                "queue_length": crate::metrics::counters::QUEUE_LENGTH.load(Ordering::SeqCst),
                "lock_wait_times": wait_times,
            },

            "type": "metric",
            "run_id": metrics.run_id,
            "tenant": tenant,
            "workspace_name": workspace,
            "pipeline_name": pipeline,
            "status": metrics.status.name(),
            "start_time": start_time_utc_str,
            "datetime": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "version": VERSION.unwrap_or("unknown"),
            "exit_code": exit_code
        });
        data["metrics"]["flush_budget"] = flush_budget_metrics;
        data["metrics"]["runtime_sink_active_session_count"] =
            json!(flush_metrics.runtime_sink_active_session_count);

        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
        let key = format!(
            "{}/{}/{}/metrics/{}_{}.json",
            tenant, workspace, pipeline, timestamp, metrics.run_id
        );

        let storage = crate::adapters::storage::get_storage();
        match storage.put_json(&key, &data).await {
            Ok(_) => {
                if exit_code.is_some() {
                    info!("Persisted metrics: {}", key);
                }
            }
            Err(e) => {
                error!("Failed to persist metrics: {}", e);
                error!("Hint: ensure AWS region is set (AWS_REGION or AWS_DEFAULT_REGION) and metrics bucket is configured.");
            }
        }

        Ok(())
    }

    pub async fn send_config() -> Result<(), Box<dyn std::error::Error>> {
        let metrics: Metrics;
        {
            metrics = METRICS.read().clone();
        }

        let workspace = Config::get_workspace_name();
        let pipeline = Config::get_pipeline_name();

        let tenant = Config::get_tenant();

        let current_time = chrono::Utc::now();
        let run_time_seconds = (current_time - metrics.start_time).num_seconds();

        let start_time_utc_str = DateTime::<chrono::Utc>::from(SystemTime::now())
            .sub(chrono::Duration::seconds(run_time_seconds as i64))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        let metrics_env_config = MetricsEnvConfig::new();

        let data = json!({
            "config": metrics_env_config,
            "type": "config",
            "run_id": metrics.run_id,
            "tenant": tenant,
            "workspace_name": workspace,
            "pipeline_name": pipeline,
            "status": metrics.status.name(),
            "start_time": start_time_utc_str,
            "datetime": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "version": VERSION.unwrap_or("unknown"),
        });

        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
        let key = format!(
            "{}/{}/{}/config/{}_{}.json",
            tenant, workspace, pipeline, timestamp, metrics.run_id
        );

        let storage = crate::adapters::storage::get_storage();
        match storage.put_json(&key, &data).await {
            Ok(_) => info!("Persisted config: {}", key),
            Err(e) => error!("Failed to persist config: {}", e),
        }

        Ok(())
    }

    pub fn init_send_loop() {
        let now = Arc::new(TimedRwLock::new("now".to_string(), Instant::now()));

        let now_clone = now.clone();

        // get curent tokio runtime
        let handle = runtime::Handle::current();
        let handle_clone = handle.clone();

        let mut planner = periodic::Planner::new();

        planner.add(
            move || {
                if RUNNING.read().load(Ordering::SeqCst) {

                    let now_lock = now_clone.read();
                    // Use atomic counters for totals and compute per-minute using a separate atomic
                    let messages_total_counter = crate::metrics::counters::MESSAGES_TOTAL.load(Ordering::Relaxed);
                    let source_bytes_total_counter = crate::metrics::counters::SOURCE_BYTES_TOTAL.load(Ordering::Relaxed);
                    let deadletters_total_counter = crate::metrics::counters::DEADLETTERS_TOTAL.load(Ordering::Relaxed);
                    let ingested_slow_total_counter = crate::metrics::counters::INGESTED_SLOW_TOTAL.load(Ordering::Relaxed);
                    let last_print = LAST_PRINT_MESSAGES_TOTAL.swap(messages_total_counter, Ordering::SeqCst);
                    let ingested_current = messages_total_counter.saturating_sub(last_print);
                    // Compute per-minute for fixed and deadletters using dedicated atomics
                    static LAST_PRINT_FIXED_TOTAL: AtomicU64 = AtomicU64::new(0);
                    static LAST_PRINT_DEAD_TOTAL: AtomicU64 = AtomicU64::new(0);
                    let last_fixed = LAST_PRINT_FIXED_TOTAL.swap(ingested_slow_total_counter, Ordering::SeqCst);
                    let fixed_current = ingested_slow_total_counter.saturating_sub(last_fixed);
                    let last_dead = LAST_PRINT_DEAD_TOTAL.swap(deadletters_total_counter, Ordering::SeqCst);
                    let dead_current = deadletters_total_counter.saturating_sub(last_dead);

                    let wal_pending_count = crate::buffer::wal_writer::pending_count();
                    let wal_pending_bytes = crate::buffer::wal_writer::pending_bytes();
                    let wal_queue_cap = crate::buffer::wal_writer::queue_capacity();
                    let wal_inflight = crate::metrics::counters::WAL_COMPACTIONS_IN_FLIGHT.load(Ordering::SeqCst);
                    let wal_started_total = crate::metrics::counters::WAL_COMPACTIONS_STARTED.load(Ordering::SeqCst);
                    let wal_completed_total = crate::metrics::counters::WAL_COMPACTIONS_COMPLETED.load(Ordering::SeqCst);
                    let wal_txn_started_total = crate::metrics::counters::WAL_COMPACTION_TRANSACTIONS_STARTED.load(Ordering::SeqCst);
                    let wal_txn_completed_total = crate::metrics::counters::WAL_COMPACTION_TRANSACTIONS_COMPLETED.load(Ordering::SeqCst);
                    let wal_refs_tombstoned_total = crate::metrics::counters::WAL_COMPACTION_REFS_TOMBSTONED.load(Ordering::SeqCst);
                    let wal_started_min = wal_started_total.saturating_sub(
                        LAST_PRINT_WAL_COMPACTIONS_STARTED.swap(wal_started_total, Ordering::SeqCst),
                    );
                    let wal_completed_min = wal_completed_total.saturating_sub(
                        LAST_PRINT_WAL_COMPACTIONS_COMPLETED.swap(wal_completed_total, Ordering::SeqCst),
                    );
                    static LAST_PRINT_WAL_TXN_STARTED: once_cell::sync::Lazy<std::sync::atomic::AtomicU64> =
                        once_cell::sync::Lazy::new(|| std::sync::atomic::AtomicU64::new(0));
                    static LAST_PRINT_WAL_TXN_COMPLETED: once_cell::sync::Lazy<std::sync::atomic::AtomicU64> =
                        once_cell::sync::Lazy::new(|| std::sync::atomic::AtomicU64::new(0));
                    static LAST_PRINT_WAL_REFS_TOMBSTONED: once_cell::sync::Lazy<std::sync::atomic::AtomicU64> =
                        once_cell::sync::Lazy::new(|| std::sync::atomic::AtomicU64::new(0));
                    let wal_txn_started_min = wal_txn_started_total.saturating_sub(
                        LAST_PRINT_WAL_TXN_STARTED.swap(wal_txn_started_total, Ordering::SeqCst),
                    );
                    let wal_txn_completed_min = wal_txn_completed_total.saturating_sub(
                        LAST_PRINT_WAL_TXN_COMPLETED.swap(wal_txn_completed_total, Ordering::SeqCst),
                    );
                    let wal_refs_tombstoned_min = wal_refs_tombstoned_total.saturating_sub(
                        LAST_PRINT_WAL_REFS_TOMBSTONED.swap(wal_refs_tombstoned_total, Ordering::SeqCst),
                    );
                    let reclaimable_partitions = crate::buffer::ingest_buffer::Buffers::reclaimable_wal_partition_count(10_000);
                    let pressure = crate::buffer::ingest_buffer::Buffers::wal_pressure_snapshot();
                    let data_dir_paused = crate::data_dir_ingest_paused();
                    let should_print_wal_digest = wal_pending_count > 0
                        || wal_pending_bytes > 0
                        || wal_inflight > 0
                        || wal_started_min > 0
                        || wal_completed_min > 0
                        || reclaimable_partitions > 0
                        || data_dir_paused;

                    if ingested_current > 0 {
                        info!("Messages per Min: {}", ingested_current);
                        info!("Messages Fixed per Min: {}", fixed_current);
                        // println!("Bytes per Min: {}", metrics.bytes_current);

                        let human_bytes = Helpers::human_readable_size(source_bytes_total_counter);

                        info!("Bytes Total: {}", human_bytes);
                        info!("Messages Total: {}", messages_total_counter);
                        info!("Deadletters per Min: {}", dead_current);
                        info!("Deadletter Total: {}", deadletters_total_counter);
                        info!("Runtime: {} seconds", now_lock.elapsed().as_secs());

                        // Upload/throughput summary
                        let up_total = crate::metrics::counters::UPLOADS_TOTAL.load(Ordering::SeqCst);
                        let up_inflight = crate::metrics::counters::UPLOADS_IN_FLIGHT.load(Ordering::SeqCst);
                        let up_lat_ns_total = crate::metrics::counters::UPLOAD_LATENCY_NS_TOTAL.load(Ordering::SeqCst);
                        let avg_up_ms = if up_total > 0 { (up_lat_ns_total / up_total) as f64 / 1_000_000.0 } else { 0.0 };
                        let up_target = crate::metrics::counters::UPLOAD_CONCURRENCY_TARGET.load(Ordering::SeqCst);
                        let wal_target = crate::metrics::counters::WAL_COMPACTION_CONCURRENCY_TARGET.load(Ordering::SeqCst);
                        let dl_target = crate::metrics::counters::S3_DOWNLOAD_CONCURRENCY_TARGET.load(Ordering::SeqCst);
                        let active = crate::metrics::counters::ACTIVE_THREADS.load(Ordering::SeqCst);
                        let queue = crate::metrics::counters::QUEUE_LENGTH.load(Ordering::SeqCst);

                        info!("Uploads total: {}, inflight: {}, avg latency: {:.2} ms", up_total, up_inflight, avg_up_ms);
                        info!("Targets - upload: {}, wal: {}, s3_download: {} | active: {}, queue: {}", up_target, wal_target, dl_target, active, queue);

                        // let total_times: Vec<(String, Duration)> = TimedRwLock::<()>::get_total_wait_times();
                        // for (key, value) in total_times.iter() {
                        //     println!("{}: {}ms", key, value.as_millis());
                        // }
                    }

                    if ingested_current > 0 || should_print_wal_digest {
                        let wal_target = crate::metrics::counters::WAL_COMPACTION_CONCURRENCY_TARGET.load(Ordering::SeqCst);
                        info!(
                            "WAL writer: queue={}/{}, pending={}/{}, coalesce_avg={}, persist_avg={:.2}ms, ack_avg={:.2}ms",
                            wal_pending_count,
                            wal_queue_cap,
                            wal_pending_count,
                            Helpers::human_readable_size(wal_pending_bytes as u64),
                            Helpers::human_readable_size(crate::buffer::wal_writer::coalesce_avg_bytes()),
                            crate::buffer::wal_writer::persist_avg_ms(),
                            crate::buffer::wal_writer::ack_avg_ms(),
                        );
                        let flush = crate::metrics::counters::flush_metrics_snapshot();
                        let grouped_builds = flush
                            .grouped_stream_eager_builds_total
                            .saturating_add(flush.grouped_stream_streaming_builds_total);
                        let active_by_sink =
                            crate::metrics::counters::compaction_active_by_sink_snapshot();
                        info!(
                            "Flush stages: planner={} avg={:.2}ms segments={} slices={} cdc_scans={} cdc_bytes={} manifest_scans={} ready={} inflight_slices={} active_jobs={} active_by_sink={:?} topups={} idle_ready_slots={} decode_wait_avg={:.2}ms sink_permit_wait_avg={:.2}ms wal_snapshots={} grouped=eager:{}/streaming:{} avg={:.2}ms pool_waiters={} pool_wait_avg={:.2}ms ipc={}/{}chunks schema_installs={} schema_publications_skipped={} sink_applies={} sink_avg={:.2}ms ledger_writes={} tombstones={} closures={} closure_avg={:.2}ms",
                            flush.compaction_planner_cycles_total,
                            average_duration_ms(
                                flush.compaction_planner_duration_ns_total,
                                flush.compaction_planner_cycles_total,
                            ),
                            flush.compaction_planner_segments_examined_total,
                            flush.compaction_planner_slices_examined_total,
                            flush.cdc_metadata_segment_scans_total,
                            Helpers::human_readable_size(
                                flush.cdc_metadata_segment_bytes_examined_total,
                            ),
                            flush.compaction_manifest_directory_scans_total,
                            flush.compaction_planner_ready_work_count,
                            flush.compaction_inflight_slice_count,
                            flush.compaction_active_jobs,
                            active_by_sink,
                            flush.compaction_scheduler_top_ups_total,
                            flush.compaction_idle_slots_with_ready_work_total,
                            average_duration_ms(
                                flush.compaction_decode_permit_wait_ns_total,
                                flush.compaction_decode_permit_acquires_total,
                            ),
                            average_duration_ms(
                                flush.compaction_sink_permit_wait_ns_total,
                                flush.compaction_sink_permit_acquires_total,
                            ),
                            flush.wal_snapshot_ready_count,
                            flush.grouped_stream_eager_builds_total,
                            flush.grouped_stream_streaming_builds_total,
                            average_duration_ms(
                                flush.grouped_stream_build_duration_ns_total,
                                grouped_builds,
                            ),
                            flush.runtime_sink_pool_waiter_count,
                            average_duration_ms(
                                flush.runtime_sink_pool_acquire_wait_ns_total,
                                flush.runtime_sink_pool_acquires_total,
                            ),
                            Helpers::human_readable_size(flush.runtime_sink_ipc_bytes_total),
                            flush.runtime_sink_ipc_chunks_total,
                            flush.runtime_schema_state_installs_sent_total,
                            flush.runtime_schema_state_publications_skipped_total,
                            flush.sink_apply_calls_total,
                            average_duration_ms(
                                flush.sink_apply_duration_ns_total,
                                flush.sink_apply_calls_total,
                            ),
                            flush.compaction_completion_ledger_writes_total,
                            flush.compaction_tombstone_writes_total,
                            flush.wal_segment_closures_total,
                            average_duration_ms(
                                flush.wal_segment_closure_latency_ns_total,
                                flush.wal_segment_closures_total,
                            ),
                        );
                        if wal_inflight > 0 {
                            info!(
                                "WAL compaction: target={}, inflight={}, started/min={}, completed/min={}, txn_started/min={}, txn_completed/min={}, refs_tombstoned/min={}, pipeline={}, committed_segments={}, indexed_refs={}, schedulable_refs={}, sink_work_inflight={}, paused={}, active_grouped=[{}]",
                                wal_target,
                                wal_inflight,
                                wal_started_min,
                                wal_completed_min,
                                wal_txn_started_min,
                                wal_txn_completed_min,
                                wal_refs_tombstoned_min,
                                pressure.pipeline,
                                pressure.committed_segments,
                                pressure.indexed_refs,
                                pressure.schedulable_refs,
                                pressure.sink_work_in_flight,
                                data_dir_paused,
                                crate::buffer::compaction_progress::format_in_flight_grouped_compactions(),
                            );
                        } else {
                            info!(
                                "WAL compaction: target={}, inflight={}, started/min={}, completed/min={}, txn_started/min={}, txn_completed/min={}, refs_tombstoned/min={}, pipeline={}, committed_segments={}, indexed_refs={}, schedulable_refs={}, sink_work_inflight={}, paused={}",
                                wal_target,
                                wal_inflight,
                                wal_started_min,
                                wal_completed_min,
                                wal_txn_started_min,
                                wal_txn_completed_min,
                                wal_refs_tombstoned_min,
                                pressure.pipeline,
                                pressure.committed_segments,
                                pressure.indexed_refs,
                                pressure.schedulable_refs,
                                pressure.sink_work_in_flight,
                                data_dir_paused,
                            );
                        }
                        if let Some((free_bytes, used_pct)) = data_dir_disk_usage() {
                            let pause_progress = if data_dir_paused {
                                Some(crate::buffer::ingest_buffer::Buffers::pause_progress_snapshot())
                            } else {
                                None
                            };
                            if let Some(progress) = pause_progress {
                                info!(
                                    "DATA_DIR: paused={}, usage={:.1}%, free={}, pipeline={}, committed_segments={}, indexed_refs={}, schedulable_refs={}, sink_work_inflight={}, wal_compactions_inflight={}, segments_deleted={}, bytes_reclaimed={}, last_progress_age_secs={:?}",
                                    data_dir_paused,
                                    used_pct,
                                    Helpers::human_readable_size(free_bytes),
                                    progress.pipeline,
                                    progress.committed_segments,
                                    progress.indexed_refs,
                                    progress.schedulable_refs,
                                    progress.sink_work_in_flight,
                                    progress.wal_compactions_in_flight,
                                    progress.segments_deleted_cumulative,
                                    Helpers::human_readable_size(progress.bytes_reclaimed_cumulative),
                                    progress.last_progress_age_secs,
                                );
                            } else {
                                info!(
                                    "DATA_DIR: paused={}, usage={:.1}%, free={}",
                                    data_dir_paused,
                                    used_pct,
                                    Helpers::human_readable_size(free_bytes),
                                );
                            }
                        } else {
                            info!("DATA_DIR: paused={}, usage=n/a, free=n/a", data_dir_paused);
                        }
                    }

                    handle_clone.spawn(async move {
                        match Metrics::send_metrics(None).await {
                            Ok(_g) => {}
                            Err(_err) => {}
                        }
                    });

                }
            },
            periodic::Every::new(Duration::from_secs(60)),
        );
        planner.start();
    }
}
