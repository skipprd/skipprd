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
        let elapsed = job.started_at.elapsed();
        let phase_elapsed = job.phase_started_at.elapsed();
        let timeout_in = job.timeout.saturating_sub(elapsed);
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
}
