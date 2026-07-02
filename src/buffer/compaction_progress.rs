use std::fmt::Write as _;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use once_cell::sync::Lazy;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GroupedCompactionPhase {
    BuildingStream,
    UploadingToSink,
}

impl GroupedCompactionPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BuildingStream => "building_wal_stream",
            Self::UploadingToSink => "uploading_to_sink",
        }
    }
}

#[derive(Clone, Debug)]
struct GroupedCompactionJob {
    compaction_id: String,
    namespace: String,
    sink_ref: String,
    wal_parts: usize,
    timeout: Duration,
    started_at: Instant,
    phase: GroupedCompactionPhase,
    phase_started_at: Instant,
    rows: Option<u64>,
    manifest_attempts: u32,
}

static GROUPED_COMPACTION_JOBS: Lazy<DashMap<String, GroupedCompactionJob>> = Lazy::new(DashMap::new);

pub struct GroupedCompactionTracker {
    compaction_id: String,
    active: bool,
}

impl GroupedCompactionTracker {
    pub fn begin(
        compaction_id: impl Into<String>,
        namespace: impl Into<String>,
        sink_ref: impl Into<String>,
        wal_parts: usize,
        timeout: Duration,
        manifest_attempts: u32,
    ) -> Self {
        let compaction_id = compaction_id.into();
        let now = Instant::now();
        GROUPED_COMPACTION_JOBS.insert(
            compaction_id.clone(),
            GroupedCompactionJob {
                compaction_id: compaction_id.clone(),
                namespace: namespace.into(),
                sink_ref: sink_ref.into(),
                wal_parts,
                timeout,
                started_at: now,
                phase: GroupedCompactionPhase::BuildingStream,
                phase_started_at: now,
                rows: None,
                manifest_attempts,
            },
        );
        Self {
            compaction_id,
            active: true,
        }
    }

    pub fn set_phase(&self, phase: GroupedCompactionPhase) {
        if let Some(mut job) = GROUPED_COMPACTION_JOBS.get_mut(&self.compaction_id) {
            job.phase = phase;
            job.phase_started_at = Instant::now();
        }
    }

    pub fn set_rows(&self, rows: u64) {
        if let Some(mut job) = GROUPED_COMPACTION_JOBS.get_mut(&self.compaction_id) {
            job.rows = Some(rows);
        }
    }

    pub fn finish(mut self) {
        GROUPED_COMPACTION_JOBS.remove(&self.compaction_id);
        self.active = false;
    }
}

impl Drop for GroupedCompactionTracker {
    fn drop(&mut self) {
        if self.active {
            GROUPED_COMPACTION_JOBS.remove(&self.compaction_id);
        }
    }
}

fn short_id(compaction_id: &str) -> &str {
    if compaction_id.len() <= 12 {
        compaction_id
    } else {
        &compaction_id[..12]
    }
}

fn format_duration_secs(duration: Duration) -> String {
    format!("{}s", duration.as_secs())
}

pub fn format_in_flight_grouped_compactions() -> String {
    if GROUPED_COMPACTION_JOBS.is_empty() {
        return "none".to_string();
    }

    let mut jobs: Vec<_> = GROUPED_COMPACTION_JOBS
        .iter()
        .map(|entry| entry.value().clone())
        .collect();
    jobs.sort_by(|a, b| a.started_at.cmp(&b.started_at));

    let mut out = String::new();
    for (idx, job) in jobs.iter().enumerate() {
        if idx > 0 {
            out.push_str("; ");
        }
        let _ = write!(
            out,
            "{}",
            format_grouped_compaction_job_detail(job),
        );
    }
    out
}

fn format_grouped_compaction_job_detail(job: &GroupedCompactionJob) -> String {
    let elapsed = job.started_at.elapsed();
    let phase_elapsed = job.phase_started_at.elapsed();
    let timeout_in = job.timeout.saturating_sub(elapsed);
    let mut out = String::new();
    let _ = write!(
        out,
        "{} phase={} job_elapsed={} phase_elapsed={} timeout_in={} wal_parts={} sink={} id={}",
        job.namespace,
        job.phase.as_str(),
        format_duration_secs(elapsed),
        format_duration_secs(phase_elapsed),
        format_duration_secs(timeout_in),
        job.wal_parts,
        job.sink_ref,
        short_id(&job.compaction_id),
    );
    if let Some(rows) = job.rows {
        let _ = write!(out, " rows={rows}");
    }
    if job.manifest_attempts > 1 {
        let _ = write!(out, " attempts={}", job.manifest_attempts);
    }
    out
}

fn format_progress_bar(completed: u64, total: u64, width: usize) -> String {
    if total == 0 {
        return "·".repeat(width);
    }
    let filled = ((completed as f64 / total as f64) * width as f64).round() as usize;
    let filled = filled.min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width.saturating_sub(filled)))
}

fn format_delta_per_interval(delta: u64, interval: Duration) -> String {
    format!("+{}/{}s", delta, interval.as_secs())
}

/// Human-readable compactor drain heartbeat for INFO logs.
pub fn format_compactor_drain_status_summary(
    drain_elapsed: Duration,
    interval: Duration,
    counters: WalCompactionCounterSnapshot,
    since_last: WalCompactionCounterSnapshot,
    reclaimable_partitions: usize,
    wal_in_flight: usize,
    uploads_in_flight: usize,
) -> String {
    let wal_bar = format_progress_bar(
        counters.wal_parts_completed,
        counters.wal_parts_started,
        20,
    );
    let wal_delta = format_delta_per_interval(since_last.wal_parts_completed, interval);
    let txn_delta = format_delta_per_interval(since_last.txn_completed, interval);

    let mut out = format!(
        "Compactor drain: elapsed={}s | wal {}/{} [{}] {wal_delta} | txns {}/{} {txn_delta}",
        drain_elapsed.as_secs(),
        counters.wal_parts_completed,
        counters.wal_parts_started,
        wal_bar,
        counters.txn_completed,
        counters.txn_started,
    );

    if counters.txn_failed > 0 {
        let _ = write!(out, " failed={}", counters.txn_failed);
    }

    let _ = write!(
        out,
        " | backlog={reclaimable_partitions} wal_in_flight={wal_in_flight} uploads_in_flight={uploads_in_flight}"
    );

    if GROUPED_COMPACTION_JOBS.is_empty() {
        if since_last.wal_parts_completed == 0 && since_last.txn_completed == 0 {
            let _ = write!(out, " | no progress/{}s", interval.as_secs());
        }
        return out;
    }

    let mut jobs: Vec<_> = GROUPED_COMPACTION_JOBS
        .iter()
        .map(|entry| entry.value().clone())
        .collect();
    jobs.sort_by(|a, b| a.started_at.cmp(&b.started_at));

    let stalled =
        since_last.wal_parts_completed == 0 && since_last.txn_completed == 0;
    if stalled {
        let _ = write!(out, " | no wal/txn progress/{}s", interval.as_secs());
    }

    let _ = write!(out, " | active({}): ", jobs.len());
    for (idx, job) in jobs.iter().enumerate() {
        if idx > 0 {
            out.push_str("; ");
        }
        let elapsed = job.started_at.elapsed();
        let phase_elapsed = job.phase_started_at.elapsed();
        let timeout_in = job.timeout.saturating_sub(elapsed);
        let phase_label = match job.phase {
            GroupedCompactionPhase::BuildingStream => "building",
            GroupedCompactionPhase::UploadingToSink => "uploading",
        };
        let _ = write!(
            out,
            "{} {} job={} phase={} timeout_in={} wal_parts={}",
            job.namespace,
            phase_label,
            format_duration_secs(elapsed),
            format_duration_secs(phase_elapsed),
            format_duration_secs(timeout_in),
            job.wal_parts,
        );
        if let Some(rows) = job.rows {
            let _ = write!(out, " rows={rows}");
        }
    }

    out
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WalCompactionCounterSnapshot {
    pub wal_parts_started: u64,
    pub wal_parts_completed: u64,
    pub txn_started: u64,
    pub txn_completed: u64,
    pub txn_failed: u64,
}

impl WalCompactionCounterSnapshot {
    pub fn capture() -> Self {
        use crate::metrics::counters;
        use std::sync::atomic::Ordering;
        Self {
            wal_parts_started: counters::WAL_COMPACTIONS_STARTED.load(Ordering::Relaxed),
            wal_parts_completed: counters::WAL_COMPACTIONS_COMPLETED.load(Ordering::Relaxed),
            txn_started: counters::WAL_COMPACTION_TRANSACTIONS_STARTED.load(Ordering::Relaxed),
            txn_completed: counters::WAL_COMPACTION_TRANSACTIONS_COMPLETED.load(Ordering::Relaxed),
            txn_failed: counters::WAL_COMPACTION_TRANSACTIONS_FAILED.load(Ordering::Relaxed),
        }
    }
}

pub struct CompactorDrainProgress {
    baseline: WalCompactionCounterSnapshot,
}

impl CompactorDrainProgress {
    pub fn new() -> Self {
        Self {
            baseline: WalCompactionCounterSnapshot::capture(),
        }
    }

    pub fn deltas_since_drain_start(&self) -> WalCompactionCounterSnapshot {
        let current = WalCompactionCounterSnapshot::capture();
        WalCompactionCounterSnapshot {
            wal_parts_started: current
                .wal_parts_started
                .saturating_sub(self.baseline.wal_parts_started),
            wal_parts_completed: current
                .wal_parts_completed
                .saturating_sub(self.baseline.wal_parts_completed),
            txn_started: current
                .txn_started
                .saturating_sub(self.baseline.txn_started),
            txn_completed: current
                .txn_completed
                .saturating_sub(self.baseline.txn_completed),
            txn_failed: current.txn_failed.saturating_sub(self.baseline.txn_failed),
        }
    }
}

pub struct CompactorDrainHeartbeat {
    last_counters: WalCompactionCounterSnapshot,
}

impl CompactorDrainHeartbeat {
    pub fn new() -> Self {
        Self {
            last_counters: WalCompactionCounterSnapshot::capture(),
        }
    }

    pub fn deltas_since_last_log(&mut self) -> WalCompactionCounterSnapshot {
        let current = WalCompactionCounterSnapshot::capture();
        let delta = WalCompactionCounterSnapshot {
            wal_parts_started: current
                .wal_parts_started
                .saturating_sub(self.last_counters.wal_parts_started),
            wal_parts_completed: current
                .wal_parts_completed
                .saturating_sub(self.last_counters.wal_parts_completed),
            txn_started: current
                .txn_started
                .saturating_sub(self.last_counters.txn_started),
            txn_completed: current
                .txn_completed
                .saturating_sub(self.last_counters.txn_completed),
            txn_failed: current
                .txn_failed
                .saturating_sub(self.last_counters.txn_failed),
        };
        self.last_counters = current;
        delta
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static TEST_JOB_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn grouped_compaction_phase_labels_are_stable() {
        assert_eq!(
            GroupedCompactionPhase::BuildingStream.as_str(),
            "building_wal_stream"
        );
        assert_eq!(
            GroupedCompactionPhase::UploadingToSink.as_str(),
            "uploading_to_sink"
        );
    }

    #[test]
    fn in_flight_summary_lists_active_jobs() {
        let _guard = TEST_JOB_LOCK.lock().unwrap();
        GROUPED_COMPACTION_JOBS.clear();
        let _tracker = GroupedCompactionTracker::begin(
            "abc123def456",
            "cube_events",
            "data_sinks.ds_datalake",
            86,
            Duration::from_secs(900),
            3,
        );
        let summary = format_in_flight_grouped_compactions();
        assert!(summary.contains("cube_events"));
        assert!(summary.contains("building_wal_stream"));
        assert!(summary.contains("wal_parts=86"));
        assert!(summary.contains("attempts=3"));
        GROUPED_COMPACTION_JOBS.clear();
    }

    #[test]
    fn drain_status_summary_includes_progress_and_active_job() {
        let _guard = TEST_JOB_LOCK.lock().unwrap();
        GROUPED_COMPACTION_JOBS.clear();
        let tracker = GroupedCompactionTracker::begin(
            "abc123def456",
            "device_data",
            "data_sinks.ds_datalake",
            41,
            Duration::from_secs(900),
            1,
        );
        tracker.set_phase(GroupedCompactionPhase::UploadingToSink);

        let summary = format_compactor_drain_status_summary(
            Duration::from_secs(2421),
            Duration::from_secs(5),
            WalCompactionCounterSnapshot {
                wal_parts_started: 4343,
                wal_parts_completed: 4302,
                txn_started: 240,
                txn_completed: 239,
                txn_failed: 0,
            },
            WalCompactionCounterSnapshot {
                wal_parts_started: 0,
                wal_parts_completed: 0,
                txn_started: 0,
                txn_completed: 0,
                txn_failed: 0,
            },
            1939,
            1,
            0,
        );

        assert!(summary.contains("elapsed=2421s"));
        assert!(summary.contains("4302/4343"));
        assert!(summary.contains("+0/5s"));
        assert!(summary.contains("backlog=1939"));
        assert!(summary.contains("no wal/txn progress/5s"));
        assert!(summary.contains("device_data uploading"));
        assert!(summary.contains("wal_parts=41"));

        GROUPED_COMPACTION_JOBS.clear();
    }

    #[test]
    fn drain_status_summary_shows_recent_progress() {
        GROUPED_COMPACTION_JOBS.clear();
        let summary = format_compactor_drain_status_summary(
            Duration::from_secs(120),
            Duration::from_secs(5),
            WalCompactionCounterSnapshot {
                wal_parts_started: 200,
                wal_parts_completed: 50,
                txn_started: 45,
                txn_completed: 45,
                txn_failed: 0,
            },
            WalCompactionCounterSnapshot {
                wal_parts_started: 12,
                wal_parts_completed: 12,
                txn_started: 12,
                txn_completed: 12,
                txn_failed: 0,
            },
            500,
            0,
            0,
        );

        assert!(summary.contains("+12/5s"));
        assert!(!summary.contains("no wal/txn progress"));
        GROUPED_COMPACTION_JOBS.clear();
    }
}
