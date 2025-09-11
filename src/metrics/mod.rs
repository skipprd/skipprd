use std::fmt::Debug;
use std::ops::Sub;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime};
use chrono::{DateTime};
use core::time::Duration;
use std::collections::HashMap;
use std::sync::Arc;
 
use serde_derive::{Deserialize, Serialize};
use serde_json::json;
use tokio::runtime;
use crate::buffer::ingest_buffer::WalIndexMetrics;

use crate::helpers::configuration::Config;
use crate::helpers::Helpers;
use crate::helpers::s3;
use crate::helpers::timed_rwlock::TimedRwLock;
use crate::{METRICS, RUNNING};

pub mod counters;

pub static LAST_MESSAGES_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static LAST_FIXED_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static LAST_DEADLETTERS_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static LAST_SOURCE_BYTES_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static LAST_WAL_WRITE_BYTES_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static LAST_WAL_WRITE_ROWS_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static LAST_WAL_COMPACTED_BYTES_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static LAST_WAL_COMPACTED_FILES_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static LAST_PARQUET_PERSISTED_BYTES_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static LAST_PARQUET_PERSISTED_ROWS_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static LAST_PARQUET_PERSISTED_OBJECTS_TOTAL: AtomicU64 = AtomicU64::new(0);
// Printer-only delta state (separate from upload deltas)
pub static LAST_PRINT_MESSAGES_TOTAL: AtomicU64 = AtomicU64::new(0);


const VERSION: Option<&str> = option_env!("CARGO_PKG_VERSION");

#[derive(Debug, Deserialize, Serialize)]
struct MetricsEnvConfig {
    data_source_plugin_name: String,
    data_output_plugin_name: String,
    schema_output_plugin_name: String,
    data_deadletter_plugin_name: String,
    data_source_batch_size_bytes: i64,
    data_source_batch_size_seconds: i64,
    buffer_threshold_bytes: u64,
    buffer_threshold_seconds: u64,
    transform_namespace_fields: String,
    transform_batch_partition_fields: String,
    transform_flatten_events: String,
    transform_batch_time_fields: String,
    transform_batch_time_units: String,
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
            data_deadletter_plugin_name: Config::get_pipeline_deadletter_plugin_name(),
            data_source_batch_size_bytes: match Config::get_pipline_plugin_config("input") {
                Ok(config) => config.batch_size_bytes().or(Some(0)).unwrap(),
                Err(_) => 0
            },
            data_source_batch_size_seconds: match Config::get_pipline_plugin_config("input") {
                Ok(config) => config.batch_size_seconds().or(Some(0)).unwrap(),
                Err(_) => 0
            },
            buffer_threshold_bytes: Config::get_pipeline_buffer_threshold_bytes() as u64,
            buffer_threshold_seconds: Config::get_pipeline_buffer_threshold_seconds() as u64,
            transform_namespace_fields: Config::get_transform_namespace_fields(),
            transform_batch_partition_fields: Config::get_transform_batch_partition_fields(),
            transform_flatten_events: Config::get_transform_flatten_events().to_string(),
            transform_batch_time_fields: Config::get_transform_batch_time_fields(),
            transform_batch_time_units: Config::get_transform_batch_time_unit(),
            data_dir: Config::get_pipeline_data_dir(),
            chaos_mode: Config::get_pipeline_chaos_mode().to_string(),
            input_format: match Config::get_pipline_plugin_config("input") {
                Ok(config) => config.format().to_string(),
                Err(_) => String::from("")
            },
            output_format: match Config::get_pipline_plugin_config("output") {
                Ok(config) => config.format().to_string(),
                Err(_) => String::from("")
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
    Unknown
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

    pub(crate) fn reset(&mut self) {
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

    pub(crate) async fn send_metrics<'a>(
        exit_code: Option<i8>,
    ) -> Result<(), Box<dyn std::error::Error>> {

        let metrics: Metrics;
        {
            metrics = METRICS.read().clone();
        }

        let workspace = Config::get_workspace_name();
        let pipeline = Config::get_pipeline_name();

        let tenant = Config::get_tenant();

        let wal_write_bytes_total = LAST_WAL_WRITE_BYTES_TOTAL.load(Ordering::SeqCst);
        let wal_write_bytes_current = metrics.wal_write_bytes_total - wal_write_bytes_total;
        LAST_WAL_WRITE_BYTES_TOTAL.store(metrics.wal_write_bytes_total, Ordering::SeqCst);

        let wal_write_rows_total = LAST_WAL_WRITE_ROWS_TOTAL.load(Ordering::SeqCst);
        let wal_write_rows_current = metrics.wal_write_rows_total - wal_write_rows_total;
        LAST_WAL_WRITE_ROWS_TOTAL.store(metrics.wal_write_rows_total, Ordering::SeqCst);

        let wal_compacted_bytes_total = LAST_WAL_COMPACTED_BYTES_TOTAL.load(Ordering::SeqCst);
        let wal_compacted_bytes_current = metrics.wal_compacted_bytes_total - wal_compacted_bytes_total;
        LAST_WAL_COMPACTED_BYTES_TOTAL.store(metrics.wal_compacted_bytes_total, Ordering::SeqCst);

        let wal_compacted_files_total = LAST_WAL_COMPACTED_FILES_TOTAL.load(Ordering::SeqCst);
        let wal_compacted_files_current = metrics.wal_compacted_files_total - wal_compacted_files_total;
        LAST_WAL_COMPACTED_FILES_TOTAL.store(metrics.wal_compacted_files_total, Ordering::SeqCst);

        // Merge counters (atomics) into snapshot before computing deltas
        use crate::metrics::counters as counters;
        let messages_total_counter = counters::MESSAGES_TOTAL.load(Ordering::Relaxed);
        let deadletters_total_counter = counters::DEADLETTERS_TOTAL.load(Ordering::Relaxed);
        let ingested_slow_total_counter = counters::INGESTED_SLOW_TOTAL.load(Ordering::Relaxed);
        let source_bytes_total_counter = counters::SOURCE_BYTES_TOTAL.load(Ordering::Relaxed);
        let wal_write_bytes_total_counter = counters::WAL_WRITE_BYTES_TOTAL.load(Ordering::Relaxed);
        let wal_write_rows_total_counter = counters::WAL_WRITE_ROWS_TOTAL.load(Ordering::Relaxed);
        let wal_compacted_bytes_total_counter = counters::WAL_COMPACTED_BYTES_TOTAL.load(Ordering::Relaxed);
        let wal_compacted_files_total_counter = counters::WAL_COMPACTED_FILES_TOTAL.load(Ordering::Relaxed);
        let wal_compacted_rows_total_counter = counters::WAL_COMPACTED_ROWS_TOTAL.load(Ordering::Relaxed);
        let parquet_persisted_bytes_total_counter = counters::PARQUET_PERSISTED_BYTES_TOTAL.load(Ordering::Relaxed);
        let parquet_persisted_rows_total_counter = counters::PARQUET_PERSISTED_ROWS_TOTAL.load(Ordering::Relaxed);
        let parquet_persisted_objects_total_counter = counters::PARQUET_PERSISTED_OBJECTS_TOTAL.load(Ordering::Relaxed);

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

        // Now compute parquet deltas from merged snapshot
        let parquet_persisted_bytes_total = LAST_PARQUET_PERSISTED_BYTES_TOTAL.load(Ordering::SeqCst);
        let parquet_persisted_bytes_current = metrics_snapshot.parquet_persisted_bytes_total - parquet_persisted_bytes_total;
        LAST_PARQUET_PERSISTED_BYTES_TOTAL.store(metrics_snapshot.parquet_persisted_bytes_total, Ordering::SeqCst);

        let parquet_persisted_rows_total = LAST_PARQUET_PERSISTED_ROWS_TOTAL.load(Ordering::SeqCst);
        let parquet_persisted_rows_current = metrics_snapshot.parquet_persisted_rows_total - parquet_persisted_rows_total;
        LAST_PARQUET_PERSISTED_ROWS_TOTAL.store(metrics_snapshot.parquet_persisted_rows_total, Ordering::SeqCst);

        let parquet_persisted_objects_total = LAST_PARQUET_PERSISTED_OBJECTS_TOTAL.load(Ordering::SeqCst);
        let parquet_persisted_objects_current = metrics_snapshot.parquet_persisted_objects_total - parquet_persisted_objects_total;
        LAST_PARQUET_PERSISTED_OBJECTS_TOTAL.store(metrics_snapshot.parquet_persisted_objects_total, Ordering::SeqCst);

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
            crate::metrics::counters::LATEST_TIMESTAMP.load(Ordering::Relaxed)
        );

        let start_time_utc_str = metrics_snapshot.start_time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        // get runtime in seconds from metrics.start_time
        let current_time = chrono::Utc::now();
        let run_time_seconds = (current_time - metrics_snapshot.start_time).num_seconds();

        let total_times: Vec<(String, Duration)> = TimedRwLock::<()>::get_total_wait_times();
        let wait_times: HashMap<String, Duration> = total_times.iter().cloned().collect();

        let data = json!({
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
                // Upload telemetry
                "uploads_total": crate::metrics::counters::UPLOADS_TOTAL.load(Ordering::SeqCst),
                "uploads_in_flight": crate::metrics::counters::UPLOADS_IN_FLIGHT.load(Ordering::SeqCst),
                "upload_latency_ns_total": crate::metrics::counters::UPLOAD_LATENCY_NS_TOTAL.load(Ordering::SeqCst),
                // Tuning targets and runtime state
                "upload_concurrency_target": crate::metrics::counters::UPLOAD_CONCURRENCY_TARGET.load(Ordering::SeqCst),
                "wal_compaction_concurrency_target": crate::metrics::counters::WAL_COMPACTION_CONCURRENCY_TARGET.load(Ordering::SeqCst),
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

        // Upload metrics to S3
        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
        let s3_key = format!("{}/{}/{}/metrics/{}_{}.json", tenant, workspace, pipeline, timestamp, metrics.run_id);

        match s3::put_json(&s3_key, &data).await {
            Ok(_) => {
                // Only print on exit or error to reduce noise
                if exit_code.is_some() {
                    println!("Uploaded metrics to S3: {}", s3_key);
                }
            }
            Err(err) => {
                println!("Failed to upload metrics to S3: {:?}", err);
                println!("Hint: ensure AWS region is set (AWS_REGION or AWS_DEFAULT_REGION) and metrics bucket is configured.");
            }
        }

        Ok(())
    }

    pub(crate) async fn send_config() -> Result<(), Box<dyn std::error::Error>> {

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

        // Upload config to S3
        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
        let s3_key = format!("{}/{}/{}/config/{}_{}.json", tenant, workspace, pipeline, timestamp, metrics.run_id);

        match s3::put_json(&s3_key, &data).await {
            Ok(_) => {
                println!("Uploaded config to S3: {}", s3_key);
            }
            Err(err) => {
                println!("Failed to upload config to S3: {:?}", err);
            }
        }

        Ok(())
    }

    pub(crate) fn init_send_loop() {

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

                    if ingested_current > 0 {
                        println!("Messages per Min: {}", ingested_current);
                        println!("Messages Fixed per Min: {}", fixed_current);
                        // println!("Bytes per Min: {}", metrics.bytes_current);

                        let human_bytes = Helpers::human_readable_size(source_bytes_total_counter);

                        println!("Bytes Total: {}", human_bytes);
                        println!("Messages Total: {}", messages_total_counter);
                        println!("Deadletters per Min: {}", dead_current);
                        println!("Deadletter Total: {}", deadletters_total_counter);
                        println!("Runtime: {} seconds", now_lock.elapsed().as_secs());

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

                        println!("Uploads total: {}, inflight: {}, avg latency: {:.2} ms", up_total, up_inflight, avg_up_ms);
                        println!("Targets - upload: {}, wal: {}, s3_download: {} | active: {}, queue: {}", up_target, wal_target, dl_target, active, queue);

                        // let total_times: Vec<(String, Duration)> = TimedRwLock::<()>::get_total_wait_times();
                        // for (key, value) in total_times.iter() {
                        //     println!("{}: {}ms", key, value.as_millis());
                        // }
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