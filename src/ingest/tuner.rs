use crate::helpers::configuration::Config;
use once_cell::sync::Lazy;
use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Minimum wall time before bytes/s (or throughput trend) is treated as meaningful.
/// Shorter spans yield unstable divisions when many samples land in one burst.
pub const MIN_THROUGHPUT_RATE_WINDOW: Duration = Duration::from_millis(100);

/// Bytes per second over `elapsed`, or zero if elapsed is shorter than `min_elapsed`.
pub fn rolling_rate_bytes_per_sec_for_elapsed(
    elapsed: Duration,
    total_bytes: u64,
    min_elapsed: Duration,
) -> u64 {
    if elapsed < min_elapsed {
        return 0;
    }
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        return 0;
    }
    (total_bytes as f64 / secs) as u64
}

/// Rolling-window ingest rate: `total_bytes` since `oldest` until `now`.
pub fn rolling_rate_bytes_per_sec(
    oldest: Instant,
    now: Instant,
    total_bytes: u64,
    min_elapsed: Duration,
) -> u64 {
    rolling_rate_bytes_per_sec_for_elapsed(
        now.saturating_duration_since(oldest),
        total_bytes,
        min_elapsed,
    )
}

/// Delta in reported bytes/s per second of wall time; zero if sample span is too short.
pub fn throughput_trend_per_sec_for_span(
    span: Duration,
    oldest_bps: u64,
    newest_bps: u64,
    min_span: Duration,
) -> f64 {
    if span < min_span {
        return 0.0;
    }
    let time_diff = span.as_secs_f64();
    if time_diff <= 0.0 {
        return 0.0;
    }
    (newest_bps as f64 - oldest_bps as f64) / time_diff
}

pub fn throughput_trend_per_sec(
    oldest_time: Instant,
    newest_time: Instant,
    oldest_bps: u64,
    newest_bps: u64,
    min_span: Duration,
) -> f64 {
    throughput_trend_per_sec_for_span(
        newest_time.saturating_duration_since(oldest_time),
        oldest_bps,
        newest_bps,
        min_span,
    )
}

const TUNE_INTERVAL: Duration = Duration::from_secs(1);
const RETRY_BACKOFF_X100: u64 = 50;
const HIGH_UPLOAD_LATENCY_MS: u64 = 30_000;
const HIGH_SINK_COMMIT_MS: u64 = 60_000;
const HIGH_SINK_WAIT_MS: u64 = 1_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlushMode {
    Ingest,
    Paused,
    Drain,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BudgetReason {
    Initial,
    EnvironmentCaps,
    IngestReserve,
    Idle,
    HealthyDrain,
    HighRss,
    LowAvailableMemory,
    HighRunQueue,
    CpuSaturated,
    S3Retry,
    GlueThrottle,
    UploadLatency,
    SinkContention,
}

impl BudgetReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::EnvironmentCaps => "environment_caps",
            Self::IngestReserve => "ingest_reserve",
            Self::Idle => "idle",
            Self::HealthyDrain => "healthy_drain",
            Self::HighRss => "high_rss",
            Self::LowAvailableMemory => "low_available_memory",
            Self::HighRunQueue => "high_run_queue",
            Self::CpuSaturated => "cpu_saturated",
            Self::S3Retry => "s3_retry",
            Self::GlueThrottle => "glue_throttle",
            Self::UploadLatency => "upload_latency",
            Self::SinkContention => "sink_contention",
        }
    }

    fn code(self) -> usize {
        match self {
            Self::Initial => 0,
            Self::EnvironmentCaps => 1,
            Self::IngestReserve => 2,
            Self::Idle => 3,
            Self::HealthyDrain => 4,
            Self::HighRss => 5,
            Self::LowAvailableMemory => 6,
            Self::HighRunQueue => 7,
            Self::CpuSaturated => 8,
            Self::S3Retry => 9,
            Self::GlueThrottle => 10,
            Self::UploadLatency => 11,
            Self::SinkContention => 12,
        }
    }
}

/// One immutable generation of every concurrency limit used by the flush pipeline.
///
/// The scheduler reads this value once per cycle. The atomics in `metrics::counters`
/// are compatibility mirrors for runtime sinks and plugins during this release.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FlushBudgetSnapshot {
    pub scheduler_jobs: usize,
    pub decode_jobs: usize,
    pub sink_sessions: usize,
    pub upload_sessions: usize,
    pub multipart_parts: usize,
    pub catalog_operations: usize,
    pub ingest_reserved_cores: usize,
    pub generation: u64,
    pub reason: BudgetReason,
}

impl Default for FlushBudgetSnapshot {
    fn default() -> Self {
        Self {
            scheduler_jobs: 1,
            decode_jobs: 1,
            sink_sessions: 1,
            upload_sessions: 1,
            multipart_parts: 1,
            catalog_operations: 1,
            ingest_reserved_cores: 1,
            generation: 0,
            reason: BudgetReason::Initial,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FlushBudgetCaps {
    pub num_cpus: usize,
    pub scheduler_jobs: usize,
    pub decode_jobs: usize,
    pub sink_sessions: usize,
    pub upload_sessions: usize,
    pub multipart_parts: usize,
    pub catalog_operations: usize,
    pub ingest_reserved_cores: usize,
    pub ingest_safe_scheduler_jobs: usize,
}

impl FlushBudgetCaps {
    pub fn for_machine(num_cpus: usize, total_memory_bytes: Option<u64>) -> Self {
        let num_cpus = num_cpus.max(2);
        let memory_gib = total_memory_bytes
            .map(|bytes| (bytes / (1024 * 1024 * 1024)).max(1).min(usize::MAX as u64) as usize);
        let scheduler_jobs = num_cpus.saturating_mul(2).clamp(2, 128);
        let decode_jobs = memory_gib
            .map(|gib| num_cpus.min(gib.saturating_mul(2)))
            .unwrap_or(num_cpus)
            .clamp(1, 128);
        let sink_sessions = memory_gib
            .map(|gib| (gib / 4).max(1))
            .unwrap_or(2)
            .min((num_cpus / 4).clamp(2, 16));
        let upload_sessions = memory_gib
            .map(|gib| gib.saturating_mul(2))
            .unwrap_or(4)
            .min(num_cpus.saturating_mul(2).clamp(4, 256));
        let multipart_parts = memory_gib
            .map(|gib| (gib / 16).clamp(1, 8))
            .unwrap_or(2)
            .min((num_cpus / 8).clamp(2, 8));
        let catalog_operations = (num_cpus / 8).clamp(2, 16);
        let ingest_reserved_cores = (num_cpus / 4).max(1);

        Self {
            num_cpus,
            scheduler_jobs,
            decode_jobs,
            sink_sessions: sink_sessions.max(1),
            upload_sessions: upload_sessions.max(1),
            multipart_parts: multipart_parts.max(1),
            catalog_operations,
            ingest_reserved_cores,
            ingest_safe_scheduler_jobs: 2.min(scheduler_jobs),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FlushBudgetSignals {
    pub mode: Option<FlushMode>,
    pub has_backlog: bool,
    pub active_ingest: usize,
    pub queued_ingest: usize,
    pub scheduler_ready_depth: usize,
    pub cpu_active_tasks: usize,
    pub run_queue: Option<f64>,
    pub process_rss_bytes: Option<u64>,
    pub total_memory_bytes: Option<u64>,
    pub available_memory_bytes: Option<u64>,
    pub decode_permit_wait_ms: Option<u64>,
    pub sink_permit_wait_ms: Option<u64>,
    pub runtime_sink_waiters: usize,
    pub uploads_in_flight: usize,
    pub upload_latency_ms: Option<u64>,
    pub sink_commit_ms: Option<u64>,
    pub s3_retries: u64,
    pub s3_retry_ema_x100: u64,
    pub glue_retries: u64,
    pub glue_retry_ema_x100: u64,
}

impl FlushBudgetSignals {
    fn mode(self) -> FlushMode {
        self.mode.unwrap_or(FlushMode::Ingest)
    }
}

fn scheduler_additive_step(num_cpus: usize) -> usize {
    (num_cpus / 32).clamp(1, 4)
}

fn cap_snapshot(
    mut snapshot: FlushBudgetSnapshot,
    caps: FlushBudgetCaps,
    ingest_busy: bool,
) -> FlushBudgetSnapshot {
    let reserved_scheduler_cap = caps
        .num_cpus
        .saturating_sub(caps.ingest_reserved_cores)
        .max(1);
    let scheduler_cap = if ingest_busy {
        caps.scheduler_jobs
            .min(caps.ingest_safe_scheduler_jobs)
            .min(reserved_scheduler_cap)
    } else {
        caps.scheduler_jobs
    };
    snapshot.scheduler_jobs = snapshot.scheduler_jobs.clamp(1, scheduler_cap.max(1));
    snapshot.decode_jobs = snapshot
        .decode_jobs
        .clamp(1, caps.decode_jobs.min(scheduler_cap).max(1));
    snapshot.sink_sessions = snapshot.sink_sessions.clamp(1, caps.sink_sessions.max(1));
    snapshot.upload_sessions = snapshot
        .upload_sessions
        .clamp(1, caps.upload_sessions.max(1));
    snapshot.multipart_parts = snapshot
        .multipart_parts
        .clamp(1, caps.multipart_parts.max(1));
    snapshot.catalog_operations = snapshot
        .catalog_operations
        .clamp(1, caps.catalog_operations.max(1));
    snapshot.ingest_reserved_cores = caps.ingest_reserved_cores;
    snapshot
}

fn same_limits(a: FlushBudgetSnapshot, b: FlushBudgetSnapshot) -> bool {
    a.scheduler_jobs == b.scheduler_jobs
        && a.decode_jobs == b.decode_jobs
        && a.sink_sessions == b.sink_sessions
        && a.upload_sessions == b.upload_sessions
        && a.multipart_parts == b.multipart_parts
        && a.catalog_operations == b.catalog_operations
        && a.ingest_reserved_cores == b.ingest_reserved_cores
}

fn finish_decision(
    current: FlushBudgetSnapshot,
    mut next: FlushBudgetSnapshot,
    reason: BudgetReason,
) -> FlushBudgetSnapshot {
    if same_limits(current, next) {
        return current;
    }
    next.generation = current.generation.saturating_add(1);
    next.reason = reason;
    next
}

fn multiplicative_backoff(snapshot: FlushBudgetSnapshot) -> FlushBudgetSnapshot {
    FlushBudgetSnapshot {
        scheduler_jobs: (snapshot.scheduler_jobs / 2).max(1),
        decode_jobs: (snapshot.decode_jobs / 2).max(1),
        sink_sessions: (snapshot.sink_sessions / 2).max(1),
        upload_sessions: (snapshot.upload_sessions / 2).max(1),
        multipart_parts: (snapshot.multipart_parts / 2).max(1),
        catalog_operations: (snapshot.catalog_operations / 2).max(1),
        ..snapshot
    }
}

fn additive_idle_decrease(snapshot: FlushBudgetSnapshot) -> FlushBudgetSnapshot {
    FlushBudgetSnapshot {
        scheduler_jobs: snapshot.scheduler_jobs.saturating_sub(1).max(1),
        decode_jobs: snapshot.decode_jobs.saturating_sub(1).max(1),
        sink_sessions: snapshot.sink_sessions.saturating_sub(1).max(1),
        upload_sessions: snapshot.upload_sessions.saturating_sub(1).max(1),
        multipart_parts: snapshot.multipart_parts.saturating_sub(1).max(1),
        catalog_operations: snapshot.catalog_operations.saturating_sub(1).max(1),
        ..snapshot
    }
}

fn ratio_at_least(value: u64, total: u64, percent: u64) -> bool {
    total > 0 && value.saturating_mul(100) >= total.saturating_mul(percent)
}

fn ratio_at_most(value: u64, total: u64, percent: u64) -> bool {
    total > 0 && value.saturating_mul(100) <= total.saturating_mul(percent)
}

/// Pure measured AIMD policy. Missing signals never become synthetic zeroes.
pub fn compute_flush_budget(
    current: FlushBudgetSnapshot,
    caps: FlushBudgetCaps,
    signals: FlushBudgetSignals,
) -> FlushBudgetSnapshot {
    let ingest_busy = signals.mode() == FlushMode::Ingest
        && (signals.active_ingest > 0 || signals.queued_ingest > 0);
    let capped = cap_snapshot(current, caps, ingest_busy);
    if !same_limits(current, capped) {
        let reason = if ingest_busy
            && (capped.scheduler_jobs < current.scheduler_jobs
                || capped.decode_jobs < current.decode_jobs)
        {
            BudgetReason::IngestReserve
        } else {
            BudgetReason::EnvironmentCaps
        };
        return finish_decision(current, capped, reason);
    }

    if let (Some(rss), Some(total)) = (signals.process_rss_bytes, signals.total_memory_bytes) {
        if ratio_at_least(rss, total, 75) {
            return finish_decision(
                current,
                multiplicative_backoff(current),
                BudgetReason::HighRss,
            );
        }
    }
    if let (Some(available), Some(total)) =
        (signals.available_memory_bytes, signals.total_memory_bytes)
    {
        if ratio_at_most(available, total, 15) {
            return finish_decision(
                current,
                multiplicative_backoff(current),
                BudgetReason::LowAvailableMemory,
            );
        }
    }
    if signals
        .run_queue
        .is_some_and(|load| load > caps.num_cpus as f64 * 1.25)
    {
        return finish_decision(
            current,
            multiplicative_backoff(current),
            BudgetReason::HighRunQueue,
        );
    }
    if signals.run_queue.is_none() && signals.cpu_active_tasks >= caps.num_cpus {
        return finish_decision(
            current,
            multiplicative_backoff(current),
            BudgetReason::CpuSaturated,
        );
    }
    if signals.s3_retries > 0 || signals.s3_retry_ema_x100 >= RETRY_BACKOFF_X100 {
        return finish_decision(
            current,
            multiplicative_backoff(current),
            BudgetReason::S3Retry,
        );
    }
    if signals.glue_retries > 0 || signals.glue_retry_ema_x100 >= RETRY_BACKOFF_X100 {
        return finish_decision(
            current,
            multiplicative_backoff(current),
            BudgetReason::GlueThrottle,
        );
    }
    if signals
        .upload_latency_ms
        .is_some_and(|latency| latency >= HIGH_UPLOAD_LATENCY_MS)
    {
        return finish_decision(
            current,
            multiplicative_backoff(current),
            BudgetReason::UploadLatency,
        );
    }
    if signals
        .sink_commit_ms
        .is_some_and(|latency| latency >= HIGH_SINK_COMMIT_MS)
        && (signals
            .sink_permit_wait_ms
            .is_some_and(|wait| wait >= HIGH_SINK_WAIT_MS)
            || signals.runtime_sink_waiters > 0)
    {
        return finish_decision(
            current,
            multiplicative_backoff(current),
            BudgetReason::SinkContention,
        );
    }

    if !signals.has_backlog {
        return finish_decision(current, additive_idle_decrease(current), BudgetReason::Idle);
    }

    // Active ingest owns its explicit CPU reserve. Queue depth alone is never a
    // reason to increase compaction, upload, sink, multipart, or catalog work.
    if signals.mode() == FlushMode::Ingest {
        return current;
    }

    let memory_healthy = match (
        signals.available_memory_bytes,
        signals.process_rss_bytes,
        signals.total_memory_bytes,
    ) {
        (Some(available), Some(rss), Some(total)) => {
            ratio_at_least(available, total, 25) && ratio_at_most(rss, total, 60)
        }
        (Some(available), _, None) => available >= 2 * 1024 * 1024 * 1024,
        (_, Some(rss), Some(total)) => ratio_at_most(rss, total, 60),
        _ => false,
    };
    let load_healthy = signals
        .run_queue
        .map(|load| load <= caps.num_cpus as f64 * 0.90)
        .unwrap_or_else(|| signals.cpu_active_tasks < caps.num_cpus.saturating_mul(3) / 4);
    let retries_healthy = signals.s3_retries == 0
        && signals.glue_retries == 0
        && signals.s3_retry_ema_x100 < RETRY_BACKOFF_X100
        && signals.glue_retry_ema_x100 < RETRY_BACKOFF_X100;

    if !(memory_healthy && load_healthy && retries_healthy) {
        return current;
    }

    let scheduler_step = scheduler_additive_step(caps.num_cpus);
    let scheduler_demand = signals.scheduler_ready_depth > current.scheduler_jobs
        || signals.decode_permit_wait_ms.is_some_and(|wait| wait > 0);
    let sink_demand = signals.scheduler_ready_depth > current.sink_sessions
        || signals.sink_permit_wait_ms.is_some_and(|wait| wait > 0)
        || signals.runtime_sink_waiters > 0;
    let upload_demand = signals.scheduler_ready_depth > current.upload_sessions
        || signals.uploads_in_flight >= current.upload_sessions
        || signals.upload_latency_ms.is_some();
    let catalog_demand = signals.scheduler_ready_depth > current.catalog_operations
        || signals.runtime_sink_waiters > 0;
    let next = FlushBudgetSnapshot {
        scheduler_jobs: if scheduler_demand {
            current
                .scheduler_jobs
                .saturating_add(scheduler_step)
                .min(caps.scheduler_jobs)
        } else {
            current.scheduler_jobs
        },
        decode_jobs: if scheduler_demand {
            current
                .decode_jobs
                .saturating_add(scheduler_step)
                .min(caps.decode_jobs)
        } else {
            current.decode_jobs
        },
        sink_sessions: if sink_demand {
            current
                .sink_sessions
                .saturating_add(1)
                .min(caps.sink_sessions)
        } else {
            current.sink_sessions
        },
        upload_sessions: if upload_demand {
            current
                .upload_sessions
                .saturating_add(scheduler_step)
                .min(caps.upload_sessions)
        } else {
            current.upload_sessions
        },
        multipart_parts: if upload_demand {
            current
                .multipart_parts
                .saturating_add(1)
                .min(caps.multipart_parts)
        } else {
            current.multipart_parts
        },
        catalog_operations: if catalog_demand {
            current
                .catalog_operations
                .saturating_add(1)
                .min(caps.catalog_operations)
        } else {
            current.catalog_operations
        },
        ..current
    };
    finish_decision(current, next, BudgetReason::HealthyDrain)
}

#[derive(Clone, Copy, Debug, Default)]
struct TelemetryTotals {
    s3_retries: u64,
    glue_retries: u64,
    uploads: u64,
    upload_latency_ns: u64,
    decode_acquires: u64,
    decode_wait_ns: u64,
    sink_acquires: u64,
    sink_wait_ns: u64,
    runtime_sink_acquires: u64,
    runtime_sink_wait_ns: u64,
    sink_commits: u64,
    sink_commit_ns: u64,
}

impl TelemetryTotals {
    fn capture() -> Self {
        use crate::metrics::counters;
        Self {
            s3_retries: counters::S3_WAL_RETRIES_TOTAL.load(Ordering::Relaxed),
            glue_retries: counters::GLUE_RETRIES_TOTAL.load(Ordering::Relaxed),
            uploads: counters::UPLOADS_TOTAL.load(Ordering::Relaxed),
            upload_latency_ns: counters::UPLOAD_LATENCY_NS_TOTAL.load(Ordering::Relaxed),
            decode_acquires: counters::COMPACTION_DECODE_PERMIT_ACQUIRES_TOTAL
                .load(Ordering::Relaxed),
            decode_wait_ns: counters::COMPACTION_DECODE_PERMIT_WAIT_NS_TOTAL
                .load(Ordering::Relaxed),
            sink_acquires: counters::COMPACTION_SINK_PERMIT_ACQUIRES_TOTAL.load(Ordering::Relaxed),
            sink_wait_ns: counters::COMPACTION_SINK_PERMIT_WAIT_NS_TOTAL.load(Ordering::Relaxed),
            runtime_sink_acquires: counters::RUNTIME_SINK_POOL_ACQUIRES_TOTAL
                .load(Ordering::Relaxed),
            runtime_sink_wait_ns: counters::RUNTIME_SINK_POOL_ACQUIRE_WAIT_NS_TOTAL
                .load(Ordering::Relaxed),
            sink_commits: counters::SINK_APPLY_CALLS_TOTAL.load(Ordering::Relaxed),
            sink_commit_ns: counters::SINK_APPLY_DURATION_NS_TOTAL.load(Ordering::Relaxed),
        }
    }
}

struct FlushTunerState {
    initialized: bool,
    current: FlushBudgetSnapshot,
    caps: FlushBudgetCaps,
    previous: TelemetryTotals,
    last_tuned: Option<Instant>,
    s3_retry_ema_x100: u64,
    glue_retry_ema_x100: u64,
}

impl Default for FlushTunerState {
    fn default() -> Self {
        Self {
            initialized: false,
            current: FlushBudgetSnapshot::default(),
            caps: FlushBudgetCaps::for_machine(2, None),
            previous: TelemetryTotals::default(),
            last_tuned: None,
            s3_retry_ema_x100: 0,
            glue_retry_ema_x100: 0,
        }
    }
}

static FLUSH_TUNER: Lazy<Mutex<FlushTunerState>> =
    Lazy::new(|| Mutex::new(FlushTunerState::default()));

fn env_cap(name: &str, log_deprecation: bool) -> Option<usize> {
    let value = Config::getenv(name, "")
        .parse::<usize>()
        .ok()
        .filter(|value| *value > 0)?;
    if log_deprecation {
        warn!(
            "tune: {}={} is deprecated as a fixed concurrency; treating it as a hard upper bound",
            name, value
        );
    }
    Some(value)
}

fn min_env_caps(default: usize, names: &[&str], log_deprecation: bool) -> usize {
    names
        .iter()
        .filter_map(|name| env_cap(name, log_deprecation))
        .fold(default, usize::min)
        .max(1)
}

fn caps_from_env(
    num_cpus: usize,
    total_memory_bytes: Option<u64>,
    log_deprecation: bool,
) -> FlushBudgetCaps {
    let mut caps = FlushBudgetCaps::for_machine(num_cpus, total_memory_bytes);
    let wal_cap = min_env_caps(
        caps.scheduler_jobs,
        &[
            "WAL_COMPACTION_CONCURRENCY_MAX",
            "WAL_COMPACTION_CONCURRENCY",
        ],
        log_deprecation,
    );
    caps.scheduler_jobs = caps.scheduler_jobs.min(wal_cap);
    caps.decode_jobs = caps.decode_jobs.min(wal_cap);
    caps.ingest_safe_scheduler_jobs = env_cap("WAL_COMPACTION_INGEST_SAFE_CAP", log_deprecation)
        .unwrap_or(caps.ingest_safe_scheduler_jobs)
        .min(caps.scheduler_jobs)
        .max(1);

    let per_sink_cap = min_env_caps(
        caps.sink_sessions,
        &["WAL_COMPACTIONS_PER_SINK_MAX", "WAL_COMPACTIONS_PER_SINK"],
        log_deprecation,
    );
    let runtime_session_cap = min_env_caps(
        caps.sink_sessions,
        &[
            "RUNTIME_SINK_SESSION_TARGET",
            "RUNTIME_SINK_SESSION_BUDGET",
        ],
        false,
    );
    caps.sink_sessions = caps
        .sink_sessions
        .min(per_sink_cap)
        .min(runtime_session_cap);

    let upload_cap = min_env_caps(
        caps.upload_sessions,
        &["UPLOAD_CONCURRENCY_MAX", "UPLOAD_CONCURRENCY"],
        log_deprecation,
    );
    caps.upload_sessions = caps.upload_sessions.min(upload_cap);
    caps.multipart_parts = caps.multipart_parts.min(upload_cap);

    caps.catalog_operations = min_env_caps(
        caps.catalog_operations,
        &[
            "ATHENA_GLUE_CP_MAX",
            "ATHENA_GLUE_CONTROL_PLANE_CONCURRENCY",
        ],
        log_deprecation,
    )
    .min(32);
    caps
}

fn seed_budget(caps: FlushBudgetCaps) -> FlushBudgetSnapshot {
    cap_snapshot(
        FlushBudgetSnapshot {
            scheduler_jobs: 2,
            decode_jobs: 2,
            sink_sessions: 2,
            upload_sessions: 4,
            multipart_parts: 2,
            catalog_operations: 2,
            ingest_reserved_cores: caps.ingest_reserved_cores,
            generation: 1,
            reason: BudgetReason::EnvironmentCaps,
        },
        caps,
        false,
    )
}

fn publish_compatibility_mirrors(snapshot: FlushBudgetSnapshot) {
    use crate::metrics::counters;
    counters::WAL_COMPACTION_CONCURRENCY_TARGET.store(snapshot.scheduler_jobs, Ordering::Relaxed);
    counters::WAL_COMPACTIONS_PER_SINK_TARGET.store(snapshot.sink_sessions, Ordering::Relaxed);
    counters::RUNTIME_SINK_POOL_TARGET.store(snapshot.sink_sessions, Ordering::Relaxed);
    counters::UPLOAD_CONCURRENCY_TARGET.store(snapshot.upload_sessions, Ordering::Relaxed);
    counters::MULTIPART_PART_CONCURRENCY_TARGET.store(snapshot.multipart_parts, Ordering::Relaxed);
    counters::ATHENA_GLUE_CP_TARGET.store(snapshot.catalog_operations, Ordering::Relaxed);
    counters::FLUSH_BUDGET_GENERATION.store(snapshot.generation as usize, Ordering::Relaxed);
    counters::FLUSH_BUDGET_REASON_CODE.store(snapshot.reason.code(), Ordering::Relaxed);
    counters::FLUSH_BUDGET_INGEST_RESERVED_CORES
        .store(snapshot.ingest_reserved_cores, Ordering::Relaxed);
}

fn memory_signals() -> (Option<u64>, Option<u64>, Option<u64>) {
    let rss = memory_stats::memory_stats().map(|usage| usage.physical_mem as u64);
    #[cfg(target_os = "linux")]
    {
        let mut total = None;
        let mut available = None;
        if let Ok(contents) = std::fs::read_to_string("/proc/meminfo") {
            for line in contents.lines() {
                let mut parts = line.split_whitespace();
                match (parts.next(), parts.next()) {
                    (Some("MemTotal:"), Some(kib)) => {
                        total = kib.parse::<u64>().ok().map(|value| value * 1024);
                    }
                    (Some("MemAvailable:"), Some(kib)) => {
                        available = kib.parse::<u64>().ok().map(|value| value * 1024);
                    }
                    _ => {}
                }
            }
        }
        return (rss, total, available);
    }
    #[cfg(target_os = "macos")]
    {
        let mut total = 0_u64;
        let mut size = std::mem::size_of::<u64>();
        let name = b"hw.memsize\0";
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr().cast(),
                (&mut total as *mut u64).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        return (rss, (rc == 0 && total > 0).then_some(total), None);
    }
    #[allow(unreachable_code)]
    (rss, None, None)
}

fn run_queue() -> Option<f64> {
    #[cfg(unix)]
    {
        let mut loads = [0.0_f64; 3];
        let count = unsafe { libc::getloadavg(loads.as_mut_ptr(), loads.len() as i32) };
        return (count > 0).then_some(loads[0]);
    }
    #[allow(unreachable_code)]
    None
}

fn ema_x100(previous: u64, delta: u64) -> u64 {
    previous
        .saturating_mul(80)
        .saturating_add(delta.saturating_mul(100).saturating_mul(20))
        / 100
}

fn delta_average_ms(
    total_count: u64,
    previous_count: u64,
    total_ns: u64,
    previous_ns: u64,
) -> Option<u64> {
    let count = total_count.saturating_sub(previous_count);
    (count > 0).then(|| {
        total_ns
            .saturating_sub(previous_ns)
            .saturating_div(count)
            .saturating_div(1_000_000)
    })
}

/// Idempotently seed the measured budget and all compatibility mirrors.
pub fn apply_env_caps() {
    if FLUSH_TUNER
        .lock()
        .expect("flush tuner mutex poisoned")
        .initialized
    {
        return;
    }
    let num_cpus = num_cpus::get().max(2);
    let (_, total_memory_bytes, _) = memory_signals();
    let caps = caps_from_env(num_cpus, total_memory_bytes, true);
    let mut state = FLUSH_TUNER.lock().expect("flush tuner mutex poisoned");
    if state.initialized {
        return;
    }
    state.caps = caps;
    state.current = seed_budget(caps);
    state.previous = TelemetryTotals::capture();
    state.s3_retry_ema_x100 =
        crate::metrics::counters::S3_WAL_RETRY_EMA_X100.load(Ordering::Relaxed);
    state.glue_retry_ema_x100 =
        crate::metrics::counters::GLUE_RETRY_EMA_X100.load(Ordering::Relaxed);
    state.last_tuned = Some(Instant::now());
    state.initialized = true;
    publish_compatibility_mirrors(state.current);
    info!(
        "flush_budget generation={} reason={} scheduler={} decode={} sink_sessions={} upload_sessions={} multipart_parts={} catalog={} ingest_reserved_cores={}",
        state.current.generation,
        state.current.reason.as_str(),
        state.current.scheduler_jobs,
        state.current.decode_jobs,
        state.current.sink_sessions,
        state.current.upload_sessions,
        state.current.multipart_parts,
        state.current.catalog_operations,
        state.current.ingest_reserved_cores,
    );

    if let Ok(v) = Config::getenv("S3_DOWNLOAD_CONCURRENCY", "").parse::<usize>() {
        if v > 0 {
            crate::metrics::counters::S3_DOWNLOAD_CONCURRENCY_TARGET
                .store(v.clamp(8, 512), Ordering::Relaxed);
        }
    }
}

pub fn current_flush_budget() -> FlushBudgetSnapshot {
    FLUSH_TUNER
        .lock()
        .expect("flush tuner mutex poisoned")
        .current
}

/// Measure one interval and atomically publish one new budget generation.
///
/// Retry deltas bypass the one-second cadence so remote throttling backs off
/// promptly. All increases remain rate-limited additive steps.
pub fn update_flush_budget(
    mode: FlushMode,
    has_backlog: bool,
    ingest_sample: Option<(usize, usize)>,
) -> FlushBudgetSnapshot {
    apply_env_caps();
    let totals = TelemetryTotals::capture();
    let now = Instant::now();
    let mut state = FLUSH_TUNER.lock().expect("flush tuner mutex poisoned");
    let s3_retries = totals.s3_retries.saturating_sub(state.previous.s3_retries);
    let glue_retries = totals
        .glue_retries
        .saturating_sub(state.previous.glue_retries);
    let urgent_retry = s3_retries > 0 || glue_retries > 0;
    if !urgent_retry
        && state
            .last_tuned
            .is_some_and(|last| now.saturating_duration_since(last) < TUNE_INTERVAL)
    {
        return state.current;
    }

    state.s3_retry_ema_x100 = ema_x100(state.s3_retry_ema_x100, s3_retries);
    state.glue_retry_ema_x100 = ema_x100(state.glue_retry_ema_x100, glue_retries);
    crate::metrics::counters::set_s3_wal_retry_ema_x100(state.s3_retry_ema_x100);
    crate::metrics::counters::set_glue_retry_ema_x100(state.glue_retry_ema_x100);

    let (rss, total_memory, available_memory) = memory_signals();
    let (active_ingest, queued_ingest) = ingest_sample.unwrap_or_else(|| {
        (
            crate::metrics::counters::ACTIVE_THREADS.load(Ordering::Relaxed),
            crate::metrics::counters::QUEUE_LENGTH.load(Ordering::Relaxed),
        )
    });
    let compaction_active =
        crate::metrics::counters::COMPACTION_ACTIVE_JOBS.load(Ordering::Relaxed);
    let compaction_sink_wait = delta_average_ms(
        totals.sink_acquires,
        state.previous.sink_acquires,
        totals.sink_wait_ns,
        state.previous.sink_wait_ns,
    );
    let runtime_sink_wait = delta_average_ms(
        totals.runtime_sink_acquires,
        state.previous.runtime_sink_acquires,
        totals.runtime_sink_wait_ns,
        state.previous.runtime_sink_wait_ns,
    );
    let signals = FlushBudgetSignals {
        mode: Some(mode),
        has_backlog,
        active_ingest,
        queued_ingest,
        scheduler_ready_depth: crate::metrics::counters::COMPACTION_PLANNER_READY_WORK_COUNT
            .load(Ordering::Relaxed),
        cpu_active_tasks: active_ingest.saturating_add(compaction_active),
        run_queue: run_queue(),
        process_rss_bytes: rss,
        total_memory_bytes: total_memory,
        available_memory_bytes: available_memory,
        decode_permit_wait_ms: delta_average_ms(
            totals.decode_acquires,
            state.previous.decode_acquires,
            totals.decode_wait_ns,
            state.previous.decode_wait_ns,
        ),
        sink_permit_wait_ms: match (compaction_sink_wait, runtime_sink_wait) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        },
        runtime_sink_waiters: crate::metrics::counters::RUNTIME_SINK_POOL_WAITER_COUNT
            .load(Ordering::Relaxed),
        uploads_in_flight: crate::metrics::counters::UPLOADS_IN_FLIGHT.load(Ordering::Relaxed),
        upload_latency_ms: delta_average_ms(
            totals.uploads,
            state.previous.uploads,
            totals.upload_latency_ns,
            state.previous.upload_latency_ns,
        ),
        sink_commit_ms: delta_average_ms(
            totals.sink_commits,
            state.previous.sink_commits,
            totals.sink_commit_ns,
            state.previous.sink_commit_ns,
        ),
        s3_retries,
        s3_retry_ema_x100: state.s3_retry_ema_x100,
        glue_retries,
        glue_retry_ema_x100: state.glue_retry_ema_x100,
    };
    let previous = state.current;
    let next = compute_flush_budget(previous, state.caps, signals);
    state.previous = totals;
    state.last_tuned = Some(now);
    if !same_limits(previous, next) {
        state.current = next;
        publish_compatibility_mirrors(next);
        info!(
            "flush_budget generation={} reason={} mode={:?} scheduler={}->{} decode={}->{} sink_sessions={}->{} upload_sessions={}->{} multipart_parts={}->{} catalog={}->{} ingest_reserved_cores={} active_ingest={} queued_ingest={} ready={} cpu_active_tasks={} run_queue={:?} rss_bytes={:?} total_memory_bytes={:?} available_memory_bytes={:?} decode_wait_ms={:?} sink_wait_ms={:?} upload_latency_ms={:?} sink_commit_ms={:?} s3_retries={} s3_retry_ema_x100={} glue_retries={} glue_retry_ema_x100={}",
            next.generation,
            next.reason.as_str(),
            mode,
            previous.scheduler_jobs,
            next.scheduler_jobs,
            previous.decode_jobs,
            next.decode_jobs,
            previous.sink_sessions,
            next.sink_sessions,
            previous.upload_sessions,
            next.upload_sessions,
            previous.multipart_parts,
            next.multipart_parts,
            previous.catalog_operations,
            next.catalog_operations,
            next.ingest_reserved_cores,
            signals.active_ingest,
            signals.queued_ingest,
            signals.scheduler_ready_depth,
            signals.cpu_active_tasks,
            signals.run_queue,
            signals.process_rss_bytes,
            signals.total_memory_bytes,
            signals.available_memory_bytes,
            signals.decode_permit_wait_ms,
            signals.sink_permit_wait_ms,
            signals.upload_latency_ms,
            signals.sink_commit_ms,
            signals.s3_retries,
            signals.s3_retry_ema_x100,
            signals.glue_retries,
            signals.glue_retry_ema_x100,
        );
    }
    state.current
}

/// Inputs for chunk size computation
pub struct ChunkSizeInputs {
    pub current_chunk_size: usize,
    pub active_cores: usize,
    pub num_cpus: usize,
    pub queue_length: usize,
    pub max_queue_length: usize,
    pub avail_mem_mib_opt: Option<usize>,
    pub max_chunk_size: usize,
}

/// Compute the next optimal chunk size based on CPU saturation, queue occupancy and memory limits.
pub fn compute_optimal_chunk_size(inputs: &ChunkSizeInputs) -> usize {
    let optimal_chunk_size_min = 4_000_000usize;
    let mut adjustment_factor = 1.0;

    // Queue occupancy guidance
    let occupancy = if inputs.max_queue_length == 0 {
        0.0
    } else {
        (inputs.queue_length as f32) / (inputs.max_queue_length as f32)
    };
    if inputs.active_cores >= inputs.num_cpus && occupancy >= 0.9 {
        adjustment_factor = 1.1;
    } else if inputs.active_cores < inputs.num_cpus && occupancy < 0.8 {
        adjustment_factor = 0.9;
    }

    let mut next = (inputs.current_chunk_size as f64 * adjustment_factor) as usize;
    if next < optimal_chunk_size_min {
        next = optimal_chunk_size_min;
    }

    // Memory-based clamp
    let denom = (inputs.num_cpus * 2).max(2);
    if let Some(avail_mib) = inputs.avail_mem_mib_opt {
        let budget_bytes = ((avail_mib as usize).saturating_mul(1024 * 1024)) / 5; // 20%
        let dyn_cap = (budget_bytes / denom).max(optimal_chunk_size_min);
        let dyn_cap = std::cmp::min(dyn_cap, inputs.max_chunk_size);
        if next > dyn_cap {
            next = dyn_cap;
        }
    } else {
        if next > inputs.max_chunk_size {
            next = inputs.max_chunk_size;
        }
    }
    next
}

/// Update throughput history, compute trend, compute and return next chunk size.
/// Returns (optimal_chunk_size, throughput_trend_per_sec).
pub fn tune_chunk_size(
    current_chunk_size: usize,
    active_cores: usize,
    queue_length: usize,
    num_cpus: usize,
    max_queue_length: usize,
    current_throughput: u64,
    max_chunk_size: usize,
    avail_mem_mib_opt: Option<usize>,
    throughput_history: &mut VecDeque<(Instant, u64)>,
) -> (usize, f64) {
    let now = Instant::now();
    // Maintain history (last 10 samples)
    throughput_history.push_back((now, current_throughput));
    while throughput_history.len() > 10 {
        throughput_history.pop_front();
    }
    // Compute trend
    let trend = if throughput_history.len() < 2 {
        0.0
    } else {
        let oldest = throughput_history.front().unwrap();
        let newest = throughput_history.back().unwrap();
        throughput_trend_per_sec(
            oldest.0,
            newest.0,
            oldest.1,
            newest.1,
            MIN_THROUGHPUT_RATE_WINDOW,
        )
    };
    // Compute next size
    let inputs = ChunkSizeInputs {
        current_chunk_size,
        active_cores,
        num_cpus,
        queue_length,
        max_queue_length,
        avail_mem_mib_opt,
        max_chunk_size,
    };
    let next = compute_optimal_chunk_size(&inputs);
    (next, trend)
}

#[cfg(test)]
mod throughput_window_tests {
    use super::{
        rolling_rate_bytes_per_sec_for_elapsed, throughput_trend_per_sec_for_span,
        MIN_THROUGHPUT_RATE_WINDOW,
    };
    use std::time::Duration;

    #[test]
    fn rolling_rate_zero_when_elapsed_below_min() {
        assert_eq!(
            rolling_rate_bytes_per_sec_for_elapsed(
                Duration::from_millis(10),
                1_000_000_000,
                MIN_THROUGHPUT_RATE_WINDOW,
            ),
            0
        );
    }

    #[test]
    fn rolling_rate_matches_bytes_over_seconds_when_above_min() {
        assert_eq!(
            rolling_rate_bytes_per_sec_for_elapsed(
                Duration::from_secs(2),
                100,
                MIN_THROUGHPUT_RATE_WINDOW,
            ),
            50
        );
    }

    #[test]
    fn rolling_rate_nonzero_at_exact_min_window() {
        assert_eq!(
            rolling_rate_bytes_per_sec_for_elapsed(
                MIN_THROUGHPUT_RATE_WINDOW,
                10_000,
                MIN_THROUGHPUT_RATE_WINDOW,
            ),
            100_000
        );
    }

    #[test]
    fn trend_zero_when_span_below_min() {
        assert_eq!(
            throughput_trend_per_sec_for_span(
                Duration::from_micros(500),
                0,
                1_000_000_000_000,
                MIN_THROUGHPUT_RATE_WINDOW,
            ),
            0.0
        );
    }

    #[test]
    fn trend_is_delta_over_time_when_span_sufficient() {
        let t = throughput_trend_per_sec_for_span(
            Duration::from_millis(500),
            100,
            600,
            MIN_THROUGHPUT_RATE_WINDOW,
        );
        assert!((t - 1000.0).abs() < 1e-6);
    }
}

#[cfg(test)]
mod tuning_tests {
    use super::{
        caps_from_env, compute_flush_budget, scheduler_additive_step, BudgetReason,
        FlushBudgetCaps, FlushBudgetSignals, FlushBudgetSnapshot, FlushMode,
    };
    use serial_test::serial;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn caps() -> FlushBudgetCaps {
        FlushBudgetCaps::for_machine(64, Some(128 * GIB))
    }

    fn current() -> FlushBudgetSnapshot {
        FlushBudgetSnapshot {
            scheduler_jobs: 8,
            decode_jobs: 8,
            sink_sessions: 8,
            upload_sessions: 8,
            multipart_parts: 4,
            catalog_operations: 4,
            ingest_reserved_cores: 16,
            generation: 10,
            reason: BudgetReason::Initial,
        }
    }

    fn healthy_drain() -> FlushBudgetSignals {
        FlushBudgetSignals {
            mode: Some(FlushMode::Drain),
            has_backlog: true,
            scheduler_ready_depth: 32,
            cpu_active_tasks: 8,
            run_queue: Some(8.0),
            process_rss_bytes: Some(32 * GIB),
            total_memory_bytes: Some(128 * GIB),
            available_memory_bytes: Some(80 * GIB),
            ..FlushBudgetSignals::default()
        }
    }

    #[test]
    fn ingest_reserve_caps_busy_compaction_without_queue_growth() {
        let signals = FlushBudgetSignals {
            mode: Some(FlushMode::Ingest),
            has_backlog: true,
            active_ingest: 48,
            queued_ingest: 10_000,
            cpu_active_tasks: 48,
            run_queue: Some(32.0),
            process_rss_bytes: Some(24 * GIB),
            total_memory_bytes: Some(128 * GIB),
            available_memory_bytes: Some(96 * GIB),
            ..FlushBudgetSignals::default()
        };
        let next = compute_flush_budget(current(), caps(), signals);
        assert_eq!(next.scheduler_jobs, 2);
        assert_eq!(next.decode_jobs, 2);
        assert_eq!(next.reason, BudgetReason::IngestReserve);
        assert_eq!(next.ingest_reserved_cores, 16);
    }

    #[test]
    fn healthy_drain_ramps_additively() {
        let before = current();
        let next = compute_flush_budget(before, caps(), healthy_drain());
        assert_eq!(
            next.scheduler_jobs,
            before.scheduler_jobs + scheduler_additive_step(64)
        );
        assert_eq!(
            next.decode_jobs,
            before.decode_jobs + scheduler_additive_step(64)
        );
        assert_eq!(next.sink_sessions, before.sink_sessions + 1);
        assert_eq!(next.multipart_parts, before.multipart_parts + 1);
        assert_eq!(next.catalog_operations, before.catalog_operations + 1);
        assert_eq!(next.reason, BudgetReason::HealthyDrain);
    }

    #[test]
    fn high_rss_backs_off_multiplicatively() {
        let mut signals = healthy_drain();
        signals.process_rss_bytes = Some(100 * GIB);
        let next = compute_flush_budget(current(), caps(), signals);
        assert_eq!(next.scheduler_jobs, 4);
        assert_eq!(next.decode_jobs, 4);
        assert_eq!(next.sink_sessions, 4);
        assert_eq!(next.reason, BudgetReason::HighRss);
    }

    #[test]
    fn high_run_queue_backs_off_multiplicatively() {
        let mut signals = healthy_drain();
        signals.run_queue = Some(96.0);
        let next = compute_flush_budget(current(), caps(), signals);
        assert_eq!(next.scheduler_jobs, 4);
        assert_eq!(next.reason, BudgetReason::HighRunQueue);
    }

    #[test]
    fn high_task_count_with_healthy_run_queue_does_not_back_off() {
        let before = current();
        let mut signals = healthy_drain();
        signals.cpu_active_tasks = 128;
        signals.run_queue = Some(8.0);
        let next = compute_flush_budget(before, caps(), signals);
        assert_eq!(next.reason, BudgetReason::HealthyDrain);
        assert!(next.scheduler_jobs > before.scheduler_jobs);
        assert!(next.sink_sessions > before.sink_sessions);
    }

    #[test]
    fn high_task_count_without_run_queue_uses_conservative_backoff() {
        let mut signals = healthy_drain();
        signals.cpu_active_tasks = 64;
        signals.run_queue = None;
        let next = compute_flush_budget(current(), caps(), signals);
        assert_eq!(next.scheduler_jobs, 4);
        assert_eq!(next.reason, BudgetReason::CpuSaturated);
    }

    #[test]
    fn s3_retry_backs_off_all_flush_stages() {
        let mut signals = healthy_drain();
        signals.s3_retries = 1;
        let next = compute_flush_budget(current(), caps(), signals);
        assert_eq!(next.scheduler_jobs, 4);
        assert_eq!(next.multipart_parts, 2);
        assert_eq!(next.reason, BudgetReason::S3Retry);
    }

    #[test]
    fn glue_retry_backs_off_catalog_and_producers() {
        let mut signals = healthy_drain();
        signals.glue_retry_ema_x100 = 50;
        let next = compute_flush_budget(current(), caps(), signals);
        assert_eq!(next.catalog_operations, 2);
        assert_eq!(next.scheduler_jobs, 4);
        assert_eq!(next.reason, BudgetReason::GlueThrottle);
    }

    struct EnvGuard {
        values: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvGuard {
        fn clear(names: &[&'static str]) -> Self {
            let values = names
                .iter()
                .map(|name| {
                    let old = std::env::var_os(name);
                    std::env::remove_var(name);
                    (*name, old)
                })
                .collect();
            Self { values }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (name, value) in self.values.drain(..) {
                if let Some(value) = value {
                    std::env::set_var(name, value);
                } else {
                    std::env::remove_var(name);
                }
            }
        }
    }

    #[test]
    #[serial]
    fn legacy_env_values_are_hard_upper_bounds() {
        const NAMES: &[&str] = &[
            "WAL_COMPACTION_CONCURRENCY_MAX",
            "WAL_COMPACTION_CONCURRENCY",
            "WAL_COMPACTION_INGEST_SAFE_CAP",
            "WAL_COMPACTIONS_PER_SINK_MAX",
            "WAL_COMPACTIONS_PER_SINK",
            "RUNTIME_SINK_CONNECTION_POOL_SIZE",
            "RUNTIME_SINK_SESSION_TARGET",
            "RUNTIME_SINK_SESSION_BUDGET",
            "UPLOAD_CONCURRENCY_MAX",
            "UPLOAD_CONCURRENCY",
            "ATHENA_GLUE_CP_MAX",
            "ATHENA_GLUE_CONTROL_PLANE_CONCURRENCY",
        ];
        let _guard = EnvGuard::clear(NAMES);
        std::env::set_var("WAL_COMPACTION_CONCURRENCY_MAX", "9");
        std::env::set_var("WAL_COMPACTION_CONCURRENCY", "7");
        std::env::set_var("WAL_COMPACTION_INGEST_SAFE_CAP", "5");
        std::env::set_var("WAL_COMPACTIONS_PER_SINK_MAX", "5");
        std::env::set_var("WAL_COMPACTIONS_PER_SINK", "3");
        std::env::set_var("RUNTIME_SINK_CONNECTION_POOL_SIZE", "4");
        std::env::set_var("RUNTIME_SINK_SESSION_TARGET", "2");
        std::env::set_var("UPLOAD_CONCURRENCY_MAX", "10");
        std::env::set_var("UPLOAD_CONCURRENCY", "6");
        std::env::set_var("ATHENA_GLUE_CP_MAX", "5");
        std::env::set_var("ATHENA_GLUE_CONTROL_PLANE_CONCURRENCY", "3");

        let caps = caps_from_env(64, Some(128 * GIB), false);
        assert_eq!(caps.scheduler_jobs, 7);
        assert_eq!(caps.decode_jobs, 7);
        assert_eq!(caps.ingest_safe_scheduler_jobs, 5);
        assert_eq!(caps.sink_sessions, 2);
        assert_eq!(caps.upload_sessions, 6);
        assert_eq!(caps.multipart_parts, 6);
        assert_eq!(caps.catalog_operations, 3);
    }

    #[test]
    fn additive_recovery_never_exceeds_one_aimd_step() {
        let before = current();
        let next = compute_flush_budget(before, caps(), healthy_drain());
        let scheduler_step = scheduler_additive_step(64);
        assert!(next.scheduler_jobs - before.scheduler_jobs <= scheduler_step);
        assert!(next.decode_jobs - before.decode_jobs <= scheduler_step);
        assert!(next.upload_sessions - before.upload_sessions <= scheduler_step);
        assert!(next.sink_sessions - before.sink_sessions <= 1);
        assert!(next.multipart_parts - before.multipart_parts <= 1);
        assert!(next.catalog_operations - before.catalog_operations <= 1);
    }

    #[test]
    fn backoff_honours_absolute_minima() {
        let minimum = FlushBudgetSnapshot {
            scheduler_jobs: 1,
            decode_jobs: 1,
            sink_sessions: 1,
            upload_sessions: 1,
            multipart_parts: 1,
            catalog_operations: 1,
            ..current()
        };
        let mut signals = healthy_drain();
        signals.s3_retries = 1;
        assert_eq!(compute_flush_budget(minimum, caps(), signals), minimum);
    }

    #[test]
    fn reference_64_core_128_gib_caps_are_bounded() {
        let caps = caps();
        assert_eq!(caps.num_cpus, 64);
        assert_eq!(caps.scheduler_jobs, 128);
        assert_eq!(caps.decode_jobs, 64);
        assert_eq!(caps.sink_sessions, 16);
        assert_eq!(caps.upload_sessions, 128);
        assert_eq!(caps.multipart_parts, 8);
        assert_eq!(caps.catalog_operations, 8);
        assert_eq!(caps.ingest_reserved_cores, 16);
    }
}
