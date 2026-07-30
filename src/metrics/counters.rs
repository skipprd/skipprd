#![allow(dead_code)]
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

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
/// Recommended number of multipart parts in flight inside one object write.
pub static MULTIPART_PART_CONCURRENCY_TARGET: Lazy<std::sync::atomic::AtomicUsize> =
    Lazy::new(|| std::sync::atomic::AtomicUsize::new(2));
pub static WAL_COMPACTION_CONCURRENCY_TARGET: Lazy<std::sync::atomic::AtomicUsize> =
    Lazy::new(|| std::sync::atomic::AtomicUsize::new(16));
pub static S3_DOWNLOAD_CONCURRENCY_TARGET: Lazy<std::sync::atomic::AtomicUsize> =
    Lazy::new(|| std::sync::atomic::AtomicUsize::new(256));
/// Max concurrent grouped compactions per sink_ref (auto-tuned; env WAL_COMPACTIONS_PER_SINK overrides).
pub static WAL_COMPACTIONS_PER_SINK_TARGET: Lazy<std::sync::atomic::AtomicUsize> =
    Lazy::new(|| std::sync::atomic::AtomicUsize::new(1));
/// Global runtime sink session-demand target.
/// Child count is derived from this target and each adapter's per-child capacity;
/// `RUNTIME_SINK_CONNECTION_POOL_SIZE` remains only a hard process cap.
pub static RUNTIME_SINK_POOL_TARGET: Lazy<std::sync::atomic::AtomicUsize> =
    Lazy::new(|| std::sync::atomic::AtomicUsize::new(1));
/// Athena Glue control-plane concurrency (auto-tuned in plugin; env ATHENA_GLUE_CONTROL_PLANE_CONCURRENCY overrides).
pub static ATHENA_GLUE_CP_TARGET: Lazy<std::sync::atomic::AtomicUsize> =
    Lazy::new(|| std::sync::atomic::AtomicUsize::new(2));
/// Compatibility metrics for the authoritative `FlushBudgetSnapshot`.
pub static FLUSH_BUDGET_GENERATION: Lazy<AtomicUsize> = Lazy::new(|| AtomicUsize::new(0));
pub static FLUSH_BUDGET_REASON_CODE: Lazy<AtomicUsize> = Lazy::new(|| AtomicUsize::new(0));
pub static FLUSH_BUDGET_INGEST_RESERVED_CORES: Lazy<AtomicUsize> =
    Lazy::new(|| AtomicUsize::new(1));

// WAL S3 error telemetry
pub static S3_WAL_RETRIES_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static S3_WAL_ERRORS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
// Exponential moving average of retries per tick, scaled by 100 (for decimals)
pub static S3_WAL_RETRY_EMA_X100: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));

// Glue/Athena control-plane retry telemetry (plugin-local when using runtime sinks)
pub static GLUE_RETRIES_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static GLUE_RETRY_EMA_X100: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static CATALOG_OUTBOX_PENDING: Lazy<AtomicUsize> = Lazy::new(|| AtomicUsize::new(0));
pub static CATALOG_OUTBOX_OLDEST_AGE_SECS: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static CATALOG_OUTBOX_RETRIES_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static CATALOG_OUTBOX_TERMINAL_FAILURES: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static CATALOG_OUTBOX_SUCCESSFUL_BATCHES: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static CATALOG_OUTBOX_LOCATION_UPDATES: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static DATA_DURABLE_CATALOG_PENDING: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));

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

// Flush-pipeline telemetry. Timings are cumulative nanoseconds so hot paths only perform
// relaxed atomic operations; rates and averages are derived by the metrics consumer.
pub static COMPACTION_PLANNER_CYCLES_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static COMPACTION_PLANNER_DURATION_NS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static COMPACTION_PLANNER_SEGMENTS_EXAMINED_TOTAL: Lazy<AtomicU64> =
    Lazy::new(|| AtomicU64::new(0));
pub static COMPACTION_PLANNER_SLICES_EXAMINED_TOTAL: Lazy<AtomicU64> =
    Lazy::new(|| AtomicU64::new(0));
pub static CDC_METADATA_SEGMENT_SCANS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static CDC_METADATA_SEGMENT_BYTES_EXAMINED_TOTAL: Lazy<AtomicU64> =
    Lazy::new(|| AtomicU64::new(0));
pub static COMPACTION_MANIFEST_DIRECTORY_SCANS_TOTAL: Lazy<AtomicU64> =
    Lazy::new(|| AtomicU64::new(0));
pub static COMPACTION_MANIFEST_ENTRIES_EXAMINED_TOTAL: Lazy<AtomicU64> =
    Lazy::new(|| AtomicU64::new(0));
/// Current number of compaction groups with at least one eligible queued slice.
pub static COMPACTION_PLANNER_READY_WORK_COUNT: Lazy<AtomicUsize> =
    Lazy::new(|| AtomicUsize::new(0));
pub static COMPACTION_INFLIGHT_SLICE_COUNT: Lazy<AtomicUsize> = Lazy::new(|| AtomicUsize::new(0));
pub static WAL_SNAPSHOT_READY_COUNT: Lazy<AtomicUsize> = Lazy::new(|| AtomicUsize::new(0));
pub static COMPACTION_ACTIVE_JOBS: Lazy<AtomicUsize> = Lazy::new(|| AtomicUsize::new(0));
pub static COMPACTION_DECODE_PERMIT_ACQUIRES_TOTAL: Lazy<AtomicU64> =
    Lazy::new(|| AtomicU64::new(0));
pub static COMPACTION_DECODE_PERMIT_WAIT_NS_TOTAL: Lazy<AtomicU64> =
    Lazy::new(|| AtomicU64::new(0));
pub static COMPACTION_SINK_PERMIT_ACQUIRES_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static COMPACTION_SINK_PERMIT_WAIT_NS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static COMPACTION_SCHEDULER_TOP_UPS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static COMPACTION_IDLE_SLOTS_WITH_READY_WORK_TOTAL: Lazy<AtomicU64> =
    Lazy::new(|| AtomicU64::new(0));
static COMPACTION_ACTIVE_BY_SINK: Lazy<DashMap<String, usize>> = Lazy::new(DashMap::new);
pub static GROUPED_STREAM_EAGER_BUILDS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static GROUPED_STREAM_STREAMING_BUILDS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static GROUPED_STREAM_BUILD_DURATION_NS_TOTAL: Lazy<AtomicU64> =
    Lazy::new(|| AtomicU64::new(0));
pub static RUNTIME_SINK_POOL_ACQUIRES_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static RUNTIME_SINK_POOL_ACQUIRE_WAIT_NS_TOTAL: Lazy<AtomicU64> =
    Lazy::new(|| AtomicU64::new(0));
pub static RUNTIME_SINK_POOL_WAITER_COUNT: Lazy<AtomicUsize> = Lazy::new(|| AtomicUsize::new(0));
pub static RUNTIME_SINK_ACTIVE_SESSION_COUNT: Lazy<AtomicUsize> = Lazy::new(|| AtomicUsize::new(0));
pub static RUNTIME_SINK_IPC_BYTES_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static RUNTIME_SINK_IPC_CHUNKS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static RUNTIME_SCHEMA_STATE_INSTALLS_SENT_TOTAL: Lazy<AtomicU64> =
    Lazy::new(|| AtomicU64::new(0));
/// Schema-state frames suppressed because the target worker already has an equal or newer version,
/// plus source schema publications disabled by configuration.
pub static RUNTIME_SCHEMA_STATE_PUBLICATIONS_SKIPPED_TOTAL: Lazy<AtomicU64> =
    Lazy::new(|| AtomicU64::new(0));
pub static SINK_APPLY_CALLS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static SINK_APPLY_DURATION_NS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
/// The current completion ledger is the durable compaction manifest.
pub static COMPACTION_COMPLETION_LEDGER_WRITES_TOTAL: Lazy<AtomicU64> =
    Lazy::new(|| AtomicU64::new(0));
pub static COMPACTION_TOMBSTONE_WRITES_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static WAL_SEGMENT_CLOSURES_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
pub static WAL_SEGMENT_CLOSURE_LATENCY_NS_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FlushMetricsSnapshot {
    pub compaction_planner_cycles_total: u64,
    pub compaction_planner_duration_ns_total: u64,
    pub compaction_planner_segments_examined_total: u64,
    pub compaction_planner_slices_examined_total: u64,
    pub cdc_metadata_segment_scans_total: u64,
    pub cdc_metadata_segment_bytes_examined_total: u64,
    pub compaction_manifest_directory_scans_total: u64,
    pub compaction_manifest_entries_examined_total: u64,
    pub compaction_planner_ready_work_count: usize,
    pub compaction_inflight_slice_count: usize,
    pub wal_snapshot_ready_count: usize,
    pub compaction_active_jobs: usize,
    pub compaction_decode_permit_acquires_total: u64,
    pub compaction_decode_permit_wait_ns_total: u64,
    pub compaction_sink_permit_acquires_total: u64,
    pub compaction_sink_permit_wait_ns_total: u64,
    pub compaction_scheduler_top_ups_total: u64,
    pub compaction_idle_slots_with_ready_work_total: u64,
    pub grouped_stream_eager_builds_total: u64,
    pub grouped_stream_streaming_builds_total: u64,
    pub grouped_stream_build_duration_ns_total: u64,
    pub runtime_sink_pool_acquires_total: u64,
    pub runtime_sink_pool_acquire_wait_ns_total: u64,
    pub runtime_sink_pool_waiter_count: usize,
    pub runtime_sink_active_session_count: usize,
    pub runtime_sink_ipc_bytes_total: u64,
    pub runtime_sink_ipc_chunks_total: u64,
    pub runtime_schema_state_installs_sent_total: u64,
    pub runtime_schema_state_publications_skipped_total: u64,
    pub sink_apply_calls_total: u64,
    pub sink_apply_duration_ns_total: u64,
    pub compaction_completion_ledger_writes_total: u64,
    pub compaction_tombstone_writes_total: u64,
    pub wal_segment_closures_total: u64,
    pub wal_segment_closure_latency_ns_total: u64,
}

#[inline]
fn duration_ns(duration: Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

pub fn flush_metrics_snapshot() -> FlushMetricsSnapshot {
    FlushMetricsSnapshot {
        compaction_planner_cycles_total: COMPACTION_PLANNER_CYCLES_TOTAL.load(Ordering::Relaxed),
        compaction_planner_duration_ns_total: COMPACTION_PLANNER_DURATION_NS_TOTAL
            .load(Ordering::Relaxed),
        compaction_planner_segments_examined_total: COMPACTION_PLANNER_SEGMENTS_EXAMINED_TOTAL
            .load(Ordering::Relaxed),
        compaction_planner_slices_examined_total: COMPACTION_PLANNER_SLICES_EXAMINED_TOTAL
            .load(Ordering::Relaxed),
        cdc_metadata_segment_scans_total: CDC_METADATA_SEGMENT_SCANS_TOTAL.load(Ordering::Relaxed),
        cdc_metadata_segment_bytes_examined_total: CDC_METADATA_SEGMENT_BYTES_EXAMINED_TOTAL
            .load(Ordering::Relaxed),
        compaction_manifest_directory_scans_total: COMPACTION_MANIFEST_DIRECTORY_SCANS_TOTAL
            .load(Ordering::Relaxed),
        compaction_manifest_entries_examined_total: COMPACTION_MANIFEST_ENTRIES_EXAMINED_TOTAL
            .load(Ordering::Relaxed),
        compaction_planner_ready_work_count: COMPACTION_PLANNER_READY_WORK_COUNT
            .load(Ordering::Relaxed),
        compaction_inflight_slice_count: COMPACTION_INFLIGHT_SLICE_COUNT.load(Ordering::Relaxed),
        wal_snapshot_ready_count: WAL_SNAPSHOT_READY_COUNT.load(Ordering::Relaxed),
        compaction_active_jobs: COMPACTION_ACTIVE_JOBS.load(Ordering::Relaxed),
        compaction_decode_permit_acquires_total: COMPACTION_DECODE_PERMIT_ACQUIRES_TOTAL
            .load(Ordering::Relaxed),
        compaction_decode_permit_wait_ns_total: COMPACTION_DECODE_PERMIT_WAIT_NS_TOTAL
            .load(Ordering::Relaxed),
        compaction_sink_permit_acquires_total: COMPACTION_SINK_PERMIT_ACQUIRES_TOTAL
            .load(Ordering::Relaxed),
        compaction_sink_permit_wait_ns_total: COMPACTION_SINK_PERMIT_WAIT_NS_TOTAL
            .load(Ordering::Relaxed),
        compaction_scheduler_top_ups_total: COMPACTION_SCHEDULER_TOP_UPS_TOTAL
            .load(Ordering::Relaxed),
        compaction_idle_slots_with_ready_work_total: COMPACTION_IDLE_SLOTS_WITH_READY_WORK_TOTAL
            .load(Ordering::Relaxed),
        grouped_stream_eager_builds_total: GROUPED_STREAM_EAGER_BUILDS_TOTAL
            .load(Ordering::Relaxed),
        grouped_stream_streaming_builds_total: GROUPED_STREAM_STREAMING_BUILDS_TOTAL
            .load(Ordering::Relaxed),
        grouped_stream_build_duration_ns_total: GROUPED_STREAM_BUILD_DURATION_NS_TOTAL
            .load(Ordering::Relaxed),
        runtime_sink_pool_acquires_total: RUNTIME_SINK_POOL_ACQUIRES_TOTAL.load(Ordering::Relaxed),
        runtime_sink_pool_acquire_wait_ns_total: RUNTIME_SINK_POOL_ACQUIRE_WAIT_NS_TOTAL
            .load(Ordering::Relaxed),
        runtime_sink_pool_waiter_count: RUNTIME_SINK_POOL_WAITER_COUNT.load(Ordering::Relaxed),
        runtime_sink_active_session_count: RUNTIME_SINK_ACTIVE_SESSION_COUNT
            .load(Ordering::Relaxed),
        runtime_sink_ipc_bytes_total: RUNTIME_SINK_IPC_BYTES_TOTAL.load(Ordering::Relaxed),
        runtime_sink_ipc_chunks_total: RUNTIME_SINK_IPC_CHUNKS_TOTAL.load(Ordering::Relaxed),
        runtime_schema_state_installs_sent_total: RUNTIME_SCHEMA_STATE_INSTALLS_SENT_TOTAL
            .load(Ordering::Relaxed),
        runtime_schema_state_publications_skipped_total:
            RUNTIME_SCHEMA_STATE_PUBLICATIONS_SKIPPED_TOTAL.load(Ordering::Relaxed),
        sink_apply_calls_total: SINK_APPLY_CALLS_TOTAL.load(Ordering::Relaxed),
        sink_apply_duration_ns_total: SINK_APPLY_DURATION_NS_TOTAL.load(Ordering::Relaxed),
        compaction_completion_ledger_writes_total: COMPACTION_COMPLETION_LEDGER_WRITES_TOTAL
            .load(Ordering::Relaxed),
        compaction_tombstone_writes_total: COMPACTION_TOMBSTONE_WRITES_TOTAL
            .load(Ordering::Relaxed),
        wal_segment_closures_total: WAL_SEGMENT_CLOSURES_TOTAL.load(Ordering::Relaxed),
        wal_segment_closure_latency_ns_total: WAL_SEGMENT_CLOSURE_LATENCY_NS_TOTAL
            .load(Ordering::Relaxed),
    }
}

#[inline]
pub fn record_compaction_planner_cycle(duration: Duration, segments: u64, slices: u64) {
    COMPACTION_PLANNER_CYCLES_TOTAL.fetch_add(1, Ordering::Relaxed);
    COMPACTION_PLANNER_DURATION_NS_TOTAL.fetch_add(duration_ns(duration), Ordering::Relaxed);
    COMPACTION_PLANNER_SEGMENTS_EXAMINED_TOTAL.fetch_add(segments, Ordering::Relaxed);
    COMPACTION_PLANNER_SLICES_EXAMINED_TOTAL.fetch_add(slices, Ordering::Relaxed);
}

#[inline]
pub fn record_cdc_metadata_segment_scan(bytes_examined: u64) {
    CDC_METADATA_SEGMENT_SCANS_TOTAL.fetch_add(1, Ordering::Relaxed);
    CDC_METADATA_SEGMENT_BYTES_EXAMINED_TOTAL.fetch_add(bytes_examined, Ordering::Relaxed);
}

#[inline]
pub fn record_compaction_manifest_directory_scan(entries_examined: u64) {
    COMPACTION_MANIFEST_DIRECTORY_SCANS_TOTAL.fetch_add(1, Ordering::Relaxed);
    COMPACTION_MANIFEST_ENTRIES_EXAMINED_TOTAL.fetch_add(entries_examined, Ordering::Relaxed);
}

#[inline]
pub fn set_compaction_planner_ready_work_count(count: usize) {
    COMPACTION_PLANNER_READY_WORK_COUNT.store(count, Ordering::Relaxed);
}

#[inline]
pub fn set_compaction_inflight_slice_count(count: usize) {
    COMPACTION_INFLIGHT_SLICE_COUNT.store(count, Ordering::Relaxed);
}

#[inline]
pub fn set_wal_snapshot_ready_count(count: usize) {
    WAL_SNAPSHOT_READY_COUNT.store(count, Ordering::Relaxed);
}

#[inline]
pub fn inc_compaction_active_job(sink_ref: &str) {
    COMPACTION_ACTIVE_JOBS.fetch_add(1, Ordering::Relaxed);
    *COMPACTION_ACTIVE_BY_SINK
        .entry(sink_ref.to_string())
        .or_default() += 1;
}

#[inline]
pub fn dec_compaction_active_job(sink_ref: &str) {
    let _ = COMPACTION_ACTIVE_JOBS.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_sub(1))
    });
    if let Entry::Occupied(mut entry) = COMPACTION_ACTIVE_BY_SINK.entry(sink_ref.to_string()) {
        if *entry.get() <= 1 {
            entry.remove();
        } else {
            *entry.get_mut() -= 1;
        }
    }
}

pub fn compaction_active_by_sink_snapshot() -> HashMap<String, usize> {
    COMPACTION_ACTIVE_BY_SINK
        .iter()
        .map(|entry| (entry.key().clone(), *entry.value()))
        .collect()
}

#[inline]
pub fn record_compaction_decode_permit_wait(duration: Duration) {
    COMPACTION_DECODE_PERMIT_ACQUIRES_TOTAL.fetch_add(1, Ordering::Relaxed);
    COMPACTION_DECODE_PERMIT_WAIT_NS_TOTAL.fetch_add(duration_ns(duration), Ordering::Relaxed);
}

#[inline]
pub fn record_compaction_sink_permit_wait(duration: Duration) {
    COMPACTION_SINK_PERMIT_ACQUIRES_TOTAL.fetch_add(1, Ordering::Relaxed);
    COMPACTION_SINK_PERMIT_WAIT_NS_TOTAL.fetch_add(duration_ns(duration), Ordering::Relaxed);
}

#[inline]
pub fn add_compaction_scheduler_top_up() {
    COMPACTION_SCHEDULER_TOP_UPS_TOTAL.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub fn add_compaction_idle_slots_with_ready_work(slots: usize) {
    COMPACTION_IDLE_SLOTS_WITH_READY_WORK_TOTAL.fetch_add(slots as u64, Ordering::Relaxed);
}

#[inline]
pub fn record_grouped_stream_build(duration: Duration, eager: bool) {
    if eager {
        GROUPED_STREAM_EAGER_BUILDS_TOTAL.fetch_add(1, Ordering::Relaxed);
    } else {
        GROUPED_STREAM_STREAMING_BUILDS_TOTAL.fetch_add(1, Ordering::Relaxed);
    }
    GROUPED_STREAM_BUILD_DURATION_NS_TOTAL.fetch_add(duration_ns(duration), Ordering::Relaxed);
}

#[inline]
pub fn inc_runtime_sink_pool_waiters() {
    RUNTIME_SINK_POOL_WAITER_COUNT.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub fn dec_runtime_sink_pool_waiters() {
    let _ = RUNTIME_SINK_POOL_WAITER_COUNT.fetch_update(
        Ordering::Relaxed,
        Ordering::Relaxed,
        |value| Some(value.saturating_sub(1)),
    );
}

#[inline]
pub fn record_runtime_sink_pool_acquire_wait(duration: Duration) {
    RUNTIME_SINK_POOL_ACQUIRES_TOTAL.fetch_add(1, Ordering::Relaxed);
    RUNTIME_SINK_POOL_ACQUIRE_WAIT_NS_TOTAL.fetch_add(duration_ns(duration), Ordering::Relaxed);
}

#[inline]
pub fn inc_runtime_sink_active_sessions() {
    RUNTIME_SINK_ACTIVE_SESSION_COUNT.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub fn dec_runtime_sink_active_sessions() {
    let _ = RUNTIME_SINK_ACTIVE_SESSION_COUNT.fetch_update(
        Ordering::Relaxed,
        Ordering::Relaxed,
        |value| Some(value.saturating_sub(1)),
    );
}

#[inline]
pub fn record_runtime_sink_ipc(bytes: u64, chunks: u64) {
    RUNTIME_SINK_IPC_BYTES_TOTAL.fetch_add(bytes, Ordering::Relaxed);
    RUNTIME_SINK_IPC_CHUNKS_TOTAL.fetch_add(chunks, Ordering::Relaxed);
}

#[inline]
pub fn add_runtime_schema_state_install_sent(n: u64) {
    RUNTIME_SCHEMA_STATE_INSTALLS_SENT_TOTAL.fetch_add(n, Ordering::Relaxed);
}

#[inline]
pub fn add_runtime_schema_state_publication_skipped(n: u64) {
    RUNTIME_SCHEMA_STATE_PUBLICATIONS_SKIPPED_TOTAL.fetch_add(n, Ordering::Relaxed);
}

#[inline]
pub fn record_sink_apply(duration: Duration) {
    SINK_APPLY_CALLS_TOTAL.fetch_add(1, Ordering::Relaxed);
    SINK_APPLY_DURATION_NS_TOTAL.fetch_add(duration_ns(duration), Ordering::Relaxed);
}

#[inline]
pub fn add_compaction_completion_ledger_write(n: u64) {
    COMPACTION_COMPLETION_LEDGER_WRITES_TOTAL.fetch_add(n, Ordering::Relaxed);
}

#[inline]
pub fn add_compaction_tombstone_write(n: u64) {
    COMPACTION_TOMBSTONE_WRITES_TOTAL.fetch_add(n, Ordering::Relaxed);
}

#[inline]
pub fn record_wal_segment_closure(duration: Duration) {
    WAL_SEGMENT_CLOSURES_TOTAL.fetch_add(1, Ordering::Relaxed);
    WAL_SEGMENT_CLOSURE_LATENCY_NS_TOTAL.fetch_add(duration_ns(duration), Ordering::Relaxed);
}

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

#[inline]
pub fn add_glue_retry(n: u64) {
    GLUE_RETRIES_TOTAL.fetch_add(n, Ordering::Relaxed);
}

#[inline]
pub fn add_catalog_retry(n: u64) {
    CATALOG_OUTBOX_RETRIES_TOTAL.fetch_add(n, Ordering::Relaxed);
}

#[inline]
pub fn add_catalog_terminal_failure(n: u64) {
    CATALOG_OUTBOX_TERMINAL_FAILURES.fetch_add(n, Ordering::Relaxed);
}

#[inline]
pub fn add_catalog_successful_batch(n: u64) {
    CATALOG_OUTBOX_SUCCESSFUL_BATCHES.fetch_add(n, Ordering::Relaxed);
}

#[inline]
pub fn add_catalog_location_update(n: u64) {
    CATALOG_OUTBOX_LOCATION_UPDATES.fetch_add(n, Ordering::Relaxed);
}

pub fn refresh_catalog_outbox_metrics(
    outbox: &crate::catalog_outbox::CatalogOutbox,
) -> std::io::Result<()> {
    const METRIC_SCAN_LIMIT: usize = 100_000;
    let pending = outbox.scan_pending(METRIC_SCAN_LIMIT + 1)?;
    if pending.len() > METRIC_SCAN_LIMIT {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "catalog outbox pending count exceeds bounded metrics scan limit",
        ));
    }
    CATALOG_OUTBOX_PENDING.store(pending.len(), Ordering::Relaxed);
    DATA_DURABLE_CATALOG_PENDING.store(pending.len() as u64, Ordering::Relaxed);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let oldest_age = pending
        .iter()
        .map(|entry| now_ms.saturating_sub(entry.created_at_ms) / 1_000)
        .max()
        .unwrap_or(0);
    CATALOG_OUTBOX_OLDEST_AGE_SECS.store(oldest_age, Ordering::Relaxed);
    Ok(())
}

#[inline]
pub fn set_glue_retry_ema_x100(v: u64) {
    GLUE_RETRY_EMA_X100.store(v, Ordering::Relaxed);
}

// (removed unused LLM/semantic add helpers)

#[cfg(test)]
pub fn reset_flush_metrics() {
    COMPACTION_PLANNER_CYCLES_TOTAL.store(0, Ordering::Relaxed);
    COMPACTION_PLANNER_DURATION_NS_TOTAL.store(0, Ordering::Relaxed);
    COMPACTION_PLANNER_SEGMENTS_EXAMINED_TOTAL.store(0, Ordering::Relaxed);
    COMPACTION_PLANNER_SLICES_EXAMINED_TOTAL.store(0, Ordering::Relaxed);
    CDC_METADATA_SEGMENT_SCANS_TOTAL.store(0, Ordering::Relaxed);
    CDC_METADATA_SEGMENT_BYTES_EXAMINED_TOTAL.store(0, Ordering::Relaxed);
    COMPACTION_MANIFEST_DIRECTORY_SCANS_TOTAL.store(0, Ordering::Relaxed);
    COMPACTION_MANIFEST_ENTRIES_EXAMINED_TOTAL.store(0, Ordering::Relaxed);
    COMPACTION_PLANNER_READY_WORK_COUNT.store(0, Ordering::Relaxed);
    COMPACTION_INFLIGHT_SLICE_COUNT.store(0, Ordering::Relaxed);
    WAL_SNAPSHOT_READY_COUNT.store(0, Ordering::Relaxed);
    COMPACTION_ACTIVE_JOBS.store(0, Ordering::Relaxed);
    COMPACTION_ACTIVE_BY_SINK.clear();
    COMPACTION_DECODE_PERMIT_ACQUIRES_TOTAL.store(0, Ordering::Relaxed);
    COMPACTION_DECODE_PERMIT_WAIT_NS_TOTAL.store(0, Ordering::Relaxed);
    COMPACTION_SINK_PERMIT_ACQUIRES_TOTAL.store(0, Ordering::Relaxed);
    COMPACTION_SINK_PERMIT_WAIT_NS_TOTAL.store(0, Ordering::Relaxed);
    COMPACTION_SCHEDULER_TOP_UPS_TOTAL.store(0, Ordering::Relaxed);
    COMPACTION_IDLE_SLOTS_WITH_READY_WORK_TOTAL.store(0, Ordering::Relaxed);
    GROUPED_STREAM_EAGER_BUILDS_TOTAL.store(0, Ordering::Relaxed);
    GROUPED_STREAM_STREAMING_BUILDS_TOTAL.store(0, Ordering::Relaxed);
    GROUPED_STREAM_BUILD_DURATION_NS_TOTAL.store(0, Ordering::Relaxed);
    RUNTIME_SINK_POOL_ACQUIRES_TOTAL.store(0, Ordering::Relaxed);
    RUNTIME_SINK_POOL_ACQUIRE_WAIT_NS_TOTAL.store(0, Ordering::Relaxed);
    RUNTIME_SINK_POOL_WAITER_COUNT.store(0, Ordering::Relaxed);
    RUNTIME_SINK_ACTIVE_SESSION_COUNT.store(0, Ordering::Relaxed);
    RUNTIME_SINK_IPC_BYTES_TOTAL.store(0, Ordering::Relaxed);
    RUNTIME_SINK_IPC_CHUNKS_TOTAL.store(0, Ordering::Relaxed);
    RUNTIME_SCHEMA_STATE_INSTALLS_SENT_TOTAL.store(0, Ordering::Relaxed);
    RUNTIME_SCHEMA_STATE_PUBLICATIONS_SKIPPED_TOTAL.store(0, Ordering::Relaxed);
    SINK_APPLY_CALLS_TOTAL.store(0, Ordering::Relaxed);
    SINK_APPLY_DURATION_NS_TOTAL.store(0, Ordering::Relaxed);
    COMPACTION_COMPLETION_LEDGER_WRITES_TOTAL.store(0, Ordering::Relaxed);
    COMPACTION_TOMBSTONE_WRITES_TOTAL.store(0, Ordering::Relaxed);
    WAL_SEGMENT_CLOSURES_TOTAL.store(0, Ordering::Relaxed);
    WAL_SEGMENT_CLOSURE_LATENCY_NS_TOTAL.store(0, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn flush_metrics_can_be_reset_and_snapshotted() {
        reset_flush_metrics();

        record_compaction_planner_cycle(Duration::from_micros(2), 3, 11);
        record_grouped_stream_build(Duration::from_micros(5), true);
        record_runtime_sink_ipc(4096, 2);

        let snapshot = flush_metrics_snapshot();
        assert_eq!(snapshot.compaction_planner_cycles_total, 1);
        assert_eq!(snapshot.compaction_planner_duration_ns_total, 2_000);
        assert_eq!(snapshot.compaction_planner_segments_examined_total, 3);
        assert_eq!(snapshot.compaction_planner_slices_examined_total, 11);
        assert_eq!(snapshot.grouped_stream_eager_builds_total, 1);
        assert_eq!(snapshot.grouped_stream_build_duration_ns_total, 5_000);
        assert_eq!(snapshot.runtime_sink_ipc_bytes_total, 4096);
        assert_eq!(snapshot.runtime_sink_ipc_chunks_total, 2);

        reset_flush_metrics();
        assert_eq!(flush_metrics_snapshot(), FlushMetricsSnapshot::default());
    }
}
