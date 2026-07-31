use crate::buffer::compaction_index::{
    CompactionGroupKey, CompactionIndex, CompactionKind, CompactionLaneKey, IndexedCompactionSlice,
    PlannedCompactionGroup,
};
use crate::buffer::compaction_progress::{
    format_compactor_drain_status_summary, format_in_flight_grouped_compactions,
    CompactorDrainHeartbeat, CompactorDrainProgress, GroupedCompactionPhase,
    GroupedCompactionTracker, WalCompactionCounterSnapshot,
};
use crate::buffer::compaction_scheduler::{
    drive_work_conserving, CompactionCycleControl, CompactionPlanBatch, CompactorCommand,
    SchedulableCompactionWork,
};
use crate::buffer::compaction_transaction::{
    load_manifest_index, persist_manifest, remove_manifest, CompactionTransaction,
    SegmentSourceDescriptor, SinkRetrySemantics, SinkWriteSemantics, WalPartRef,
};
use crate::buffer::completion_ledger::{SegmentCompletionLedger, SegmentCompletionUpdate};
use crate::buffer::flush_execution_budget::FlushExecutionBudget;
use crate::buffer::s3_wal_body_cache;
#[cfg(test)]
use crate::buffer::segment_file::SegmentPartMetaSummary;
use crate::buffer::segment_file::{
    PartitionKey, SegmentFile, SegmentFileMetadata, SegmentPartitionIndexEntry,
};
use crate::buffer::wal_store::{SegmentWriteLocation, SegmentWriteResult};
use crate::buffer::BufferChunker;
use crate::helpers::configuration::Config;
use crate::helpers::offsets::{OffsetKey, OffsetTypes, Offsets};
use crate::helpers::timed_rwlock::TimedRwLock;
use crate::helpers::Helpers;
use crate::metrics::counters as metrics_hot;
use crate::plugins::cdc::SinkCapability;
use crate::plugins::DataSink;
use crate::METRICS;
use arrow::array::RecordBatch;
use arrow::ipc::reader::StreamReader;
use arrow_schema::{ArrowError, SchemaRef};
use dashmap::DashMap;
use datafusion::error::DataFusionError;
use datafusion::physical_plan::RecordBatchStream;
use datafusion::physical_plan::SendableRecordBatchStream;
use hex;
use once_cell::sync::Lazy;
use once_cell::sync::Lazy as OnceLazy;
use rayon::prelude::*;
use serde_derive::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll as TaskPoll};
use std::time::SystemTime;
use std::{fs, io};
use tokio::time::{sleep as tokio_sleep, Duration as TokioDuration};
use tracing::{debug, error, info, warn};

use tokio::sync::mpsc;

/// Identifies a WAL segment and provides access to its data.
/// `Disk` holds a local path; `S3` holds bucket + object key. Optional `body`
/// is a hot in-memory copy; when absent, bytes live on S3 and compaction loads
/// them via the bounded body LRU.
#[derive(Clone)]
pub(crate) enum SegmentSource {
    Disk(PathBuf),
    S3 {
        key: String,
        bucket: String,
        body: Option<Arc<Vec<u8>>>,
    },
}

impl SegmentSource {
    /// Stable identifier for tombstone filenames and dedup.
    fn segment_id(&self) -> &str {
        match self {
            SegmentSource::Disk(p) => p.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown"),
            SegmentSource::S3 { key, .. } => {
                let filename = key.rsplit('/').next().unwrap_or(key);
                filename.strip_suffix(".seg").unwrap_or(filename)
            }
        }
    }

    fn display_name(&self) -> String {
        match self {
            SegmentSource::Disk(p) => p.to_string_lossy().to_string(),
            SegmentSource::S3 { key, bucket, .. } => format!("s3://{bucket}/{key}"),
        }
    }

    fn logical_byte_len(&self, meta: &SegmentFileMetadata) -> io::Result<u64> {
        match self {
            SegmentSource::Disk(p) => {
                let file = OpenOptions::new().read(true).open(p)?;
                Ok(file.metadata()?.len())
            }
            SegmentSource::S3 {
                body: Some(data), ..
            } => Ok(data.len() as u64),
            SegmentSource::S3 { body: None, .. } => Ok(meta.total_bytes),
        }
    }

    fn is_s3(&self) -> bool {
        matches!(self, SegmentSource::S3 { .. })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DiskSegmentCleanup {
    Removed,
    AlreadyMissing,
}

fn is_s3_wal() -> bool {
    Config::get_wal_storage().eq_ignore_ascii_case("s3")
}

#[allow(dead_code)]
pub static TOTAL_ROWS: Lazy<TimedRwLock<AtomicU64>> =
    Lazy::new(|| TimedRwLock::new("record_batch_total".to_string(), AtomicU64::new(0)));

// Lock-free WAL index and counters
pub static WAL_BYTES_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
static COMPACTOR_STARTED: Lazy<std::sync::atomic::AtomicBool> =
    Lazy::new(|| std::sync::atomic::AtomicBool::new(false));
static COMPACTOR_HANDLE: Lazy<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>> =
    Lazy::new(|| std::sync::Mutex::new(None));
static COMPACTOR_COMMAND_TX: Lazy<
    std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<CompactorCommand>>>,
> = Lazy::new(|| std::sync::Mutex::new(None));

static COMPACT_FAILURES: Lazy<DashMap<String, u32>> = Lazy::new(DashMap::new);

const DEFAULT_COMPACTION_GROUP_TARGET_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_COMPACTION_GROUP_MAX_PARTS: usize = 128;
const DEFAULT_GROUPED_STREAM_EAGER_MAX_PARTS: usize = 16;
const DEFAULT_GROUPED_STREAM_EAGER_MAX_BYTES: u64 = 8 * 1024 * 1024;
const GROUPED_STREAM_PREFETCH_PARTS: usize = 4;
const GROUPED_STREAM_CHANNEL_CAPACITY: usize = 8;

// Global in-memory segment accumulator (reduces tiny WAL files across threads)
static SEGMENT_LIVE: Lazy<Arc<std::sync::Mutex<GlobalSegment>>> =
    Lazy::new(|| Arc::new(std::sync::Mutex::new(GlobalSegment::new())));
// Segment snapshot representation
#[derive(Clone)]
struct SegmentPartitionMeta {
    bytes: u64,
    updated_at: SystemTime,
}

struct SegmentSnapshot {
    id: String,
    created_at: SystemTime,
    total_bytes: u64,
    offsets: HashMap<OffsetKey, u64>,
    batches: HashMap<PartitionKey, Vec<RecordBatch>>,
    meta: HashMap<PartitionKey, SegmentPartitionMeta>,
    part_meta_blobs: HashMap<PartitionKey, Vec<u8>>,
    checkpoint_updates: Vec<(String, crate::plugins::cdc::CheckpointEnvelope)>,
}

impl SegmentSnapshot {
    fn new(
        id: String,
        offsets: HashMap<OffsetKey, u64>,
        batches: HashMap<PartitionKey, Vec<RecordBatch>>,
        meta: HashMap<PartitionKey, SegmentPartitionMeta>,
        total_bytes: u64,
        part_meta_blobs: HashMap<PartitionKey, Vec<u8>>,
        checkpoint_updates: Vec<(String, crate::plugins::cdc::CheckpointEnvelope)>,
    ) -> Self {
        let now = SystemTime::now();
        SegmentSnapshot {
            id,
            created_at: now,
            total_bytes,
            offsets,
            batches,
            meta,
            part_meta_blobs,
            checkpoint_updates,
        }
    }
}

// Global single-flight guard to avoid double compaction of the same partition region
static COMPACTION_IN_FLIGHT: OnceLazy<DashMap<(String, u64, u64), ()>> =
    OnceLazy::new(|| DashMap::new());
static SWEEP_CYCLE_COUNTER: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));

/// In-memory cache of committed WAL segments.
/// Populated on startup (wal_recover) and during ingest (flush).
/// Entries are removed when all partitions in a segment have been compacted.
struct CachedSegment {
    source: SegmentSource,
    meta: SegmentFileMetadata,
}

static SEGMENT_CACHE: OnceLazy<DashMap<String, CachedSegment>> = OnceLazy::new(DashMap::new);
static COMPACTION_INDEX: Lazy<std::sync::Mutex<CompactionIndex>> =
    Lazy::new(|| std::sync::Mutex::new(CompactionIndex::default()));

fn compaction_index() -> std::sync::MutexGuard<'static, CompactionIndex> {
    COMPACTION_INDEX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Clone)]
struct CompactionEntry {
    source: SegmentSource,
    meta: SegmentFileMetadata,
    ordinal: usize,
    idx: SegmentPartitionIndexEntry,
    wal_ref: WalPartRef,
}

struct CompactionWork {
    txn: CompactionTransaction,
    entries: Vec<CompactionEntry>,
}

impl SchedulableCompactionWork for CompactionWork {
    fn lane(&self) -> CompactionLaneKey {
        CompactionLaneKey {
            sink_ref: self.txn.sink_ref.clone(),
            namespace: self.txn.namespace.clone(),
        }
    }
}

struct CompactionReservationGuard {
    txn_id: String,
    refs: Vec<WalPartRef>,
    armed: bool,
}

impl CompactionReservationGuard {
    fn new(work: &CompactionWork) -> Self {
        Self {
            txn_id: work.txn.id.clone(),
            refs: work.txn.refs.clone(),
            armed: true,
        }
    }

    fn handoff(&mut self) {
        self.armed = false;
    }
}

impl Drop for CompactionReservationGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut index = compaction_index();
        index.release_refs(&self.refs, Buffers::now_secs());
        index.release_manifest(&self.txn_id);
        metrics_hot::set_compaction_planner_ready_work_count(index.ready_queue_depth());
    }
}

#[cfg(test)]
mod work_conserving_scheduler_tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct TestWork {
        id: &'static str,
        lane: CompactionLaneKey,
        delay: TokioDuration,
        compacted: bool,
    }

    impl SchedulableCompactionWork for TestWork {
        fn lane(&self) -> CompactionLaneKey {
            self.lane.clone()
        }
    }

    #[derive(Default)]
    struct TestExecutionState {
        started: Mutex<Vec<&'static str>>,
        completed: Mutex<Vec<&'static str>>,
        active: AtomicUsize,
        max_active: AtomicUsize,
        reservations: AtomicUsize,
    }

    struct TestReservationGuard(Arc<TestExecutionState>);

    impl Drop for TestReservationGuard {
        fn drop(&mut self) {
            self.0.reservations.fetch_sub(1, Ordering::SeqCst);
        }
    }

    struct TestActiveGuard(Arc<TestExecutionState>);

    impl Drop for TestActiveGuard {
        fn drop(&mut self) {
            self.0.active.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn lane(sink_ref: &str, namespace: &str) -> CompactionLaneKey {
        CompactionLaneKey {
            sink_ref: sink_ref.to_string(),
            namespace: namespace.to_string(),
        }
    }

    fn plan_test_work(
        queue: &Arc<Mutex<VecDeque<TestWork>>>,
        slots: usize,
        blocked: &HashSet<CompactionLaneKey>,
    ) -> CompactionPlanBatch<TestWork> {
        let mut queue = queue.lock().unwrap();
        let mut works = Vec::with_capacity(slots);
        let mut deferred = VecDeque::new();
        while works.len() < slots {
            let Some(work) = queue.pop_front() else {
                break;
            };
            if blocked.contains(&work.lane) {
                deferred.push_back(work);
            } else {
                works.push(work);
            }
        }
        deferred.append(&mut queue);
        *queue = deferred;
        CompactionPlanBatch {
            works,
            ready_work_remaining: !queue.is_empty(),
        }
    }

    fn test_executor(
        state: Arc<TestExecutionState>,
    ) -> impl FnMut(TestWork) -> Pin<Box<dyn Future<Output = bool> + Send>> {
        move |work| {
            state.reservations.fetch_add(1, Ordering::SeqCst);
            let reservation = TestReservationGuard(state.clone());
            let state = state.clone();
            Box::pin(async move {
                let _reservation = reservation;
                state.started.lock().unwrap().push(work.id);
                let active = state.active.fetch_add(1, Ordering::SeqCst) + 1;
                state.max_active.fetch_max(active, Ordering::SeqCst);
                let _active = TestActiveGuard(state.clone());
                tokio_sleep(work.delay).await;
                state.completed.lock().unwrap().push(work.id);
                work.compacted
            })
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn straggler_does_not_leave_an_executable_slot_idle() {
        metrics_hot::reset_flush_metrics();
        let queue = Arc::new(Mutex::new(VecDeque::from([
            TestWork {
                id: "straggler",
                lane: lane("sink.a", "slow"),
                delay: TokioDuration::from_millis(80),
                compacted: true,
            },
            TestWork {
                id: "short-1",
                lane: lane("sink.b", "one"),
                delay: TokioDuration::from_millis(10),
                compacted: true,
            },
            TestWork {
                id: "short-2",
                lane: lane("sink.c", "two"),
                delay: TokioDuration::from_millis(10),
                compacted: true,
            },
        ])));
        let planner_queue = queue.clone();
        let state = Arc::new(TestExecutionState::default());

        let made_progress = drive_work_conserving(
            2,
            false,
            move |slots, _, _, blocked| plan_test_work(&planner_queue, slots, blocked),
            test_executor(state.clone()),
            None,
        )
        .await;

        let completed = state.completed.lock().unwrap();
        let short_2 = completed.iter().position(|id| *id == "short-2").unwrap();
        let straggler = completed.iter().position(|id| *id == "straggler").unwrap();
        assert!(made_progress);
        assert!(short_2 < straggler);
        assert_eq!(state.max_active.load(Ordering::SeqCst), 2);
        assert_eq!(state.reservations.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn scheduler_continuously_tops_up_after_completions() {
        metrics_hot::reset_flush_metrics();
        let queue = Arc::new(Mutex::new(
            (0..6)
                .map(|id| TestWork {
                    id: ["a", "b", "c", "d", "e", "f"][id],
                    lane: lane(&format!("sink.{id}"), "ns"),
                    delay: TokioDuration::from_millis(5),
                    compacted: true,
                })
                .collect::<VecDeque<_>>(),
        ));
        let planner_queue = queue.clone();
        let state = Arc::new(TestExecutionState::default());

        drive_work_conserving(
            2,
            false,
            move |slots, _, _, blocked| plan_test_work(&planner_queue, slots, blocked),
            test_executor(state.clone()),
            None,
        )
        .await;

        assert_eq!(state.completed.lock().unwrap().len(), 6);
        assert!(metrics_hot::COMPACTION_SCHEDULER_TOP_UPS_TOTAL.load(Ordering::Relaxed) >= 3);
        assert_eq!(
            metrics_hot::COMPACTION_ACTIVE_JOBS.load(Ordering::Relaxed),
            0
        );
        assert!(metrics_hot::compaction_active_by_sink_snapshot().is_empty());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn scheduler_yields_periodically_to_refresh_the_flush_budget() {
        metrics_hot::reset_flush_metrics();
        let ids = [
            "retune-0", "retune-1", "retune-2", "retune-3", "retune-4", "retune-5", "retune-6",
            "retune-7", "retune-8", "retune-9",
        ];
        let queue = Arc::new(Mutex::new(
            (0..10)
                .map(|id| TestWork {
                    id: ids[id],
                    lane: lane(&format!("sink.{id}"), "ns"),
                    delay: TokioDuration::from_millis(1),
                    compacted: true,
                })
                .collect::<VecDeque<_>>(),
        ));
        let planner_queue = queue.clone();
        let state = Arc::new(TestExecutionState::default());

        drive_work_conserving(
            1,
            true,
            move |slots, _, _, blocked| plan_test_work(&planner_queue, slots, blocked),
            test_executor(state.clone()),
            None,
        )
        .await;

        assert_eq!(state.completed.lock().unwrap().len(), 4);
        assert_eq!(queue.lock().unwrap().len(), 6);
        assert_eq!(state.reservations.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn failed_lane_releases_and_unrelated_work_continues() {
        metrics_hot::reset_flush_metrics();
        let failed_lane = lane("sink.failed", "ns");
        let queue = Arc::new(Mutex::new(VecDeque::from([
            TestWork {
                id: "fails",
                lane: failed_lane.clone(),
                delay: TokioDuration::from_millis(5),
                compacted: false,
            },
            TestWork {
                id: "slow-success",
                lane: lane("sink.ok", "slow"),
                delay: TokioDuration::from_millis(40),
                compacted: true,
            },
            TestWork {
                id: "same-failed-lane",
                lane: failed_lane,
                delay: TokioDuration::from_millis(1),
                compacted: true,
            },
            TestWork {
                id: "unrelated",
                lane: lane("sink.other", "ready"),
                delay: TokioDuration::from_millis(5),
                compacted: true,
            },
        ])));
        let planner_queue = queue.clone();
        let state = Arc::new(TestExecutionState::default());

        let made_progress = drive_work_conserving(
            2,
            false,
            move |slots, _, _, blocked| plan_test_work(&planner_queue, slots, blocked),
            test_executor(state.clone()),
            None,
        )
        .await;

        let started = state.started.lock().unwrap();
        assert!(made_progress);
        assert!(started.contains(&"unrelated"));
        assert!(!started.contains(&"same-failed-lane"));
        assert_eq!(state.reservations.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn cancellation_drains_active_jobs_without_leaking_reservations() {
        metrics_hot::reset_flush_metrics();
        let queue = Arc::new(Mutex::new(VecDeque::from([
            TestWork {
                id: "active-a",
                lane: lane("sink.a", "ns"),
                delay: TokioDuration::from_millis(30),
                compacted: true,
            },
            TestWork {
                id: "active-b",
                lane: lane("sink.b", "ns"),
                delay: TokioDuration::from_millis(30),
                compacted: true,
            },
            TestWork {
                id: "not-reserved",
                lane: lane("sink.c", "ns"),
                delay: TokioDuration::from_millis(1),
                compacted: true,
            },
        ])));
        let planner_queue = queue.clone();
        let state = Arc::new(TestExecutionState::default());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            tokio_sleep(TokioDuration::from_millis(5)).await;
            drop(tx);
        });
        let mut drain_reply = None;
        let mut stop_requested = false;
        let mut control = CompactionCycleControl {
            rx: &mut rx,
            drain_reply: &mut drain_reply,
            stop_requested: &mut stop_requested,
        };

        drive_work_conserving(
            2,
            false,
            move |slots, _, _, blocked| plan_test_work(&planner_queue, slots, blocked),
            test_executor(state.clone()),
            Some(&mut control),
        )
        .await;
        drop(control);

        assert!(stop_requested);
        assert_eq!(state.started.lock().unwrap().len(), 2);
        assert_eq!(state.completed.lock().unwrap().len(), 2);
        assert_eq!(state.reservations.load(Ordering::SeqCst), 0);
        assert_eq!(
            metrics_hot::COMPACTION_ACTIVE_JOBS.load(Ordering::Relaxed),
            0
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn drain_waits_active_jobs_and_leaves_no_reservations() {
        metrics_hot::reset_flush_metrics();
        let queue = Arc::new(Mutex::new(VecDeque::from([
            TestWork {
                id: "active-a",
                lane: lane("sink.a", "ns"),
                delay: TokioDuration::from_millis(20),
                compacted: true,
            },
            TestWork {
                id: "active-b",
                lane: lane("sink.b", "ns"),
                delay: TokioDuration::from_millis(20),
                compacted: true,
            },
            TestWork {
                id: "drain-top-up",
                lane: lane("sink.c", "ns"),
                delay: TokioDuration::from_millis(1),
                compacted: true,
            },
        ])));
        let planner_queue = queue.clone();
        let state = Arc::new(TestExecutionState::default());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx_for_drain = tx.clone();
        tokio::spawn(async move {
            tokio_sleep(TokioDuration::from_millis(5)).await;
            let (reply, _done) = tokio::sync::oneshot::channel();
            tx_for_drain
                .send(CompactorCommand::DrainAndStop(reply))
                .unwrap();
        });
        let mut drain_reply = None;
        let mut stop_requested = false;
        let mut control = CompactionCycleControl {
            rx: &mut rx,
            drain_reply: &mut drain_reply,
            stop_requested: &mut stop_requested,
        };

        drive_work_conserving(
            2,
            false,
            move |slots, _, _, blocked| plan_test_work(&planner_queue, slots, blocked),
            test_executor(state.clone()),
            Some(&mut control),
        )
        .await;
        drop(control);
        drop(tx);

        assert!(drain_reply.is_some());
        assert!(!stop_requested);
        assert_eq!(state.completed.lock().unwrap().len(), 3);
        assert_eq!(state.reservations.load(Ordering::SeqCst), 0);
        assert_eq!(
            metrics_hot::COMPACTION_ACTIVE_JOBS.load(Ordering::Relaxed),
            0
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn quiet_budget_runs_multiple_jobs_while_reduced_slots_preempt_admission() {
        metrics_hot::reset_flush_metrics();
        let queue = Arc::new(Mutex::new(
            (0..6)
                .map(|id| TestWork {
                    id: ["q0", "q1", "q2", "q3", "q4", "q5"][id],
                    lane: lane(&format!("sink.{id}"), "ns"),
                    delay: TokioDuration::from_millis(15),
                    compacted: true,
                })
                .collect::<VecDeque<_>>(),
        ));
        let planner_queue = queue.clone();
        let state = Arc::new(TestExecutionState::default());

        // Quiet ingest headroom: multiple grouped jobs run concurrently.
        drive_work_conserving(
            3,
            false,
            move |slots, _, _, blocked| plan_test_work(&planner_queue, slots, blocked),
            test_executor(state.clone()),
            None,
        )
        .await;
        assert_eq!(state.completed.lock().unwrap().len(), 6);
        assert_eq!(state.max_active.load(Ordering::SeqCst), 3);

        metrics_hot::reset_flush_metrics();
        let remaining = Arc::new(Mutex::new(
            (0..4)
                .map(|id| TestWork {
                    id: ["p0", "p1", "p2", "p3"][id],
                    lane: lane(&format!("preempt.{id}"), "ns"),
                    delay: TokioDuration::from_millis(20),
                    compacted: true,
                })
                .collect::<VecDeque<_>>(),
        ));
        let planner_remaining = remaining.clone();
        let preempted = Arc::new(TestExecutionState::default());

        // Ingest pressure shrinks the immutable cycle budget to the ingest-safe
        // cap; only that many jobs are admitted before the cycle retunes.
        drive_work_conserving(
            1,
            true,
            move |slots, _, _, blocked| plan_test_work(&planner_remaining, slots, blocked),
            test_executor(preempted.clone()),
            None,
        )
        .await;
        assert_eq!(preempted.max_active.load(Ordering::SeqCst), 1);
        assert_eq!(preempted.completed.lock().unwrap().len(), 4);
        assert!(remaining.lock().unwrap().is_empty());
    }

    #[test]
    fn no_work_keeps_periodic_backoff() {
        assert!(Buffers::compactor_idle_backoff() >= TokioDuration::from_millis(100));
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GroupedCompactionTestEvent {
    SinkSucceeded,
    ManifestAcked,
    SlicesTombstoned,
}

#[cfg(test)]
type GroupedCompactionTestObserver =
    Arc<dyn Fn(GroupedCompactionTestEvent) + Send + Sync + 'static>;

#[cfg(test)]
tokio::task_local! {
    static GROUPED_COMPACTION_TEST_OBSERVER: GroupedCompactionTestObserver;
}

#[cfg(test)]
fn observe_grouped_compaction_test_event(event: GroupedCompactionTestEvent) {
    let _ = GROUPED_COMPACTION_TEST_OBSERVER.try_with(|observer| observer(event));
}

struct CompactionPlannerMetricGuard {
    started: std::time::Instant,
    segments_examined: u64,
    slices_examined: u64,
}

impl CompactionPlannerMetricGuard {
    fn new() -> Self {
        Self {
            started: std::time::Instant::now(),
            segments_examined: 0,
            slices_examined: 0,
        }
    }
}

impl Drop for CompactionPlannerMetricGuard {
    fn drop(&mut self) {
        metrics_hot::record_compaction_planner_cycle(
            self.started.elapsed(),
            self.segments_examined,
            self.slices_examined,
        );
    }
}

struct GroupedStreamBuildMetricGuard {
    started: std::time::Instant,
    eager: bool,
}

impl GroupedStreamBuildMetricGuard {
    fn new(eager: bool) -> Self {
        Self {
            started: std::time::Instant::now(),
            eager,
        }
    }
}

impl Drop for GroupedStreamBuildMetricGuard {
    fn drop(&mut self) {
        metrics_hot::record_grouped_stream_build(self.started.elapsed(), self.eager);
    }
}

#[inline]
fn record_wal_write_metrics(rows: u64, bytes: u64) {
    metrics_hot::add_wal_write_bytes(bytes);
    metrics_hot::add_wal_write_rows(rows);
}

fn linux_vm_rss_kb() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
            for line in s.lines() {
                if let Some(rest) = line.strip_prefix("VmRSS:") {
                    if let Some(kb) = rest.trim().split_whitespace().next() {
                        return kb.parse().ok();
                    }
                }
            }
        }
    }
    None
}

fn wal_segment_cache_obs_stats() -> (usize, u64, usize) {
    let mut s3_entries = 0usize;
    let mut indexed_body_bytes = 0u64;
    let mut remote_meta_only = 0usize;
    for e in SEGMENT_CACHE.iter() {
        if let SegmentSource::S3 { body, .. } = &e.value().source {
            s3_entries += 1;
            if let Some(d) = body {
                indexed_body_bytes = indexed_body_bytes.saturating_add(d.len() as u64);
            } else {
                remote_meta_only += 1;
            }
        }
    }
    (s3_entries, indexed_body_bytes, remote_meta_only)
}

fn log_wal_s3_memory_obs(tag: &str) {
    if !Config::log_wal_enabled() {
        return;
    }
    let (n_s3, ix_bytes, remote_n) = wal_segment_cache_obs_stats();
    let rss = linux_vm_rss_kb()
        .map(|k| format!("vm_rss_kb={}", k))
        .unwrap_or_else(|| "vm_rss_kb=n/a".to_string());
    let (cap_b, cap_e) = s3_wal_body_cache::autotuned_caps();
    info!(
        "WAL mem obs tag={} {} segment_cache_s3_entries={} segment_cache_s3_indexed_body_bytes={} segment_cache_s3_meta_only_entries={} s3_wal_body_lru_bytes={} s3_wal_body_lru_entries={} s3_wal_body_lru_autotune_max_bytes={} s3_wal_body_lru_autotune_max_entries={}",
        tag,
        rss,
        n_s3,
        ix_bytes,
        remote_n,
        s3_wal_body_cache::lru_byte_count(),
        s3_wal_body_cache::lru_entry_count(),
        cap_b,
        cap_e,
    );
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
pub struct OffsetKeySerialize {
    pub(crate) source_namespace: String,
    pub(crate) source_partition: String,
    pub(crate) position: u64,
}

use crate::ingest::record_types::{NormalizedRecord, SourceRecord};

#[derive(Debug, Clone)]
pub struct IngestRecord {
    pub(crate) _namespace: String,
    pub(crate) _partition: String,
    pub(crate) _time: Option<i64>,
    pub(crate) source: SourceRecord,
    pub(crate) normalized: NormalizedRecord,
    pub(crate) _offset_pos: u64,
}

pub struct IngestBufferBatch {
    pub(crate) offsets: HashMap<OffsetKey, u64>,
    pub(crate) sink_ref: String,
    pub(crate) _namespace: String,
    pub(crate) _partition: String,
    pub(crate) _time: Option<i64>,
    pub(crate) _schema_fingerprint: String,
    pub(crate) schema: SchemaRef,
    pub(crate) record_batches: Option<Vec<RecordBatch>>,
    /// Per-row CDC metadata aligned 1:1 with the rows in `record_batches`.
    /// `None` means append-mode (no CDC metadata).
    pub(crate) cdc_rows: Option<Vec<crate::plugins::cdc::WalRowMeta>>,
    /// Checkpoint envelope to store when this batch's WAL segment is durably committed.
    pub(crate) checkpoint_update: Option<(String, crate::plugins::cdc::CheckpointEnvelope)>,
}

// Single global segment that aggregates batches for all partitions
struct GlobalSegment {
    batches: HashMap<PartitionKey, Vec<RecordBatch>>,
    offsets: HashMap<OffsetKey, u64>,
    cdc_meta: HashMap<PartitionKey, Vec<crate::plugins::cdc::WalRowMeta>>,
    checkpoint_updates: Vec<(String, crate::plugins::cdc::CheckpointEnvelope)>,
    bytes: u64,
    updated_at: SystemTime,
    flushed_at: SystemTime,
}

impl GlobalSegment {
    fn new() -> Self {
        let now = SystemTime::now();
        GlobalSegment {
            batches: HashMap::with_capacity(64),
            offsets: HashMap::new(),
            cdc_meta: HashMap::new(),
            checkpoint_updates: Vec::new(),
            bytes: 0,
            updated_at: now,
            flushed_at: now,
        }
    }
    fn add(
        &mut self,
        key: PartitionKey,
        batches: Vec<RecordBatch>,
        offsets: &HashMap<OffsetKey, u64>,
        cdc_rows: Option<Vec<crate::plugins::cdc::WalRowMeta>>,
        checkpoint_update: Option<(String, crate::plugins::cdc::CheckpointEnvelope)>,
    ) {
        let mut add_bytes: u64 = 0;
        for b in batches.iter() {
            add_bytes = add_bytes.saturating_add(b.get_array_memory_size() as u64);
        }
        self.batches
            .entry(key.clone())
            .or_insert_with(|| Vec::with_capacity(64))
            .extend(batches);
        if let Some(rows) = cdc_rows {
            self.cdc_meta
                .entry(key)
                .or_insert_with(|| Vec::with_capacity(64))
                .extend(rows);
        }
        for (k, v) in offsets.iter() {
            self.offsets
                .entry(k.clone())
                .and_modify(|p| *p = (*p).max(*v))
                .or_insert(*v);
        }
        if let Some(update) = checkpoint_update {
            self.checkpoint_updates.push(update);
        }
        self.bytes = self.bytes.saturating_add(add_bytes);
        self.updated_at = SystemTime::now();
    }
    fn take(
        &mut self,
    ) -> (
        HashMap<PartitionKey, Vec<RecordBatch>>,
        HashMap<OffsetKey, u64>,
        HashMap<PartitionKey, Vec<crate::plugins::cdc::WalRowMeta>>,
        Vec<(String, crate::plugins::cdc::CheckpointEnvelope)>,
        u64,
    ) {
        let batches = std::mem::take(&mut self.batches);
        let offsets = std::mem::take(&mut self.offsets);
        let cdc_meta = std::mem::take(&mut self.cdc_meta);
        let checkpoint_updates = std::mem::take(&mut self.checkpoint_updates);
        let bytes = std::mem::replace(&mut self.bytes, 0);
        self.updated_at = SystemTime::now();
        (batches, offsets, cdc_meta, checkpoint_updates, bytes)
    }

    /// Re-merge a failed live flush back into the accumulator so data is not lost.
    fn restore_after_failed_flush(
        &mut self,
        batches: HashMap<PartitionKey, Vec<RecordBatch>>,
        offsets: HashMap<OffsetKey, u64>,
        cdc_meta: HashMap<PartitionKey, Vec<crate::plugins::cdc::WalRowMeta>>,
        checkpoint_updates: Vec<(String, crate::plugins::cdc::CheckpointEnvelope)>,
        bytes: u64,
    ) {
        for (key, batch_vec) in batches {
            self.batches
                .entry(key.clone())
                .or_insert_with(|| Vec::with_capacity(64))
                .extend(batch_vec);
            if let Some(rows) = cdc_meta.get(&key) {
                self.cdc_meta
                    .entry(key)
                    .or_insert_with(|| Vec::with_capacity(64))
                    .extend(rows.iter().cloned());
            }
        }
        for (k, v) in offsets {
            self.offsets
                .entry(k)
                .and_modify(|p| *p = (*p).max(v))
                .or_insert(v);
        }
        self.checkpoint_updates.extend(checkpoint_updates);
        self.bytes = self.bytes.saturating_add(bytes);
        self.updated_at = SystemTime::now();
    }
}

fn serialize_cdc_meta_to_blobs(
    cdc_meta: &HashMap<PartitionKey, Vec<crate::plugins::cdc::WalRowMeta>>,
    batches: &HashMap<PartitionKey, Vec<RecordBatch>>,
) -> HashMap<PartitionKey, Vec<u8>> {
    use crate::plugins::cdc::WalPartMeta;
    let mut blobs: HashMap<PartitionKey, Vec<u8>> = HashMap::new();
    for (key, rows) in cdc_meta.iter() {
        if rows.is_empty() {
            continue;
        }
        let arrow_row_count = batches
            .get(key)
            .map(|partition_batches| {
                partition_batches
                    .iter()
                    .map(|batch| batch.num_rows() as u64)
                    .sum()
            })
            .unwrap_or(0);
        let meta = WalPartMeta::cdc(rows.clone(), arrow_row_count)
            .expect("CDC WAL metadata must align with Arrow rows before WAL serialization");
        let bytes = bincode::serialize(&meta)
            .expect("CDC WAL metadata must serialize before WAL segment write");
        blobs.insert(key.clone(), bytes);
    }
    blobs
}

fn schema_fingerprint(schema: &SchemaRef) -> String {
    let mut hasher = Sha256::new();
    for f in schema.fields().iter() {
        hasher.update(f.name().as_bytes());
        hasher.update(format!("{:?}", f.data_type()).as_bytes());
        hasher.update(&[if f.is_nullable() { 1 } else { 0 }]);
    }
    let digest = hasher.finalize();
    hex::encode(&digest[..8])
}

pub struct Buffers;

impl Buffers {
    fn segment_meta_to_store_meta(
        meta: &HashMap<PartitionKey, SegmentPartitionMeta>,
    ) -> HashMap<PartitionKey, (u64, SystemTime)> {
        meta.iter()
            .map(|(key, value)| (key.clone(), (value.bytes, value.updated_at)))
            .collect()
    }

    pub(crate) fn live_segment_bytes() -> u64 {
        SEGMENT_LIVE
            .lock()
            .map(|guard| guard.bytes)
            .unwrap_or_default()
    }

    pub(crate) fn append_batches_to_live(batches: Vec<IngestBufferBatch>) -> Result<u64, String> {
        let mut added_bytes = 0u64;
        for mut ingest_buffer_batch in batches.into_iter() {
            let sink_ref = ingest_buffer_batch.sink_ref.clone();
            let namespace = ingest_buffer_batch._namespace.clone();
            let partition = ingest_buffer_batch._partition.clone();
            let time = ingest_buffer_batch._time;
            let schema_fingerprint = if ingest_buffer_batch._schema_fingerprint.is_empty() {
                schema_fingerprint(&ingest_buffer_batch.schema)
            } else {
                ingest_buffer_batch._schema_fingerprint.clone()
            };
            let key = PartitionKey {
                sink_ref,
                namespace,
                partition,
                time,
                schema_fingerprint,
            };

            let mut batches_vec = ingest_buffer_batch
                .record_batches
                .take()
                .unwrap_or_default();
            if batches_vec.is_empty() {
                if Config::log_wal_enabled() {
                    debug!(
                        "WAL: skipped write ns={} part={} time={:?} (no record batches)",
                        ingest_buffer_batch._namespace,
                        ingest_buffer_batch._partition,
                        ingest_buffer_batch._time
                    );
                }
                continue;
            }
            let batch_bytes = batches_vec
                .iter()
                .map(|b| b.get_array_memory_size() as u64)
                .sum::<u64>();
            added_bytes = added_bytes.saturating_add(batch_bytes);
            let mut seg = SEGMENT_LIVE
                .lock()
                .map_err(|_| "WAL live segment lock poisoned".to_string())?;
            seg.add(
                key,
                batches_vec.drain(..).collect(),
                &ingest_buffer_batch.offsets,
                ingest_buffer_batch.cdc_rows.take(),
                ingest_buffer_batch.checkpoint_update.take(),
            );
        }
        Ok(added_bytes)
    }

    async fn persist_snapshot_to_wal(
        snapshot: &mut SegmentSnapshot,
        offsets_db: &Offsets,
        context: &str,
    ) -> Result<SegmentWriteResult, ArrowError> {
        let snapshot_id = snapshot.id.clone();
        let partitions_meta = Self::segment_meta_to_store_meta(&snapshot.meta);
        let store = crate::buffer::wal_store::WalStoreFactory::for_batches(&snapshot.batches);
        if Config::debug_enabled() || Config::log_wal_enabled() {
            let partition_sample: Vec<String> = snapshot
                .meta
                .iter()
                .take(3)
                .map(|(key, part_meta)| {
                    format!("{}/{}/{}B", key.namespace, key.partition, part_meta.bytes)
                })
                .collect();
            info!(
                "WAL persist start context={} id={} partitions={} offsets={} total_bytes={} partition_sample={:?} offset_sample={:?}",
                context,
                snapshot_id,
                snapshot.meta.len(),
                snapshot.offsets.len(),
                snapshot.total_bytes,
                partition_sample,
                offset_sample(snapshot.offsets.iter(), 3)
            );
            append_wal_debug_trace(&format!(
                "persist_start id={} context={} offsets={} bytes={}",
                snapshot_id,
                context,
                snapshot.offsets.len(),
                snapshot.total_bytes
            ));
        }
        let write_result = match store
            .write_snapshot_and_commit(
                &snapshot_id,
                &snapshot.offsets,
                &snapshot.batches,
                &partitions_meta,
                &snapshot.part_meta_blobs,
            )
            .await
        {
            Ok(result) => result,
            Err(err) => {
                error!(
                    "Segment write failed ({}): id={} err={}",
                    context, snapshot_id, err
                );
                return Err(ArrowError::ExternalError(Box::new(err)));
            }
        };

        if Config::debug_enabled() || Config::log_wal_enabled() {
            info!(
                "WAL persist write returned id={} rows={} bytes={} offsets={}",
                snapshot_id,
                write_result.total_rows,
                write_result.meta.total_bytes,
                snapshot.offsets.len()
            );
            append_wal_debug_trace(&format!(
                "persist_write_returned id={} rows={} bytes={}",
                snapshot_id, write_result.total_rows, write_result.meta.total_bytes
            ));
        }
        if Config::debug_enabled() || Config::log_wal_enabled() {
            info!(
                "WAL persist marking offsets durable id={} offset_sample={:?}",
                snapshot_id,
                offset_sample(snapshot.offsets.iter(), 3)
            );
            append_wal_debug_trace(&format!("persist_mark_offsets id={}", snapshot_id));
        }
        mark_offsets_durable_in_wal(offsets_db, snapshot.offsets.iter());
        for (key, envelope) in snapshot.checkpoint_updates.iter() {
            offsets_db
                .store_checkpoint_envelope(key, envelope)
                .map_err(|err| ArrowError::ExternalError(Box::new(std::io::Error::other(err))))?;
        }
        if Config::debug_enabled() || Config::log_wal_enabled() {
            info!("WAL persist offsets durable id={}", snapshot_id);
            append_wal_debug_trace(&format!("persist_offsets_durable id={}", snapshot_id));
        }
        Self::segment_cache_register_from_write(&write_result);
        if is_s3_wal() {
            log_wal_s3_memory_obs("after_persist_register");
        }
        if Config::debug_enabled() || Config::log_wal_enabled() {
            info!(
                "WAL persist cached id={} partitions={} offset_sample={:?}",
                snapshot_id,
                write_result.meta.num_partitions,
                offset_sample(snapshot.offsets.iter(), 3)
            );
            append_wal_debug_trace(&format!(
                "persist_cached id={} partitions={}",
                snapshot_id, write_result.meta.num_partitions
            ));
        }

        if Config::debug_enabled() || Config::log_wal_enabled() {
            let location_kind = match &write_result.location {
                crate::buffer::wal_store::SegmentWriteLocation::Disk { .. } => "disk",
                crate::buffer::wal_store::SegmentWriteLocation::S3 { .. } => "s3",
            };
            info!(
                "WAL persist committed id={} rows={} bytes={} location={}",
                snapshot_id, write_result.total_rows, write_result.meta.total_bytes, location_kind
            );
            append_wal_debug_trace(&format!(
                "persist_committed id={} rows={} bytes={} location={}",
                snapshot_id, write_result.total_rows, write_result.meta.total_bytes, location_kind
            ));
        }

        metrics_hot::record_wal_segment_closure(snapshot.created_at.elapsed().unwrap_or_default());
        Ok(write_result)
    }

    async fn flush_live_segment_to_wal(offsets_db: &Offsets) -> Result<(u64, u64), ArrowError> {
        let (to_flush_batches, to_flush_offsets, to_flush_cdc, to_flush_checkpoints, total_bytes) = {
            let mut guard = SEGMENT_LIVE.lock().unwrap();
            if guard.batches.is_empty() {
                return Ok((0, 0));
            }
            guard.flushed_at = SystemTime::now();
            let (b, o, c, cp, bytes) = guard.take();
            (b, o, c, cp, bytes)
        };

        let mut meta: HashMap<PartitionKey, SegmentPartitionMeta> = HashMap::new();
        for (key, batches) in to_flush_batches.iter() {
            let bytes_estimate = batches
                .iter()
                .map(|b| b.get_array_memory_size() as u64)
                .sum();
            meta.insert(
                key.clone(),
                SegmentPartitionMeta {
                    bytes: bytes_estimate,
                    updated_at: SystemTime::now(),
                },
            );
        }

        let part_meta_blobs = serialize_cdc_meta_to_blobs(&to_flush_cdc, &to_flush_batches);
        let mut snapshot = SegmentSnapshot::new(
            Helpers::random_str(16),
            to_flush_offsets,
            to_flush_batches,
            meta,
            total_bytes,
            part_meta_blobs,
            to_flush_checkpoints,
        );

        match Self::persist_snapshot_to_wal(&mut snapshot, offsets_db, "live flush").await {
            Ok(result) => Ok((result.total_rows, result.meta.total_bytes)),
            Err(e) => {
                let batches = std::mem::take(&mut snapshot.batches);
                let offsets = std::mem::take(&mut snapshot.offsets);
                let mut guard = SEGMENT_LIVE.lock().unwrap();
                guard.restore_after_failed_flush(
                    batches,
                    offsets,
                    to_flush_cdc,
                    std::mem::take(&mut snapshot.checkpoint_updates),
                    total_bytes,
                );
                Err(e)
            }
        }
    }

    pub(crate) async fn flush_live_segment_for_writer(
        offsets_db: &Offsets,
    ) -> Result<(u64, u64), ArrowError> {
        Self::flush_live_segment_to_wal(offsets_db).await
    }

    pub fn start_compactor_service(
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
        offsets_db: Arc<Offsets>,
    ) {
        use std::sync::atomic::Ordering as AO;
        if COMPACTOR_STARTED
            .compare_exchange(false, true, AO::Relaxed, AO::Relaxed)
            .is_err()
        {
            return;
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<CompactorCommand>();
        if let Ok(mut guard) = COMPACTOR_COMMAND_TX.lock() {
            *guard = Some(tx);
        }
        let handle = tokio::spawn(Self::run_compactor_service(rx, shared_output, offsets_db));
        if let Ok(mut guard) = COMPACTOR_HANDLE.lock() {
            *guard = Some(handle);
        }
    }

    pub fn wake_compactor() {
        let tx = {
            let guard = match COMPACTOR_COMMAND_TX.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            guard.clone()
        };
        if let Some(tx) = tx {
            let _ = tx.send(CompactorCommand::Wake);
        }
    }

    pub async fn drain_and_stop_compactor(offsets_db: Arc<Offsets>) -> bool {
        let _ = flush_all_segments(offsets_db).await;
        let tx = {
            let mut guard = match COMPACTOR_COMMAND_TX.lock() {
                Ok(g) => g,
                Err(_) => return false,
            };
            guard.take()
        };
        let Some(tx) = tx else {
            return true;
        };
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<bool>();
        if tx.send(CompactorCommand::DrainAndStop(done_tx)).is_err() {
            return false;
        }
        let drain_started = std::time::Instant::now();
        let mut last_progress_log = std::time::Instant::now();
        let drain_progress = CompactorDrainProgress::new();
        let mut drain_heartbeat = CompactorDrainHeartbeat::new();
        info!("Compactor drain: started waiting for grouped WAL compactions/uploads to finish");
        tokio::pin!(done_rx);
        let drain_result = loop {
            tokio::select! {
                res = &mut done_rx => break res,
                _ = tokio_sleep(TokioDuration::from_secs(1)) => {
                    let uploads_in_flight =
                        crate::metrics::counters::UPLOADS_IN_FLIGHT.load(AtomicOrdering::Relaxed);
                    let wal_in_flight =
                        crate::metrics::counters::WAL_COMPACTIONS_IN_FLIGHT.load(AtomicOrdering::Relaxed);
                    let reclaimable_wal = Self::has_reclaimable_wal();
                    let has_backlog =
                        wal_in_flight > 0 || uploads_in_flight > 0 || reclaimable_wal;
                    if last_progress_log.elapsed() >= std::time::Duration::from_secs(5) {
                        let interval = last_progress_log.elapsed();
                        let remaining_sample = Self::reclaimable_wal_partition_count(10_000);
                        let counters = WalCompactionCounterSnapshot::capture();
                        let since_drain = drain_progress.deltas_since_drain_start();
                        let since_last = drain_heartbeat.deltas_since_last_log();
                        let in_flight_summary = format_in_flight_grouped_compactions();
                        let status_summary = format_compactor_drain_status_summary(
                            drain_started.elapsed(),
                            interval,
                            counters,
                            since_last,
                            remaining_sample,
                            wal_in_flight,
                            uploads_in_flight,
                        );
                        info!("{status_summary}");
                        debug!(
                            "Compactor drain: waiting drain_elapsed={}s wal_in_flight={} uploads_in_flight={} reclaimable_partitions={} wal_parts_started={} wal_parts_completed={} wal_parts_since_drain=+{} wal_parts_since_last=+{} txn_started={} txn_completed={} txn_failed={} txn_since_drain=+{} txn_since_last=+{} active_grouped=[{}]",
                            drain_started.elapsed().as_secs(),
                            wal_in_flight,
                            uploads_in_flight,
                            remaining_sample,
                            counters.wal_parts_started,
                            counters.wal_parts_completed,
                            since_drain.wal_parts_completed,
                            since_last.wal_parts_completed,
                            counters.txn_started,
                            counters.txn_completed,
                            counters.txn_failed,
                            since_drain.txn_completed,
                            since_last.txn_completed,
                            in_flight_summary,
                        );
                        last_progress_log = std::time::Instant::now();
                    }
                    if has_backlog && Config::log_wal_enabled() {
                        debug!(
                            "Compactor drain: wal_in_flight={} uploads_in_flight={} reclaimable_wal={}",
                            wal_in_flight,
                            uploads_in_flight,
                            reclaimable_wal
                        );
                    }
                }
            }
        };
        match drain_result {
            Ok(true) => {
                let handle = {
                    let mut guard = match COMPACTOR_HANDLE.lock() {
                        Ok(g) => g,
                        Err(_) => return false,
                    };
                    guard.take()
                };
                if let Some(handle) = handle {
                    match handle.await {
                        Ok(()) => {
                            COMPACTOR_STARTED.store(false, std::sync::atomic::Ordering::Relaxed);
                            let since_drain = drain_progress.deltas_since_drain_start();
                            info!(
                                "Compactor drain: finished drain_elapsed={}s wal_parts_completed_since_drain=+{} txn_completed_since_drain=+{} txn_failed_since_drain=+{}",
                                drain_started.elapsed().as_secs(),
                                since_drain.wal_parts_completed,
                                since_drain.txn_completed,
                                since_drain.txn_failed,
                            );
                            true
                        }
                        Err(e) => {
                            warn!("Compactor task join error after drain: {e}");
                            false
                        }
                    }
                } else {
                    COMPACTOR_STARTED.store(false, std::sync::atomic::Ordering::Relaxed);
                    true
                }
            }
            Ok(false) | Err(_) => {
                let handle = {
                    let mut guard = match COMPACTOR_HANDLE.lock() {
                        Ok(g) => g,
                        Err(_) => return false,
                    };
                    guard.take()
                };
                if let Some(handle) = handle {
                    handle.abort();
                }
                COMPACTOR_STARTED.store(false, std::sync::atomic::Ordering::Relaxed);
                false
            }
        }
    }

    async fn run_compactor_service(
        mut rx: tokio::sync::mpsc::UnboundedReceiver<CompactorCommand>,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
        offsets_db: Arc<Offsets>,
    ) {
        let mut drain_reply: Option<tokio::sync::oneshot::Sender<bool>> = None;
        let mut consecutive_failures: u32 = 0;
        let mut stop_requested = false;
        loop {
            let force = drain_reply.is_some();
            let did_work = {
                let mut control = CompactionCycleControl {
                    rx: &mut rx,
                    drain_reply: &mut drain_reply,
                    stop_requested: &mut stop_requested,
                };
                Self::run_compaction_cycle(
                    force,
                    shared_output.clone(),
                    offsets_db.clone(),
                    Some(&mut control),
                )
                .await
            };
            if stop_requested {
                break;
            }
            let force = drain_reply.is_some();
            if force && !did_work {
                if let Some(reply) = drain_reply.take() {
                    let _ = reply.send(COMPACT_FAILURES.is_empty());
                }
                break;
            }

            if did_work {
                consecutive_failures = 0;
            } else if !COMPACT_FAILURES.is_empty() {
                consecutive_failures = consecutive_failures.saturating_add(1);
                let backoff_ms =
                    (500u64 * 2u64.saturating_pow(consecutive_failures.min(6))).min(30_000);
                debug!(
                    "Compactor: backoff {}ms after {} consecutive failure cycles",
                    backoff_ms, consecutive_failures
                );
                tokio_sleep(TokioDuration::from_millis(backoff_ms)).await;
            }

            if did_work {
                continue;
            }

            tokio::select! {
                cmd = rx.recv() => {
                    match cmd {
                        Some(CompactorCommand::Wake) => {}
                        Some(CompactorCommand::DrainAndStop(reply)) => {
                            drain_reply = Some(reply);
                        }
                        None => break,
                    }
                }
                _ = tokio_sleep(Self::compactor_idle_backoff()) => {}
            }
        }
        COMPACTOR_STARTED.store(false, std::sync::atomic::Ordering::Relaxed);
    }

    async fn run_compaction_cycle(
        force: bool,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
        _offsets_db: Arc<Offsets>,
        mut control: Option<&mut CompactionCycleControl<'_>>,
    ) -> bool {
        let ingest_paused = crate::data_dir_ingest_paused();
        let drain_requested = force;
        let force = force || ingest_paused;
        let mode = if ingest_paused {
            crate::ingest::tuner::FlushMode::Paused
        } else if drain_requested {
            crate::ingest::tuner::FlushMode::Drain
        } else {
            crate::ingest::tuner::FlushMode::Ingest
        };
        let ready_work = compaction_index().reclaimable_slice_count(
            force,
            Self::now_secs(),
            Self::sent_manifest_stale_secs(),
            usize::MAX,
        );
        metrics_hot::set_compaction_planner_ready_work_count(ready_work);
        let measured_backlog = force
            || ready_work > 0
            || crate::metrics::counters::WAL_COMPACTIONS_IN_FLIGHT
                .load(std::sync::atomic::Ordering::Relaxed)
                > 0
            || crate::metrics::counters::COMPACTION_ACTIVE_JOBS
                .load(std::sync::atomic::Ordering::Relaxed)
                > 0
            || crate::metrics::counters::COMPACTION_INFLIGHT_SLICE_COUNT
                .load(std::sync::atomic::Ordering::Relaxed)
                > 0;
        let snapshot = crate::ingest::tuner::update_flush_budget(mode, measured_backlog, None);
        let budget = Arc::new(FlushExecutionBudget::from_snapshot(snapshot));
        let concurrency = budget.scheduler_limit();

        let _output_capability = shared_output.capability();
        let currently_in_flight = crate::metrics::counters::WAL_COMPACTIONS_IN_FLIGHT
            .load(std::sync::atomic::Ordering::Relaxed);
        let executable_slots = concurrency.saturating_sub(currently_in_flight);
        if executable_slots == 0 {
            return false;
        }

        debug!(
            "Compactor: execution budget generation={} scheduler={} decode={} sink={} executable_slots={}",
            budget.generation(),
            budget.scheduler_limit(),
            budget.decode_limit(),
            budget.sink_limit(),
            executable_slots,
        );

        let planner_output = shared_output.clone();
        let planner_sink_limit = budget.sink_limit();
        let executor_output = shared_output.clone();
        let executor_budget = budget.clone();
        let made_progress = drive_work_conserving(
            executable_slots,
            force,
            move |slots, plan_force, active_by_lane, blocked_lanes| {
                let works = Self::next_compaction_transactions(
                    slots,
                    plan_force,
                    planner_output.as_ref().as_ref(),
                    active_by_lane,
                    blocked_lanes,
                    planner_sink_limit,
                );
                CompactionPlanBatch {
                    works,
                    ready_work_remaining: Self::has_ready_compaction_work(plan_force),
                }
            },
            move |work| {
                Self::compaction_job_future(work, executor_output.clone(), executor_budget.clone())
            },
            control.as_deref_mut(),
        )
        .await;

        let sweep_force = control
            .as_deref()
            .map(|control| control.force(force))
            .unwrap_or(force);
        if made_progress && !is_s3_wal() {
            Self::maybe_sweep_segment_cleanup(sweep_force);
        }
        made_progress
    }

    fn compactor_idle_backoff() -> TokioDuration {
        TokioDuration::from_millis(100)
    }

    fn compaction_job_future(
        work: CompactionWork,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
        budget: Arc<FlushExecutionBudget>,
    ) -> Pin<Box<dyn Future<Output = bool> + Send>> {
        let mut reservation = CompactionReservationGuard::new(&work);
        Box::pin(async move {
            let sink_ref = work.txn.sink_ref.clone();
            let namespace = work.txn.namespace.clone();
            let compaction_id = work.txn.id.clone();
            let target = work.txn.target_filename.clone();
            let wal_parts = work.entries.len();
            let timeout = Self::grouped_compaction_timeout();
            // From the first poll onward compact_grouped_work owns release through
            // its WorkGuard. Before that, this future owns the planner reservation.
            reservation.handoff();
            match tokio::time::timeout(
                timeout,
                Self::compact_grouped_work(work, shared_output, budget),
            )
            .await
            {
                Ok(Ok(compacted)) => compacted,
                Ok(Err(err)) => {
                    error!(
                        "Compactor: grouped compaction failed sink_ref={} namespace={} compaction_id={} target={} err={}",
                        sink_ref, namespace, compaction_id, target, err
                    );
                    false
                }
                Err(_) => {
                    let active = format_in_flight_grouped_compactions();
                    error!(
                        "Compactor: grouped compaction timed out sink_ref={} namespace={} compaction_id={} target={} wal_parts={} timeout_secs={} active_grouped=[{}]",
                        sink_ref,
                        namespace,
                        compaction_id,
                        target,
                        wal_parts,
                        timeout.as_secs(),
                        active,
                    );
                    COMPACT_FAILURES.insert(format!("{}:{}", sink_ref, compaction_id), 1);
                    crate::metrics::counters::add_wal_compaction_transaction_failed(1);
                    false
                }
            }
        })
    }

    fn has_ready_compaction_work(force: bool) -> bool {
        Self::ensure_manifest_index_loaded();
        compaction_index().reclaimable_slice_count(
            force,
            Self::now_secs(),
            Self::sent_manifest_stale_secs(),
            1,
        ) > 0
    }

    fn grouped_compaction_timeout() -> TokioDuration {
        let secs = Config::getenv("WAL_GROUPED_COMPACTION_TIMEOUT_SECS", "")
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .or_else(|| {
                Config::getenv("RUNTIME_GROUPED_COMPACTION_TIMEOUT_SECS", "")
                    .parse::<u64>()
                    .ok()
                    .filter(|value| *value > 0)
            })
            .unwrap_or(900);
        TokioDuration::from_secs(secs)
    }

    pub fn has_reclaimable_wal() -> bool {
        Self::reclaimable_wal_partition_count(1) > 0
    }

    pub(crate) fn reclaimable_wal_partition_count(limit: usize) -> usize {
        Self::ensure_manifest_index_loaded();
        compaction_index().reclaimable_slice_count(
            true,
            Self::now_secs(),
            Self::sent_manifest_stale_secs(),
            limit,
        )
    }

    fn grouped_write_semantics(capability: &SinkCapability) -> SinkWriteSemantics {
        match capability.retry_semantics {
            SinkRetrySemantics::TransactionalIdempotent
            | SinkRetrySemantics::FinalStateIdempotent => SinkWriteSemantics::ExactOnce,
            SinkRetrySemantics::DeterministicOverwrite => SinkWriteSemantics::IdempotentAtLeastOnce,
            SinkRetrySemantics::AtLeastOnce | SinkRetrySemantics::NonRetryable => {
                SinkWriteSemantics::AtLeastOnce
            }
        }
    }

    // ── Segment cache: single write / single remove / unified read ──────

    fn segment_cache_register(source: SegmentSource, meta: SegmentFileMetadata) {
        let id = source.segment_id().to_string();
        let source_id = source.display_name();
        let source_descriptor = Self::source_descriptor(&source);
        let ledger = Self::completion_ledger();
        let mut indexed_slices = Vec::with_capacity(meta.index.len());
        for (ordinal, idx) in meta.index.iter().enumerate() {
            match ledger.is_complete(&id, &meta.index, ordinal) {
                Ok(true) => continue,
                Ok(false) => {}
                Err(err) => {
                    warn!(
                        "Compactor: refusing to index segment {} with unreadable completion ledger: {}",
                        source.display_name(),
                        err
                    );
                    indexed_slices.clear();
                    break;
                }
            }
            let schema_fingerprint = Self::schema_fingerprint_for_group(idx, &meta);
            let wal_ref = WalPartRef {
                segment_id: id.clone(),
                source: source_descriptor.clone(),
                start: idx.start,
                len: idx.len,
                key: idx.key.clone(),
                cdc_meta_hash: idx.part_meta_summary.canonical_hash(),
            };
            indexed_slices.push(IndexedCompactionSlice {
                id: crate::buffer::compaction_index::CompactionSliceId {
                    segment_id: id.clone(),
                    source_id: source_id.clone(),
                    start: idx.start,
                    len: idx.len,
                },
                group_key: Self::grouping_key(idx, &schema_fingerprint),
                index_entry: idx.clone(),
                wal_ref,
            });
        }
        SEGMENT_CACHE.insert(id.clone(), CachedSegment { source, meta });
        let mut index = compaction_index();
        index.register_segment(
            &id,
            indexed_slices,
            Config::get_pipeline_buffer_threshold_bytes(),
            Config::get_pipeline_buffer_threshold_seconds() as u64,
            Self::now_secs(),
        );
        metrics_hot::set_compaction_planner_ready_work_count(index.ready_queue_depth());
    }

    fn segment_cache_remove(segment_id: &str) {
        SEGMENT_CACHE.remove(segment_id);
        let mut index = compaction_index();
        index.remove_segment(segment_id);
        metrics_hot::set_compaction_planner_ready_work_count(index.ready_queue_depth());
        s3_wal_body_cache::remove(segment_id);
    }

    fn segment_cache_register_from_write(result: &SegmentWriteResult) {
        let source = match &result.location {
            SegmentWriteLocation::Disk { path } => SegmentSource::Disk(path.clone()),
            SegmentWriteLocation::S3 { key, bucket } => SegmentSource::S3 {
                key: key.clone(),
                bucket: bucket.clone(),
                body: None,
            },
        };
        Self::segment_cache_register(source, result.meta.clone());
    }

    fn patch_manifest_semantics(
        mut txn: CompactionTransaction,
        output: &dyn DataSink,
    ) -> Option<CompactionTransaction> {
        let cap = output.capability_for_sink_ref(&txn.sink_ref)?;
        let expected = Self::grouped_write_semantics(cap);
        if txn.semantics != expected {
            txn.semantics = expected;
        }
        Some(txn)
    }

    fn source_descriptor(source: &SegmentSource) -> SegmentSourceDescriptor {
        match source {
            SegmentSource::Disk(path) => SegmentSourceDescriptor::Disk { path: path.clone() },
            SegmentSource::S3 { bucket, key, .. } => SegmentSourceDescriptor::S3 {
                bucket: bucket.clone(),
                key: key.clone(),
            },
        }
    }

    fn schema_fingerprint_for_meta(meta: &SegmentFileMetadata) -> String {
        let mut hasher = Sha256::new();
        for idx in meta.index.iter() {
            hasher.update(idx.key.namespace.as_bytes());
            hasher.update([0]);
            hasher.update(idx.key.schema_fingerprint.as_bytes());
            hasher.update([0]);
        }
        hex::encode(hasher.finalize())
    }

    fn grouping_key(
        idx: &SegmentPartitionIndexEntry,
        schema_fingerprint: &str,
    ) -> CompactionGroupKey {
        let kind = if idx.part_meta_summary.is_cdc() {
            CompactionKind::Cdc
        } else {
            CompactionKind::Append
        };
        CompactionGroupKey {
            sink_ref: idx.key.sink_ref.clone(),
            namespace: idx.key.namespace.clone(),
            partition: idx.key.partition.clone(),
            time: idx.key.time,
            schema_fingerprint: schema_fingerprint.to_string(),
            kind,
        }
    }

    fn schema_fingerprint_for_group(
        idx: &SegmentPartitionIndexEntry,
        meta: &SegmentFileMetadata,
    ) -> String {
        if idx.key.schema_fingerprint.is_empty() {
            Self::schema_fingerprint_for_meta(meta)
        } else {
            idx.key.schema_fingerprint.clone()
        }
    }

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    fn sent_manifest_stale_secs() -> u64 {
        Config::getenv("WAL_COMPACTION_SENT_STALE_SECS", "300")
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .unwrap_or(300)
    }

    fn ensure_manifest_index_loaded() {
        if compaction_index().manifests_loaded() {
            return;
        }
        match load_manifest_index() {
            Ok(manifests) => {
                let mut index = compaction_index();
                if !index.manifests_loaded() {
                    index.install_manifests(manifests);
                    metrics_hot::set_compaction_planner_ready_work_count(index.ready_queue_depth());
                }
            }
            Err(err) => warn!(
                "Compactor: failed to load pending transaction manifests: {}",
                err
            ),
        }
    }

    fn persist_compaction_manifest(txn: &CompactionTransaction) -> io::Result<()> {
        persist_manifest(txn)?;
        let mut index = compaction_index();
        index.upsert_manifest(txn.clone());
        metrics_hot::set_compaction_planner_ready_work_count(index.ready_queue_depth());
        Ok(())
    }

    fn remove_compaction_manifest(id: &str) -> io::Result<()> {
        remove_manifest(id)?;
        let mut index = compaction_index();
        index.remove_manifest(id, Self::now_secs());
        metrics_hot::set_compaction_planner_ready_work_count(index.ready_queue_depth());
        Ok(())
    }

    fn next_compaction_transactions(
        limit: usize,
        force: bool,
        output: &dyn DataSink,
        active_by_lane: &HashMap<CompactionLaneKey, usize>,
        blocked_lanes: &HashSet<CompactionLaneKey>,
        per_sink_limit: usize,
    ) -> Vec<CompactionWork> {
        let mut planner_metrics = CompactionPlannerMetricGuard::new();
        let now_secs = Self::now_secs();
        let target_bytes = Self::compaction_group_target_bytes();
        let max_parts = Self::compaction_group_max_parts();
        let sent_stale_secs = Self::sent_manifest_stale_secs();
        let per_sink_limit = per_sink_limit.max(1);
        let mut out = Vec::with_capacity(limit);

        Self::ensure_manifest_index_loaded();
        let pending = compaction_index().reserve_ready_manifests(
            limit,
            per_sink_limit,
            now_secs,
            sent_stale_secs,
            active_by_lane,
            blocked_lanes,
        );
        for reserved in pending {
            let txn = reserved.txn;
            if reserved.retried_stale {
                warn!(
                    "Compactor: retrying stale sent manifest id={} sink_ref={} namespace={} age_secs={} attempts={}",
                    txn.id,
                    txn.sink_ref,
                    txn.namespace,
                    reserved.stale_age_secs,
                    txn.attempts,
                );
            }
            planner_metrics.slices_examined = planner_metrics
                .slices_examined
                .saturating_add(txn.refs.len() as u64);
            planner_metrics.segments_examined = planner_metrics.segments_examined.saturating_add(
                txn.refs
                    .iter()
                    .map(|wal_ref| wal_ref.segment_id.as_str())
                    .collect::<HashSet<_>>()
                    .len() as u64,
            );
            let txn_id = txn.id.clone();
            if let Some(work) = Self::work_from_manifest(txn, output) {
                out.push(work);
            } else {
                compaction_index().release_manifest(&txn_id);
            }
        }
        if !out.is_empty() {
            let ready_work = compaction_index().reclaimable_slice_count(
                force,
                now_secs,
                sent_stale_secs,
                usize::MAX,
            );
            metrics_hot::set_compaction_planner_ready_work_count(ready_work);
            return out;
        }

        let plan = compaction_index().plan_ready(
            limit,
            force,
            now_secs,
            Config::get_pipeline_buffer_threshold_bytes(),
            Config::get_pipeline_buffer_threshold_seconds() as u64,
            target_bytes,
            max_parts,
            per_sink_limit,
            active_by_lane.clone(),
            blocked_lanes.clone(),
        );
        planner_metrics.segments_examined = planner_metrics
            .segments_examined
            .saturating_add(plan.segments_examined);
        planner_metrics.slices_examined = planner_metrics
            .slices_examined
            .saturating_add(plan.slices_examined);
        if plan.oversized_slices_deferred > 0 {
            warn!(
                "Compactor: deferred {} indivisible WAL slices larger than WAL_COMPACTION_GROUP_TARGET_BYTES={}",
                plan.oversized_slices_deferred, target_bytes
            );
        }
        for planned in plan.groups {
            let refs = planned
                .slices
                .iter()
                .map(|slice| slice.wal_ref.clone())
                .collect::<Vec<_>>();
            if let Some(work) = Self::work_from_planned_group(planned, output) {
                out.push(work);
            } else {
                compaction_index().release_refs(&refs, now_secs);
            }
        }
        let ready_work = compaction_index().reclaimable_slice_count(
            force,
            now_secs,
            sent_stale_secs,
            usize::MAX,
        );
        metrics_hot::set_compaction_planner_ready_work_count(ready_work);
        out
    }

    fn compaction_group_target_bytes() -> u64 {
        Config::getenv(
            "WAL_COMPACTION_GROUP_TARGET_BYTES",
            &DEFAULT_COMPACTION_GROUP_TARGET_BYTES.to_string(),
        )
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_COMPACTION_GROUP_TARGET_BYTES)
    }

    fn compaction_group_max_parts() -> usize {
        Config::getenv(
            "WAL_COMPACTION_GROUP_MAX_PARTS",
            &DEFAULT_COMPACTION_GROUP_MAX_PARTS.to_string(),
        )
        .parse::<usize>()
        .ok()
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_COMPACTION_GROUP_MAX_PARTS)
    }

    fn work_from_planned_group(
        planned: PlannedCompactionGroup,
        output: &dyn DataSink,
    ) -> Option<CompactionWork> {
        let schema_fingerprint = planned.key.schema_fingerprint.clone();
        let mut entries = Vec::with_capacity(planned.slices.len());
        for slice in planned.slices {
            let cached = SEGMENT_CACHE.get(&slice.id.segment_id)?;
            if cached.source.display_name() != slice.id.source_id {
                return None;
            }
            let ordinal = slice.index_entry.slice_ordinal as usize;
            entries.push(CompactionEntry {
                source: cached.source.clone(),
                meta: cached.meta.clone(),
                ordinal,
                idx: slice.index_entry,
                wal_ref: slice.wal_ref,
            });
        }
        Self::build_compaction_work(entries, schema_fingerprint, output)
    }

    fn work_from_manifest(
        txn: CompactionTransaction,
        output: &dyn DataSink,
    ) -> Option<CompactionWork> {
        if Self::manifest_refs_in_flight(&txn) {
            return None;
        }
        let sink_ref = txn.sink_ref.clone();
        let txn_id = txn.id.clone();
        let txn = match Self::patch_manifest_semantics(txn, output) {
            Some(txn) => txn,
            None => {
                warn!(
                    "Compactor: manifest resume skipped unknown sink_ref={}",
                    sink_ref
                );
                let _ = Self::remove_compaction_manifest(&txn_id);
                return None;
            }
        };
        let mut entries = Vec::with_capacity(txn.refs.len());
        let mut completed_refs = Vec::new();
        for wal_ref in txn.refs.iter() {
            let cached = SEGMENT_CACHE.get(&wal_ref.segment_id)?;
            let (ordinal, idx) = cached.meta.index.iter().enumerate().find(|(_, idx)| {
                idx.start == wal_ref.start && idx.len == wal_ref.len && idx.key == wal_ref.key
            })?;
            match Self::completion_ledger().is_complete(
                cached.source.segment_id(),
                &cached.meta.index,
                ordinal,
            ) {
                Ok(true) => {
                    completed_refs.push(wal_ref.clone());
                    continue;
                }
                Ok(false) => {}
                Err(err) => {
                    warn!(
                        "Compactor: retaining manifest {} because completion ledger is unreadable for seg={}: {}",
                        txn.id,
                        cached.source.display_name(),
                        err
                    );
                    return None;
                }
            }
            if let Some(expected_hash) = wal_ref.cdc_meta_hash {
                if Some(expected_hash) != idx.part_meta_summary.canonical_hash() {
                    warn!(
                        "Compactor: manifest resume skipped metadata hash mismatch segment_id={} slice_ordinal={}",
                        wal_ref.segment_id,
                        idx.slice_ordinal
                    );
                    return None;
                }
            }
            entries.push(CompactionEntry {
                source: cached.source.clone(),
                meta: cached.meta.clone(),
                ordinal,
                idx: idx.clone(),
                wal_ref: wal_ref.clone(),
            });
        }
        if !completed_refs.is_empty() {
            compaction_index().complete_refs(&completed_refs);
        }
        if entries.is_empty() {
            let _ = Self::remove_compaction_manifest(&txn.id);
            return None;
        }
        Some(CompactionWork { txn, entries })
    }

    fn manifest_refs_in_flight(txn: &CompactionTransaction) -> bool {
        txn.refs.iter().any(|wal_ref| {
            COMPACTION_IN_FLIGHT.contains_key(&(
                wal_ref.source.stable_id(),
                wal_ref.start,
                wal_ref.len,
            ))
        })
    }

    fn build_compaction_work(
        entries: Vec<CompactionEntry>,
        schema_fingerprint_hint: String,
        output: &dyn DataSink,
    ) -> Option<CompactionWork> {
        let first = entries.first()?;
        let sink_ref = first.idx.key.sink_ref.clone();
        let cap = match output.capability_for_sink_ref(&sink_ref) {
            Some(cap) => cap,
            None => {
                warn!("Compactor: no sink registered for sink_ref={}", sink_ref);
                return None;
            }
        };
        let namespace = first.idx.key.namespace.clone();
        let write_policy = crate::plugins::source_contract::namespace_source_contract(&namespace)
            .map(|contract| contract.write_policy)
            .unwrap_or(crate::plugins::source_contract::WritePolicy::Append);
        let schema_fingerprint = if schema_fingerprint_hint.is_empty() {
            Self::schema_fingerprint_for_group(&first.idx, &first.meta)
        } else {
            schema_fingerprint_hint
        };
        let mut out_key = BufferChunker::encode_chunk_name(
            "output",
            Some(&sink_ref),
            Some(&namespace),
            Some(&first.idx.key.partition),
            first.idx.key.time,
            Some(&first.idx.key.schema_fingerprint),
        );
        let refs = entries
            .iter()
            .map(|entry| entry.wal_ref.clone())
            .collect::<Vec<_>>();
        let txn = CompactionTransaction::new(
            sink_ref,
            namespace,
            schema_fingerprint,
            write_policy,
            Self::grouped_write_semantics(cap),
            refs,
            String::new(),
        );
        out_key = format!("{}-c={}", out_key, txn.id);
        let txn = CompactionTransaction {
            target_filename: out_key,
            ..txn
        };
        Some(CompactionWork { txn, entries })
    }

    fn is_truncated_stream_error(err: &str) -> bool {
        err.contains("failed to fill whole buffer") || err.contains("UnexpectedEof")
    }

    fn quarantine_truncated_disk_segment(seg_path: &Path, diag: &str, seg_display: &str) {
        let qdir = PathBuf::from(format!(
            "{}/segment_buffer/quarantine",
            Config::get_data_dir()
        ));
        if let Err(err) = fs::create_dir_all(&qdir) {
            error!(
                "Compactor: failed to create quarantine dir {}: {}",
                qdir.to_string_lossy(),
                err
            );
            return;
        }
        let ts = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let base = seg_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");
        let qseg = qdir.join(format!("{}.{}.seg", base, ts));
        let qdiag = qdir.join(format!("{}.{}.diag.txt", base, ts));
        if !qseg.exists() {
            if let Err(err) = fs::copy(seg_path, &qseg) {
                error!(
                    "Compactor: quarantine copy failed seg={} to={} err={}",
                    seg_display,
                    qseg.to_string_lossy(),
                    err
                );
            }
        }
        if let Err(err) = fs::write(&qdiag, diag.as_bytes()) {
            error!(
                "Compactor: quarantine diag write failed seg={} path={} err={}",
                seg_display,
                qdiag.to_string_lossy(),
                err
            );
        }
        warn!("Compactor: truncated-stream quarantine seg={}", seg_display);
        crate::metrics::counters::add_quarantined_partitions(1);
    }

    fn quarantine_truncated_entries(entries: &[CompactionEntry], diag_prefix: &str, err: &str) {
        if !Self::is_truncated_stream_error(err) {
            return;
        }
        let mut quarantined = HashSet::new();
        for entry in entries {
            let SegmentSource::Disk(seg_path) = &entry.source else {
                continue;
            };
            let seg_display = entry.source.display_name();
            if !quarantined.insert(seg_display.clone()) {
                continue;
            }
            let file_len = entry.source.logical_byte_len(&entry.meta).unwrap_or(0);
            let diag = format!(
                "{diag_prefix} seg={} file_len={} start={} idx_len={} key={:?} error={}",
                seg_display, file_len, entry.idx.start, entry.idx.len, entry.idx.key, err
            );
            Self::quarantine_truncated_disk_segment(seg_path, &diag, &seg_display);
        }
    }

    fn tombstone_dir() -> PathBuf {
        PathBuf::from(format!("{}/segment_buffer/done", Config::get_data_dir()))
    }

    fn completion_ledger() -> SegmentCompletionLedger {
        SegmentCompletionLedger::new(Self::tombstone_dir())
    }

    #[cfg(test)]
    fn tombstone_path_for_id(segment_id: &str, key: &PartitionKey) -> PathBuf {
        Self::completion_ledger().legacy_tombstone_path(segment_id, key)
    }

    #[cfg(test)]
    fn tombstone_path_for_source(source: &SegmentSource, key: &PartitionKey) -> PathBuf {
        Self::tombstone_path_for_id(source.segment_id(), key)
    }

    fn remove_disk_segment_and_commit_marker(seg_path: &Path) -> io::Result<DiskSegmentCleanup> {
        let cleanup = match fs::remove_file(seg_path) {
            Ok(_) => DiskSegmentCleanup::Removed,
            Err(err) if err.kind() == io::ErrorKind::NotFound => DiskSegmentCleanup::AlreadyMissing,
            Err(err) => return Err(err),
        };
        let commit_path = seg_path.with_extension("seg.commit");
        if let Err(err) = fs::remove_file(&commit_path) {
            if err.kind() != io::ErrorKind::NotFound {
                warn!(
                    "Failed to remove commit marker {}: {}",
                    commit_path.to_string_lossy(),
                    err
                );
            }
        }
        Ok(cleanup)
    }

    #[cfg(not(windows))]
    fn fsync_dir(dir: &PathBuf) -> io::Result<()> {
        let df = File::open(dir)?;
        df.sync_all()
    }

    #[cfg(windows)]
    fn fsync_dir(_dir: &PathBuf) -> io::Result<()> {
        // Windows can return ERROR_ACCESS_DENIED when flushing directory
        // handles. The commit marker file itself is fsynced before this call.
        Ok(())
    }

    pub fn write_seg_commit(
        seg_path: &PathBuf,
        sha256: &[u8; 32],
        parts_count: u32,
        total_bytes: u64,
    ) -> io::Result<()> {
        // Zero-copy binary commit header: MAGIC("SEGC"), VERSION(u32 LE), created_at(u64 LE), size(u64 LE), parts(u32 LE), sha256([u8;32])
        let commit_path = seg_path.with_extension("seg.commit");
        let buf = crate::buffer::segment_file::SegmentFile::build_commit_header_bytes(
            parts_count,
            total_bytes,
            sha256,
        );
        let mut commit_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&commit_path)?;
        commit_file.write_all(&buf)?;
        commit_file.sync_all()?;
        if let Some(parent) = commit_path.parent() {
            Self::fsync_dir(&parent.to_path_buf())?;
        }
        Ok(())
    }

    pub fn read_seg_commit(
        seg_path: &PathBuf,
    ) -> io::Result<(
        u32,      /*version*/
        u64,      /*created_at*/
        u64,      /*size*/
        u32,      /*parts*/
        [u8; 32], /*sha*/
    )> {
        let commit_path = seg_path.with_extension("seg.commit");
        let mut f = File::open(&commit_path)?;
        let mut buf = [0u8; 60];
        f.read_exact(&mut buf)?;
        if &buf[0..4] != b"SEGC" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad commit magic",
            ));
        }
        let mut sha = [0u8; 32];
        sha.copy_from_slice(&buf[28..60]);
        Ok((
            u32::from_le_bytes(buf[4..8].try_into().unwrap()),
            u64::from_le_bytes(buf[8..16].try_into().unwrap()),
            u64::from_le_bytes(buf[16..24].try_into().unwrap()),
            u32::from_le_bytes(buf[24..28].try_into().unwrap()),
            sha,
        ))
    }

    /// Best-effort cleanup for `.seg.commit` files that no longer have a sibling `.seg`.
    /// Returns (scanned_commit_markers, removed_orphans, errors).
    pub fn cleanup_orphan_seg_commits(max_scan: usize) -> (usize, usize, usize) {
        if max_scan == 0 {
            return (0, 0, 0);
        }
        let seg_dir = PathBuf::from(format!("{}/segment_buffer/segs", Config::get_data_dir()));
        if !seg_dir.exists() {
            return (0, 0, 0);
        }

        let mut scanned = 0usize;
        let mut removed = 0usize;
        let mut errors = 0usize;

        let entries = match fs::read_dir(&seg_dir) {
            Ok(rd) => rd,
            Err(e) => {
                warn!(
                    "Orphan commit cleanup: failed to read dir {}: {}",
                    seg_dir.to_string_lossy(),
                    e
                );
                return (0, 0, 1);
            }
        };

        for entry in entries.flatten() {
            if scanned >= max_scan {
                break;
            }
            let path = entry.path();
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if !name.ends_with(".seg.commit") {
                continue;
            }
            scanned = scanned.saturating_add(1);

            // `foo.seg.commit` -> `foo.seg`
            let seg_path = path.with_extension("");
            if seg_path.exists() {
                continue;
            }

            match fs::remove_file(&path) {
                Ok(_) => {
                    removed = removed.saturating_add(1);
                }
                Err(e) => {
                    if e.kind() != io::ErrorKind::NotFound {
                        errors = errors.saturating_add(1);
                        warn!(
                            "Orphan commit cleanup: failed removing {}: {}",
                            path.to_string_lossy(),
                            e
                        );
                    }
                }
            }
        }

        (scanned, removed, errors)
    }

    fn maybe_sweep_segment_cleanup(force_full: bool) {
        let cycle = SWEEP_CYCLE_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
        if force_full || cycle % 60 == 0 {
            let _ = Self::sweep_segment_cleanup();
        } else {
            let _ = Self::sweep_segment_cleanup_cached();
        }
    }

    fn segment_files_bytes(seg_path: &Path) -> u64 {
        let seg_bytes = seg_path.metadata().map(|meta| meta.len()).unwrap_or(0);
        let commit_bytes = seg_path
            .with_extension("seg.commit")
            .metadata()
            .map(|meta| meta.len())
            .unwrap_or(0);
        seg_bytes.saturating_add(commit_bytes)
    }

    fn sweep_segment_cleanup_cached() -> WalSweepResult {
        let mut result = WalSweepResult::default();
        for entry in SEGMENT_CACHE.iter() {
            let cached = entry.value();
            let seg_path = match &cached.source {
                SegmentSource::Disk(path) => path.clone(),
                SegmentSource::S3 { .. } => continue,
            };
            let commit = seg_path.with_extension("seg.commit");
            if !commit.exists() {
                continue;
            }
            match Self::completion_ledger()
                .all_complete(cached.source.segment_id(), &cached.meta.index)
            {
                Ok(true) => {
                    Self::remove_fully_compacted_disk_segment(&seg_path, &cached.meta, &mut result);
                }
                Ok(false) => {}
                Err(err) => {
                    result.errors = result.errors.saturating_add(1);
                    warn!(
                        "WAL sweep: retaining segment {} with unreadable completion ledger: {}",
                        seg_path.to_string_lossy(),
                        err
                    );
                }
            }
        }
        result
    }

    fn remove_fully_compacted_disk_segment(
        seg_path: &PathBuf,
        meta: &SegmentFileMetadata,
        sweep: &mut WalSweepResult,
    ) {
        let reclaim_bytes = Self::segment_files_bytes(seg_path);
        match Self::remove_disk_segment_and_commit_marker(seg_path) {
            Ok(cleanup) => {
                if matches!(cleanup, DiskSegmentCleanup::Removed) {
                    sweep.segments_deleted = sweep.segments_deleted.saturating_add(1);
                    sweep.bytes_reclaimed = sweep.bytes_reclaimed.saturating_add(reclaim_bytes);
                    crate::metrics::counters::record_wal_segment_reclaimed(reclaim_bytes);
                    info!(
                        "Removed fully-compacted segment {}",
                        seg_path.to_string_lossy()
                    );
                }
                if let Some(id) = seg_path.file_stem().and_then(|s| s.to_str()) {
                    Self::segment_cache_remove(id);
                    if let Err(e) = Self::completion_ledger().remove_segment(id, &meta.index) {
                        error!(
                            "Failed to remove completion state for segment {}: {}",
                            id, e
                        );
                        sweep.errors = sweep.errors.saturating_add(1);
                    }
                }
            }
            Err(e) => {
                sweep.errors = sweep.errors.saturating_add(1);
                error!(
                    "Failed to remove fully-compacted segment {}: {}",
                    seg_path.to_string_lossy(),
                    e
                );
            }
        }
    }

    fn sweep_segment_cleanup() -> WalSweepResult {
        let mut result = WalSweepResult::default();
        let seg_dir = PathBuf::from(format!("{}/segment_buffer/segs", Config::get_data_dir()));
        if !seg_dir.exists() {
            return result;
        }
        let dir_iter = match fs::read_dir(&seg_dir) {
            Ok(r) => r,
            Err(_) => return result,
        };
        for entry in dir_iter {
            if let Ok(ent) = entry {
                let p = ent.path();
                if p.extension().and_then(|s| s.to_str()) != Some("seg") {
                    continue;
                }
                let commit = p.with_extension("seg.commit");
                if !commit.exists() {
                    continue;
                }
                let segf = SegmentFile { path: p.clone() };
                if let Ok(m) = segf.read_metadata() {
                    let segment_id = p
                        .file_stem()
                        .and_then(|value| value.to_str())
                        .unwrap_or("unknown");
                    match Self::completion_ledger().all_complete(segment_id, &m.index) {
                        Ok(true) => {
                            Self::remove_fully_compacted_disk_segment(&p, &m, &mut result);
                        }
                        Ok(false) => {}
                        Err(err) => {
                            result.errors = result.errors.saturating_add(1);
                            warn!(
                                "WAL sweep: retaining segment {} with unreadable completion ledger: {}",
                                p.to_string_lossy(),
                                err
                            );
                        }
                    }
                }
            }
        }
        result
    }

    /// Pressure-only full sweep: delete only commit-marked segments whose indexed slices are complete.
    pub fn sweep_pressure_safe() -> WalSweepResult {
        Self::sweep_segment_cleanup()
    }

    fn indexed_wal_ref_count() -> usize {
        compaction_index().indexed_slice_count()
    }

    fn index_committed_segment(path: PathBuf, meta: SegmentFileMetadata) -> usize {
        let refs = meta.index.len();
        Self::segment_cache_register(SegmentSource::Disk(path), meta);
        refs
    }

    /// Reindex commit-marked segments missing from the in-memory cache.
    pub fn reconcile_missing_cache_entries() -> WalReconcileResult {
        let mut result = WalReconcileResult::default();
        let seg_dir = PathBuf::from(format!("{}/segment_buffer/segs", Config::get_data_dir()));
        if !seg_dir.exists() {
            result.indexed_refs = Self::indexed_wal_ref_count();
            result.schedulable_refs = Self::reclaimable_wal_partition_count(usize::MAX);
            return result;
        }
        if let Ok(rd) = fs::read_dir(&seg_dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("seg") {
                    continue;
                }
                let commit = path.with_extension("seg.commit");
                if !commit.exists() {
                    continue;
                }
                result.committed_segments = result.committed_segments.saturating_add(1);
                let segment_id = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_string();
                if SEGMENT_CACHE.contains_key(&segment_id) {
                    continue;
                }
                let seg = SegmentFile { path: path.clone() };
                match seg.read_metadata() {
                    Ok(meta) => {
                        let refs = Self::index_committed_segment(path, meta);
                        result.indexed_refs = result.indexed_refs.saturating_add(refs);
                    }
                    Err(err) => {
                        result.unreadable_segments = result.unreadable_segments.saturating_add(1);
                        warn!(
                            "WAL reconcile: unreadable committed segment {}: {}",
                            path.to_string_lossy(),
                            err
                        );
                    }
                }
            }
        }
        if result.indexed_refs == 0 {
            result.indexed_refs = Self::indexed_wal_ref_count();
        }
        result.schedulable_refs = Self::reclaimable_wal_partition_count(usize::MAX);
        result
    }

    pub fn wal_pressure_snapshot() -> WalPressureSnapshot {
        let now_secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let last_progress = crate::metrics::counters::LAST_WAL_RECLAIM_PROGRESS_EPOCH_SECS
            .load(AtomicOrdering::Relaxed);
        let last_progress_age_secs = if last_progress == 0 {
            None
        } else {
            Some(now_secs.saturating_sub(last_progress))
        };
        WalPressureSnapshot {
            pipeline: Config::get_pipeline_name(),
            committed_segments: Self::segs_remaining(),
            indexed_refs: Self::indexed_wal_ref_count(),
            schedulable_refs: Self::reclaimable_wal_partition_count(usize::MAX),
            sink_work_in_flight: crate::buffer::compaction_progress::sink_work_in_flight_count(),
            wal_compactions_in_flight: crate::metrics::counters::WAL_COMPACTIONS_IN_FLIGHT
                .load(AtomicOrdering::Relaxed),
            segments_deleted_cumulative: crate::metrics::counters::WAL_SEGMENTS_RECLAIMED_TOTAL
                .load(AtomicOrdering::Relaxed),
            bytes_reclaimed_cumulative: crate::metrics::counters::WAL_BYTES_RECLAIMED_TOTAL
                .load(AtomicOrdering::Relaxed),
            last_progress_age_secs,
        }
    }

    pub fn current_pipeline_work_exhausted() -> bool {
        let snapshot = Self::wal_pressure_snapshot();
        snapshot.schedulable_refs == 0
            && snapshot.wal_compactions_in_flight == 0
            && snapshot.sink_work_in_flight == 0
    }

    pub fn segs_remaining() -> usize {
        let seg_dir = PathBuf::from(format!("{}/segment_buffer/segs", Config::get_data_dir()));
        if !seg_dir.exists() {
            return 0;
        }
        let mut count = 0usize;
        if let Ok(rd) = fs::read_dir(&seg_dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|s| s.to_str()) == Some("seg") {
                    let commit = p.with_extension("seg.commit");
                    if commit.exists() {
                        count += 1;
                    }
                }
            }
        }
        count
    }

    /// Snapshot for DATA_DIR pause progress logs.
    pub fn pause_progress_snapshot() -> WalPauseProgress {
        let pressure = Self::wal_pressure_snapshot();
        WalPauseProgress {
            pipeline: pressure.pipeline,
            committed_segments: pressure.committed_segments,
            indexed_refs: pressure.indexed_refs,
            schedulable_refs: pressure.schedulable_refs,
            segs_remaining: pressure.committed_segments,
            reclaimable_partitions: pressure.schedulable_refs,
            sink_work_in_flight: pressure.sink_work_in_flight,
            wal_compactions_in_flight: pressure.wal_compactions_in_flight,
            segments_deleted_cumulative: pressure.segments_deleted_cumulative,
            bytes_reclaimed_cumulative: pressure.bytes_reclaimed_cumulative,
            last_progress_age_secs: pressure.last_progress_age_secs,
            wal_compactions_completed: crate::metrics::counters::WAL_COMPACTIONS_COMPLETED
                .load(AtomicOrdering::Relaxed),
            wal_txn_completed: crate::metrics::counters::WAL_COMPACTION_TRANSACTIONS_COMPLETED
                .load(AtomicOrdering::Relaxed),
            wal_refs_tombstoned: crate::metrics::counters::WAL_COMPACTION_REFS_TOMBSTONED
                .load(AtomicOrdering::Relaxed),
        }
    }

    fn read_disk_entry_batches(
        entry: &CompactionEntry,
        seg_path: &Path,
    ) -> io::Result<Vec<RecordBatch>> {
        let mut file = OpenOptions::new().read(true).open(seg_path)?;
        file.seek(io::SeekFrom::Start(entry.idx.start))?;
        let reader = io::BufReader::new(file);
        use std::io::Read as IoRead;
        let mut take = reader.take(entry.idx.len);
        match StreamReader::try_new(&mut take, None) {
            Ok(stream_reader) => stream_reader
                .collect::<Result<Vec<_>, _>>()
                .map_err(|err| io::Error::other(err.to_string()))
                .or_else(|err| {
                    Self::maybe_quarantine_disk_read_error(entry, seg_path, &err)?;
                    Err(err)
                }),
            Err(err) => {
                let err_str = err.to_string();
                if Self::is_truncated_stream_error(&err_str) {
                    Self::quarantine_disk_read_error(entry, seg_path, &err_str);
                    return Err(io::Error::other(err_str));
                }
                Err(io::Error::other(err_str))
            }
        }
    }

    fn quarantine_disk_read_error(entry: &CompactionEntry, seg_path: &Path, err_str: &str) {
        let file_len = seg_path.metadata().map(|meta| meta.len()).unwrap_or(0);
        let seg_display = entry.source.display_name();
        let diag = format!(
            "seg={} file_len={} start={} idx_len={} key={:?} error={}",
            seg_display, file_len, entry.idx.start, entry.idx.len, entry.idx.key, err_str
        );
        Self::quarantine_truncated_disk_segment(seg_path, &diag, &seg_display);
    }

    fn maybe_quarantine_disk_read_error(
        entry: &CompactionEntry,
        seg_path: &Path,
        err: &io::Error,
    ) -> io::Result<()> {
        let err_str = err.to_string();
        if Self::is_truncated_stream_error(&err_str) {
            Self::quarantine_disk_read_error(entry, seg_path, &err_str);
        }
        Ok(())
    }

    fn read_entry_batches(
        entry: &CompactionEntry,
        s3_resolved: Option<Arc<Vec<u8>>>,
    ) -> io::Result<Vec<RecordBatch>> {
        match (&s3_resolved, &entry.source) {
            (Some(data), SegmentSource::S3 { .. }) => {
                let start = entry.idx.start as usize;
                let end = start.saturating_add(entry.idx.len as usize).min(data.len());
                let mut cursor = io::Cursor::new(&data[start..end]);
                StreamReader::try_new(&mut cursor, None)
                    .map_err(|err| io::Error::other(err.to_string()))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|err| io::Error::other(err.to_string()))
            }
            (None, SegmentSource::Disk(seg_path)) => Self::read_disk_entry_batches(entry, seg_path),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "grouped compaction source/body mismatch",
            )),
        }
    }

    fn grouped_stream_eager_max_parts() -> usize {
        Config::getenv(
            "WAL_GROUPED_STREAM_EAGER_MAX_PARTS",
            &DEFAULT_GROUPED_STREAM_EAGER_MAX_PARTS.to_string(),
        )
        .parse::<usize>()
        .ok()
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_GROUPED_STREAM_EAGER_MAX_PARTS)
    }

    fn grouped_stream_eager_max_bytes() -> u64 {
        Config::getenv(
            "WAL_GROUPED_STREAM_EAGER_MAX_BYTES",
            &DEFAULT_GROUPED_STREAM_EAGER_MAX_BYTES.to_string(),
        )
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_GROUPED_STREAM_EAGER_MAX_BYTES)
    }

    fn should_build_grouped_stream_eager(work: &CompactionWork) -> bool {
        if work.entries.len() > Self::grouped_stream_eager_max_parts() {
            return false;
        }
        let total_bytes = work
            .entries
            .iter()
            .fold(0u64, |sum, entry| sum.saturating_add(entry.idx.bytes));
        total_bytes <= Self::grouped_stream_eager_max_bytes()
    }

    async fn build_eager_grouped_stream(
        work: &CompactionWork,
        expected_cdc_rows: Option<u64>,
    ) -> io::Result<(SendableRecordBatchStream, u64)> {
        let mut batches = Vec::new();
        for entry in work.entries.iter() {
            let s3_body = Self::resolve_entry_s3_body(entry).await?;
            let entry_for_blocking = entry.clone();
            let entry_batches = tokio::task::spawn_blocking(move || {
                Self::read_entry_batches(&entry_for_blocking, s3_body)
            })
            .await
            .map_err(|err| io::Error::other(err.to_string()))??;
            if let Some(expected) = entry.idx.part_meta_summary.row_count() {
                let actual = entry_batches
                    .iter()
                    .map(|batch| batch.num_rows() as u64)
                    .sum::<u64>();
                if actual != expected {
                    return Err(io::Error::other(format!(
                        "CDC WAL slice ordinal {} metadata expected {} rows but Arrow stream produced {} rows",
                        entry.idx.slice_ordinal, expected, actual
                    )));
                }
            }
            batches.extend(entry_batches);
        }

        let mut schema: Option<SchemaRef> = None;
        let mut seen_rows = 0u64;
        for batch in batches.iter() {
            if let Some(existing) = &schema {
                if existing.as_ref() != batch.schema().as_ref() {
                    return Err(io::Error::other("grouped compaction schema mismatch"));
                }
            } else {
                schema = Some(batch.schema());
            }
            seen_rows = seen_rows.saturating_add(batch.num_rows() as u64);
        }

        if let Some(expected) = expected_cdc_rows {
            if seen_rows != expected {
                return Err(io::Error::other(format!(
                    "CDC grouped WAL metadata expected {} rows but Arrow stream produced {} rows",
                    expected, seen_rows
                )));
            }
        }

        struct EagerGroupedWalBatchStream {
            schema: SchemaRef,
            batches: std::vec::IntoIter<RecordBatch>,
        }

        impl futures::Stream for EagerGroupedWalBatchStream {
            type Item = Result<RecordBatch, DataFusionError>;

            fn poll_next(
                mut self: Pin<&mut Self>,
                _cx: &mut TaskContext<'_>,
            ) -> TaskPoll<Option<Self::Item>> {
                TaskPoll::Ready(self.batches.next().map(Ok))
            }
        }

        impl RecordBatchStream for EagerGroupedWalBatchStream {
            fn schema(&self) -> SchemaRef {
                self.schema.clone()
            }
        }

        let schema = schema.unwrap_or_else(|| Arc::new(arrow_schema::Schema::empty()));
        Ok((
            Box::pin(EagerGroupedWalBatchStream {
                schema,
                batches: batches.into_iter(),
            }),
            seen_rows,
        ))
    }

    async fn resolve_entry_s3_body(entry: &CompactionEntry) -> io::Result<Option<Arc<Vec<u8>>>> {
        match &entry.source {
            SegmentSource::S3 {
                body: Some(body), ..
            } => Ok(Some(body.clone())),
            SegmentSource::S3 {
                body: None,
                bucket,
                key,
            } => s3_wal_body_cache::get_or_fetch(bucket, key, entry.source.segment_id())
                .await
                .map(Some),
            SegmentSource::Disk(_) => Ok(None),
        }
    }

    fn read_entry_part_meta(
        entry: &CompactionEntry,
        s3_resolved: Option<Arc<Vec<u8>>>,
    ) -> io::Result<Option<crate::plugins::cdc::WalPartMeta>> {
        match (&s3_resolved, &entry.source) {
            (Some(data), SegmentSource::S3 { .. }) => {
                SegmentFile::read_part_meta_from_bytes(data.as_ref(), &entry.idx)
            }
            (None, SegmentSource::Disk(seg_path)) => {
                let mut file = OpenOptions::new().read(true).open(seg_path)?;
                SegmentFile::read_part_meta_from_reader(&mut file, &entry.idx)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "grouped compaction metadata source/body mismatch",
            )),
        }
    }

    async fn load_entry_cdc_meta(
        entry: &CompactionEntry,
    ) -> io::Result<crate::plugins::cdc::WalPartMeta> {
        if !entry.idx.part_meta_summary.is_cdc() {
            return Err(io::Error::other(format!(
                "WAL slice ordinal {} is not CDC",
                entry.idx.slice_ordinal
            )));
        }
        let s3_body = Self::resolve_entry_s3_body(entry).await?;
        let entry_for_blocking = entry.clone();
        let meta = tokio::task::spawn_blocking(move || {
            Self::read_entry_part_meta(&entry_for_blocking, s3_body)
        })
        .await
        .map_err(|err| io::Error::other(err.to_string()))??
        .ok_or_else(|| {
            io::Error::other(format!(
                "CDC WAL slice ordinal {} has no metadata sidecar",
                entry.idx.slice_ordinal
            ))
        })?;
        if meta.kind != crate::plugins::cdc::WalPartKind::Cdc {
            return Err(io::Error::other(format!(
                "WAL slice ordinal {} metadata kind changed after indexing",
                entry.idx.slice_ordinal
            )));
        }
        Ok(meta)
    }

    async fn build_grouped_stream(
        work: &CompactionWork,
    ) -> io::Result<(
        SendableRecordBatchStream,
        Option<crate::plugins::cdc::SyncContext>,
        u64,
        Arc<AtomicU64>,
        bool,
    )> {
        let eager = Self::should_build_grouped_stream_eager(work);
        let _build_metrics = GroupedStreamBuildMetricGuard::new(eager);
        let mut cdc_rows = Vec::new();
        let mut saw_cdc = false;
        let mut saw_append = false;
        let mut total_rows = 0u64;

        for entry in work.entries.iter() {
            if entry.idx.part_meta_summary.is_cdc() {
                saw_cdc = true;
            } else {
                saw_append = true;
            }
        }

        if saw_cdc && saw_append {
            return Err(io::Error::other(format!(
                "grouped compaction transaction {} mixed CDC and append WAL parts",
                work.txn.id
            )));
        }

        let cdc_ctx = if saw_cdc {
            for entry in work.entries.iter() {
                let meta = Self::load_entry_cdc_meta(entry).await?;
                total_rows = total_rows.saturating_add(meta.row_count);
                cdc_rows.extend(meta.rows);
            }
            let part_meta = crate::plugins::cdc::WalPartMeta::cdc(cdc_rows, total_rows)
                .map_err(|err| io::Error::other(err.to_string()))?;
            let contract = crate::plugins::cdc::get_namespace_cdc_contract(&work.txn.namespace);
            Some(crate::plugins::cdc::SyncContext {
                part_meta,
                contract,
            })
        } else {
            None
        };
        let expected_cdc_rows = cdc_ctx.as_ref().map(|ctx| ctx.part_meta.row_count);
        let total_entries = work.entries.len();

        if eager {
            let (stream, seen_rows) =
                Self::build_eager_grouped_stream(work, expected_cdc_rows).await?;
            // Prefer Arrow row count for append; CDC meta matches Arrow after validation.
            let rows = if saw_cdc { total_rows } else { seen_rows };
            let row_counter = Arc::new(AtomicU64::new(rows));
            return Ok((stream, cdc_ctx, rows, row_counter, true));
        }

        enum GroupedStreamMsg {
            Batch {
                entry_index: usize,
                batch_index: usize,
                batch: RecordBatch,
            },
            EntryDone {
                entry_index: usize,
                batch_count: usize,
            },
        }

        let (tx, rx) = mpsc::channel::<Result<GroupedStreamMsg, DataFusionError>>(
            GROUPED_STREAM_CHANNEL_CAPACITY,
        );
        let entries = work.entries.clone();
        let producer_handle = tokio::spawn(async move {
            use futures::stream::{self, StreamExt};
            let producer = stream::iter(entries.into_iter().enumerate())
                .map(|(entry_index, entry)| async move {
                    let s3_body = Buffers::resolve_entry_s3_body(&entry)
                        .await
                        .map_err(|err| DataFusionError::External(Box::new(err)))?;
                    let entry_for_blocking = entry.clone();
                    let batches = tokio::task::spawn_blocking(move || {
                        Buffers::read_entry_batches(&entry_for_blocking, s3_body)
                    })
                    .await
                    .map_err(|err| DataFusionError::External(Box::new(io::Error::other(err))))?
                    .map_err(|err| DataFusionError::External(Box::new(err)))?;
                    if let Some(expected) = entry.idx.part_meta_summary.row_count() {
                        let actual = batches
                            .iter()
                            .map(|batch| batch.num_rows() as u64)
                            .sum::<u64>();
                        if actual != expected {
                            return Err(DataFusionError::Internal(format!(
                                "CDC WAL slice ordinal {} metadata expected {} rows but Arrow stream produced {} rows",
                                entry.idx.slice_ordinal, expected, actual
                            )));
                        }
                    }
                    Ok((entry_index, batches))
                })
                .buffered(GROUPED_STREAM_PREFETCH_PARTS);

            let mut producer = std::pin::pin!(producer);
            while let Some(result) = producer.next().await {
                match result {
                    Ok((entry_index, batches)) => {
                        let batch_count = batches.len();
                        for (batch_index, batch) in batches.into_iter().enumerate() {
                            if tx
                                .send(Ok(GroupedStreamMsg::Batch {
                                    entry_index,
                                    batch_index,
                                    batch,
                                }))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                        if tx
                            .send(Ok(GroupedStreamMsg::EntryDone {
                                entry_index,
                                batch_count,
                            }))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(err) => {
                        let _ = tx.send(Err(err)).await;
                        return;
                    }
                }
            }
        });

        // CDC: seed with metadata row count (known at build). Append: count Arrow rows as the
        // stream is consumed so complete/fail logs are accurate.
        let rows_known_at_build = saw_cdc;
        let row_counter = Arc::new(AtomicU64::new(if saw_cdc { total_rows } else { 0 }));

        struct GroupedWalBatchStream {
            schema: Option<SchemaRef>,
            rx: mpsc::Receiver<Result<GroupedStreamMsg, DataFusionError>>,
            producer_handle: tokio::task::JoinHandle<()>,
            next_entry_index: usize,
            next_batch_index: usize,
            reorder: BTreeMap<(usize, usize), RecordBatch>,
            entry_batch_counts: HashMap<usize, usize>,
            entries_done: usize,
            total_entries: usize,
            expected_cdc_rows: Option<u64>,
            seen_rows: u64,
            row_counter: Arc<AtomicU64>,
            count_into_counter: bool,
            emitted_cdc_count_error: bool,
            finished: bool,
        }

        impl GroupedWalBatchStream {
            fn record_emitted_rows(&mut self, batch_rows: u64) {
                self.seen_rows = self.seen_rows.saturating_add(batch_rows);
                if self.count_into_counter {
                    self.row_counter
                        .fetch_add(batch_rows, AtomicOrdering::Relaxed);
                }
            }

            fn try_emit_ready(&mut self) -> Option<Result<RecordBatch, DataFusionError>> {
                loop {
                    if self.next_entry_index >= self.total_entries {
                        return None;
                    }
                    if let Some(expected_count) =
                        self.entry_batch_counts.get(&self.next_entry_index)
                    {
                        if self.next_batch_index >= *expected_count {
                            self.next_entry_index += 1;
                            self.next_batch_index = 0;
                            continue;
                        }
                    } else {
                        return None;
                    }
                    match self
                        .reorder
                        .remove(&(self.next_entry_index, self.next_batch_index))
                    {
                        Some(batch) => {
                            if let Some(existing) = &self.schema {
                                if existing.as_ref() != batch.schema().as_ref() {
                                    self.finished = true;
                                    return Some(Err(DataFusionError::Internal(
                                        "grouped compaction schema mismatch".to_string(),
                                    )));
                                }
                            } else {
                                self.schema = Some(batch.schema());
                            }
                            self.record_emitted_rows(batch.num_rows() as u64);
                            self.next_batch_index += 1;
                            return Some(Ok(batch));
                        }
                        None => return None,
                    }
                }
            }

            fn finish_if_done(&mut self) -> TaskPoll<Option<Result<RecordBatch, DataFusionError>>> {
                if self.entries_done < self.total_entries
                    || !self.reorder.is_empty()
                    || self.next_entry_index < self.total_entries
                {
                    return TaskPoll::Pending;
                }
                if let Some(expected) = self.expected_cdc_rows {
                    if !self.emitted_cdc_count_error && self.seen_rows != expected {
                        self.emitted_cdc_count_error = true;
                        self.finished = true;
                        return TaskPoll::Ready(Some(Err(DataFusionError::Internal(format!(
                            "CDC grouped WAL metadata expected {} rows but Arrow stream produced {} rows",
                            expected, self.seen_rows
                        )))));
                    }
                }
                self.finished = true;
                TaskPoll::Ready(None)
            }
        }

        impl Drop for GroupedWalBatchStream {
            fn drop(&mut self) {
                self.producer_handle.abort();
            }
        }

        impl futures::Stream for GroupedWalBatchStream {
            type Item = Result<RecordBatch, DataFusionError>;

            fn poll_next(
                mut self: Pin<&mut Self>,
                cx: &mut TaskContext<'_>,
            ) -> TaskPoll<Option<Self::Item>> {
                if self.finished {
                    return TaskPoll::Ready(None);
                }
                if let Some(batch) = self.try_emit_ready() {
                    return TaskPoll::Ready(Some(batch));
                }
                match self.rx.poll_recv(cx) {
                    TaskPoll::Ready(Some(Ok(GroupedStreamMsg::Batch {
                        entry_index,
                        batch_index,
                        batch,
                    }))) => {
                        if entry_index == self.next_entry_index
                            && batch_index == self.next_batch_index
                        {
                            if let Some(existing) = &self.schema {
                                if existing.as_ref() != batch.schema().as_ref() {
                                    self.finished = true;
                                    return TaskPoll::Ready(Some(Err(DataFusionError::Internal(
                                        "grouped compaction schema mismatch".to_string(),
                                    ))));
                                }
                            } else {
                                self.schema = Some(batch.schema());
                            }
                            self.record_emitted_rows(batch.num_rows() as u64);
                            self.next_batch_index += 1;
                            TaskPoll::Ready(Some(Ok(batch)))
                        } else {
                            self.reorder.insert((entry_index, batch_index), batch);
                            if let Some(batch) = self.try_emit_ready() {
                                TaskPoll::Ready(Some(batch))
                            } else {
                                cx.waker().wake_by_ref();
                                TaskPoll::Pending
                            }
                        }
                    }
                    TaskPoll::Ready(Some(Ok(GroupedStreamMsg::EntryDone {
                        entry_index,
                        batch_count,
                    }))) => {
                        self.entry_batch_counts.insert(entry_index, batch_count);
                        self.entries_done += 1;
                        if let Some(batch) = self.try_emit_ready() {
                            TaskPoll::Ready(Some(batch))
                        } else {
                            let done = self.finish_if_done();
                            if matches!(done, TaskPoll::Pending) {
                                // We just consumed a channel item; poll_recv may not have registered
                                // a wake for messages that were already buffered behind it.
                                cx.waker().wake_by_ref();
                            }
                            done
                        }
                    }
                    TaskPoll::Ready(Some(Err(err))) => {
                        self.finished = true;
                        TaskPoll::Ready(Some(Err(err)))
                    }
                    TaskPoll::Ready(None) => {
                        if let Some(expected) = self.expected_cdc_rows {
                            if !self.emitted_cdc_count_error && self.seen_rows != expected {
                                self.emitted_cdc_count_error = true;
                                self.finished = true;
                                return TaskPoll::Ready(Some(Err(DataFusionError::Internal(
                                    format!(
                                        "CDC grouped WAL metadata expected {} rows but Arrow stream produced {} rows",
                                        expected, self.seen_rows
                                    ),
                                ))));
                            }
                        }
                        self.finished = true;
                        TaskPoll::Ready(None)
                    }
                    TaskPoll::Pending => TaskPoll::Pending,
                }
            }
        }

        impl RecordBatchStream for GroupedWalBatchStream {
            fn schema(&self) -> SchemaRef {
                self.schema
                    .clone()
                    .unwrap_or_else(|| Arc::new(arrow_schema::Schema::empty()))
            }
        }

        let stream = Box::pin(GroupedWalBatchStream {
            schema: None,
            rx,
            producer_handle,
            next_entry_index: 0,
            next_batch_index: 0,
            reorder: BTreeMap::new(),
            entry_batch_counts: HashMap::new(),
            entries_done: 0,
            total_entries,
            expected_cdc_rows,
            seen_rows: 0,
            row_counter: row_counter.clone(),
            count_into_counter: !saw_cdc,
            emitted_cdc_count_error: false,
            finished: false,
        });
        Ok((
            stream,
            cdc_ctx,
            if saw_cdc { total_rows } else { 0 },
            row_counter,
            rows_known_at_build,
        ))
    }

    fn mark_work_in_flight(work: &CompactionWork) -> bool {
        let mut inserted = Vec::new();
        for entry in work.entries.iter() {
            let key = (entry.source.display_name(), entry.idx.start, entry.idx.len);
            if COMPACTION_IN_FLIGHT.insert(key.clone(), ()).is_some() {
                for key in inserted {
                    COMPACTION_IN_FLIGHT.remove(&key);
                }
                let mut index = compaction_index();
                index.release_refs(&work.txn.refs, Self::now_secs());
                index.release_manifest(&work.txn.id);
                metrics_hot::set_compaction_planner_ready_work_count(index.ready_queue_depth());
                metrics_hot::set_compaction_inflight_slice_count(COMPACTION_IN_FLIGHT.len());
                return false;
            }
            inserted.push(key);
        }
        metrics_hot::set_compaction_inflight_slice_count(COMPACTION_IN_FLIGHT.len());
        true
    }

    fn release_work_in_flight(work: &CompactionWork) {
        for entry in work.entries.iter() {
            COMPACTION_IN_FLIGHT.remove(&(
                entry.source.display_name(),
                entry.idx.start,
                entry.idx.len,
            ));
        }
        metrics_hot::set_compaction_inflight_slice_count(COMPACTION_IN_FLIGHT.len());
        let mut index = compaction_index();
        index.release_refs(&work.txn.refs, Self::now_secs());
        index.release_manifest(&work.txn.id);
        metrics_hot::set_compaction_planner_ready_work_count(index.ready_queue_depth());
    }

    fn tombstone_grouped_work(work: &CompactionWork) -> io::Result<()> {
        let ledger = Self::completion_ledger();
        let mut grouped: HashMap<String, (SegmentSource, SegmentFileMetadata, Vec<usize>)> =
            HashMap::new();
        for entry in work.entries.iter() {
            let group = grouped
                .entry(entry.source.segment_id().to_string())
                .or_insert_with(|| (entry.source.clone(), entry.meta.clone(), Vec::new()));
            if !group.2.contains(&entry.ordinal) {
                group.2.push(entry.ordinal);
            }
        }

        let updates = grouped
            .iter()
            .map(
                |(segment_id, (_, meta, ordinals))| SegmentCompletionUpdate {
                    segment_id,
                    index: &meta.index,
                    ordinals,
                },
            )
            .collect::<Vec<_>>();
        ledger.mark_complete_batch(&updates)?;
        {
            let mut index = compaction_index();
            index.complete_refs(&work.txn.refs);
            metrics_hot::set_compaction_planner_ready_work_count(index.ready_queue_depth());
        }

        for (segment_id, (source, meta, _)) in grouped {
            match ledger.all_complete(&segment_id, &meta.index) {
                Ok(true) => match &source {
                    SegmentSource::Disk(seg_path) => {
                        let mut sweep = WalSweepResult::default();
                        Self::remove_fully_compacted_disk_segment(seg_path, &meta, &mut sweep);
                    }
                    SegmentSource::S3 { key, bucket, .. } => {
                        let key = key.clone();
                        let bucket = bucket.clone();
                        let segment_index = meta.index.clone();
                        tokio::spawn(async move {
                            let client = crate::helpers::s3::get_s3_client().await;
                            let commit_key = format!("{}.commit", key);
                            let _ = client
                                .delete_object()
                                .bucket(&bucket)
                                .key(&key)
                                .send()
                                .await;
                            let _ = client
                                .delete_object()
                                .bucket(&bucket)
                                .key(&commit_key)
                                .send()
                                .await;
                            Buffers::segment_cache_remove(&segment_id);
                            if let Err(err) = Buffers::completion_ledger()
                                .remove_segment(&segment_id, &segment_index)
                            {
                                error!(
                                    "Failed to remove completion state for S3 segment {}: {}",
                                    segment_id, err
                                );
                            }
                        });
                    }
                },
                Ok(false) => {}
                Err(err) => {
                    return Err(io::Error::other(format!(
                        "refusing to delete segment {} with unreadable completion ledger: {}",
                        source.display_name(),
                        err
                    )));
                }
            }
        }
        Ok(())
    }

    async fn compact_grouped_work(
        work: CompactionWork,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
        budget: Arc<FlushExecutionBudget>,
    ) -> io::Result<bool> {
        if work.entries.is_empty() || !Self::mark_work_in_flight(&work) {
            return Ok(false);
        }
        let Some(_conflict_guard) =
            crate::buffer::sink_conflict::try_acquire_sink_conflict(&work.txn)
        else {
            Self::release_work_in_flight(&work);
            return Ok(false);
        };
        let timeout = Self::grouped_compaction_timeout();
        let job_started = std::time::Instant::now();
        let progress = GroupedCompactionTracker::begin(
            work.txn.id.clone(),
            work.txn.namespace.clone(),
            work.txn.sink_ref.clone(),
            work.entries.len(),
            timeout,
            work.txn.attempts,
        );
        info!(
            "Compactor: grouped compaction started namespace={} wal_parts={} sink_ref={} compaction_id={} timeout_secs={} manifest_state={:?} attempts={}",
            work.txn.namespace,
            work.entries.len(),
            work.txn.sink_ref,
            work.txn.id,
            timeout.as_secs(),
            work.txn.state,
            work.txn.attempts,
        );
        struct WorkGuard<'a>(&'a CompactionWork);
        impl<'a> Drop for WorkGuard<'a> {
            fn drop(&mut self) {
                Buffers::release_work_in_flight(self.0);
                crate::metrics::counters::dec_wal_compactions_in_flight();
            }
        }
        crate::metrics::counters::add_wal_compaction_started(work.entries.len() as u64);
        crate::metrics::counters::add_wal_compaction_transaction_started(1);
        crate::metrics::counters::inc_wal_compactions_in_flight();
        let _guard = WorkGuard(&work);
        let _s3_body_pin = s3_wal_body_cache::pin_segments(
            work.entries
                .iter()
                .filter(|entry| entry.source.is_s3())
                .map(|entry| entry.source.segment_id().to_string()),
        );

        Self::persist_compaction_manifest(&work.txn)?;
        let _decode_permit = budget.acquire_decode().await?;
        let stream_started = std::time::Instant::now();
        let (batch_stream, cdc_ctx, rows, row_counter, rows_known_at_build) =
            match Self::build_grouped_stream(&work).await {
                Ok(stream) => stream,
                Err(err) => {
                    let err_str = err.to_string();
                    Self::quarantine_truncated_entries(&work.entries, "grouped_read", &err_str);
                    let failure_key = format!("{}:{}", work.txn.sink_ref, work.txn.id);
                    let attempts = {
                        let mut entry = COMPACT_FAILURES.entry(failure_key.clone()).or_insert(0);
                        *entry += 1;
                        *entry
                    };
                    error!(
                    "Compactor: grouped stream build failed sink_ref={} namespace={} compaction_id={} attempt={} wal_parts={} elapsed={}s err={}",
                    work.txn.sink_ref,
                    work.txn.namespace,
                    work.txn.id,
                    attempts,
                    work.entries.len(),
                    stream_started.elapsed().as_secs(),
                    err_str
                );
                    crate::metrics::counters::add_wal_compaction_transaction_failed(1);
                    return Err(err);
                }
            };
        progress.set_rows(rows);
        progress.set_phase(GroupedCompactionPhase::UploadingToSink);
        let rows_start_label = if rows_known_at_build {
            rows.to_string()
        } else {
            "pending".to_string()
        };
        info!(
            "Compactor: grouped compaction building_wal_stream complete namespace={} wal_parts={} rows={} elapsed={}s compaction_id={}",
            work.txn.namespace,
            work.entries.len(),
            rows_start_label,
            stream_started.elapsed().as_secs(),
            work.txn.id,
        );
        let source_contract =
            crate::plugins::source_contract::namespace_source_contract(&work.txn.namespace);
        let sink_ctx = crate::plugins::SinkWriteContext {
            filename: work.txn.target_filename.clone(),
            compaction_id: work.txn.id.clone(),
            idempotency_key: work.txn.id.clone(),
            wal_refs: work
                .txn
                .refs
                .iter()
                .map(WalPartRef::to_runtime_ref)
                .collect(),
            write_semantics: work.txn.semantics,
            schema_fingerprint: work.txn.schema_fingerprint.clone(),
            cdc_ctx: cdc_ctx.as_ref(),
            source_contract: source_contract.as_ref(),
        };
        let grouped_ctx = crate::plugins::GroupedSinkWriteContext::try_from(sink_ctx)
            .map_err(|err| io::Error::new(io::ErrorKind::Unsupported, err))?;
        if work.entries.len() >= 100
            && (grouped_ctx.grouping_key.partition.is_empty()
                || grouped_ctx.grouping_key.time.is_none())
        {
            warn!(
                "Compactor: high-volume grouped write has empty partition/time grouping namespace={} sink_ref={} compaction_id={} wal_parts={} partition_empty={} time_empty={}",
                grouped_ctx.grouping_key.namespace,
                grouped_ctx.grouping_key.sink_ref,
                grouped_ctx.compaction_id,
                work.entries.len(),
                grouped_ctx.grouping_key.partition.is_empty(),
                grouped_ctx.grouping_key.time.is_none()
            );
        }
        let grouped_reader = crate::plugins::GroupedBatchReader::new(
            batch_stream,
            grouped_ctx.grouping_key.clone(),
            crate::plugins::GroupedBatchReaderConfig::default(),
        );
        let _commit_lane =
            crate::buffer::sink_conflict::acquire_exact_once_commit_lane(&work.txn).await;
        let sink_permit = budget.acquire_sink().await?;
        let sent_txn = work.txn.clone().mark_sent();
        Self::persist_compaction_manifest(&sent_txn)?;
        info!(
            "Compactor: grouped compaction uploading_to_sink started namespace={} rows={} sink_ref={} compaction_id={} target={}",
            work.txn.namespace,
            rows_start_label,
            work.txn.sink_ref,
            work.txn.id,
            work.txn.target_filename,
        );
        let upload_started = std::time::Instant::now();
        let sink_result = shared_output
            .sync_grouped(grouped_reader, grouped_ctx)
            .await;
        drop(sink_permit);
        metrics_hot::record_sink_apply(upload_started.elapsed());
        if let Err(err) = sink_result {
            let err_str = err.to_string();
            Self::quarantine_truncated_entries(&work.entries, "grouped_sync", &err_str);
            let failure_key = format!("{}:{}", work.txn.sink_ref, work.txn.id);
            let attempts = {
                let mut entry = COMPACT_FAILURES.entry(failure_key.clone()).or_insert(0);
                *entry += 1;
                *entry
            };
            let final_rows = row_counter.load(AtomicOrdering::Relaxed);
            error!(
                "Compactor: grouped compact failed sink_ref={} namespace={} compaction_id={} attempt={} rows={} upload_elapsed={}s job_elapsed={}s err={}",
                work.txn.sink_ref,
                work.txn.namespace,
                work.txn.id,
                attempts,
                final_rows,
                upload_started.elapsed().as_secs(),
                job_started.elapsed().as_secs(),
                err_str
            );
            crate::metrics::counters::add_wal_compaction_transaction_failed(1);
            return Err(err);
        }
        #[cfg(test)]
        observe_grouped_compaction_test_event(GroupedCompactionTestEvent::SinkSucceeded);
        let final_rows = row_counter.load(AtomicOrdering::Relaxed);
        progress.set_rows(final_rows);
        Self::persist_compaction_manifest(&sent_txn.mark_acked())?;
        #[cfg(test)]
        observe_grouped_compaction_test_event(GroupedCompactionTestEvent::ManifestAcked);
        Self::tombstone_grouped_work(&work)?;
        #[cfg(test)]
        observe_grouped_compaction_test_event(GroupedCompactionTestEvent::SlicesTombstoned);
        COMPACT_FAILURES.remove(&format!("{}:{}", work.txn.sink_ref, work.txn.id));
        crate::metrics::counters::add_wal_compaction_completed(work.entries.len() as u64);
        crate::metrics::counters::add_wal_compaction_transaction_completed(1);
        info!(
            "Compactor: grouped compaction complete namespace={} wal_parts={} rows={} upload_elapsed={}s job_elapsed={}s compaction_id={} target={}",
            work.txn.namespace,
            work.entries.len(),
            final_rows,
            upload_started.elapsed().as_secs(),
            job_started.elapsed().as_secs(),
            work.txn.id,
            work.txn.target_filename
        );
        crate::metrics::counters::add_wal_compaction_refs_tombstoned(work.entries.len() as u64);
        Self::remove_compaction_manifest(&work.txn.id)?;
        progress.finish();
        Ok(true)
    }
}

fn mark_offsets_durable_in_wal<'a, I>(offsets_db: &Offsets, offsets: I)
where
    I: IntoIterator<Item = (&'a OffsetKey, &'a u64)>,
{
    for (offset, position) in offsets {
        if Config::debug_enabled() || Config::log_wal_enabled() {
            debug!(
                "WAL offset mark start namespace={} partition={} position={}",
                offset.namespace, offset.partition, position
            );
        }
        let offset_key = OffsetKey {
            namespace: offset.namespace.clone(),
            partition: offset.partition.clone(),
        };
        if Config::debug_enabled() || Config::log_wal_enabled() {
            debug!(
                "WAL offset set closed start namespace={} partition={} value=1",
                offset.namespace, offset.partition
            );
            append_wal_debug_trace(&format!(
                "offset_set_closed_start namespace={} partition={} value=1",
                offset.namespace, offset.partition
            ));
        }
        offsets_db.set(&offset_key, OffsetTypes::Closed, 1);
        if Config::debug_enabled() || Config::log_wal_enabled() {
            debug!(
                "WAL offset set closed done namespace={} partition={} value=1",
                offset.namespace, offset.partition
            );
            append_wal_debug_trace(&format!(
                "offset_set_closed_done namespace={} partition={} value=1",
                offset.namespace, offset.partition
            ));
        }
        if Config::debug_enabled() || Config::log_wal_enabled() {
            debug!(
                "WAL offset set position start namespace={} partition={} value={}",
                offset.namespace, offset.partition, position
            );
            append_wal_debug_trace(&format!(
                "offset_set_position_start namespace={} partition={} value={}",
                offset.namespace, offset.partition, position
            ));
        }
        offsets_db.set(&offset_key, OffsetTypes::Position, *position);
        if Config::debug_enabled() || Config::log_wal_enabled() {
            debug!(
                "WAL offset set position done namespace={} partition={} value={}",
                offset.namespace, offset.partition, position
            );
            append_wal_debug_trace(&format!(
                "offset_set_position_done namespace={} partition={} value={}",
                offset.namespace, offset.partition, position
            ));
        }
        if Config::debug_enabled() || Config::log_wal_enabled() {
            debug!(
                "WAL offset mark done namespace={} partition={} position={}",
                offset.namespace, offset.partition, position
            );
        }
    }
}

fn append_wal_debug_trace(message: &str) {
    if !(Config::debug_enabled() || Config::log_wal_enabled()) {
        return;
    }
    let path = PathBuf::from(format!("{}/wal-debug-trace.log", Config::get_data_dir()));
    let timestamp_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{} {}", timestamp_ms, message);
        let _ = file.flush();
    }
}

fn offset_sample<'a, I>(offsets: I, limit: usize) -> Vec<String>
where
    I: IntoIterator<Item = (&'a OffsetKey, &'a u64)>,
{
    offsets
        .into_iter()
        .take(limit)
        .map(|(offset, position)| format!("{}:{}@{}", offset.namespace, offset.partition, position))
        .collect()
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WalSweepResult {
    pub segments_deleted: usize,
    pub bytes_reclaimed: u64,
    pub errors: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WalReconcileResult {
    pub committed_segments: usize,
    pub indexed_refs: usize,
    pub schedulable_refs: usize,
    pub unreadable_segments: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalPressureSnapshot {
    pub pipeline: String,
    pub committed_segments: usize,
    pub indexed_refs: usize,
    pub schedulable_refs: usize,
    pub sink_work_in_flight: usize,
    pub wal_compactions_in_flight: usize,
    pub segments_deleted_cumulative: u64,
    pub bytes_reclaimed_cumulative: u64,
    pub last_progress_age_secs: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalPauseProgress {
    pub pipeline: String,
    pub committed_segments: usize,
    pub indexed_refs: usize,
    pub schedulable_refs: usize,
    pub segs_remaining: usize,
    pub reclaimable_partitions: usize,
    pub sink_work_in_flight: usize,
    pub wal_compactions_in_flight: usize,
    pub segments_deleted_cumulative: u64,
    pub bytes_reclaimed_cumulative: u64,
    pub last_progress_age_secs: Option<u64>,
    pub wal_compactions_completed: u64,
    pub wal_txn_completed: u64,
    pub wal_refs_tombstoned: u64,
}

fn wal_index_progress_every() -> usize {
    Config::getenv("WAL_INDEX_PROGRESS_EVERY", "500")
        .parse::<usize>()
        .ok()
        .filter(|value| *value > 0)
        .unwrap_or(500)
}

struct RecoveredSegmentMeta {
    path: PathBuf,
    meta: SegmentFileMetadata,
}

pub fn wal_recover_disk(offsets_db: Arc<Offsets>) -> io::Result<()> {
    let started = std::time::Instant::now();
    let mut dir_entries_scanned = 0u64;
    let mut commit_markers_seen = 0u64;

    info!("Indexing commited WAL Segments");

    // Scan the on-disk segment directory for .seg files
    let seg_dir = PathBuf::from(format!("{}/segment_buffer/segs", Config::get_data_dir()));
    let mut seg_files: Vec<PathBuf> = Vec::new();
    if seg_dir.exists() {
        for entry in fs::read_dir(&seg_dir)? {
            let entry = entry?;
            let path = entry.path();
            dir_entries_scanned = dir_entries_scanned.saturating_add(1);
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if name.ends_with(".seg.commit") {
                commit_markers_seen = commit_markers_seen.saturating_add(1);
            }
            if path.extension().and_then(|s| s.to_str()) == Some("seg") {
                let commit = path.with_extension("seg.commit");
                if commit.exists() {
                    seg_files.push(path);
                }
            }
        }
    }

    let seg_files_count = seg_files.len();
    info!(
        "WAL scan examined {} entries (commit_markers_seen={}, committed_seg_candidates={})",
        dir_entries_scanned, commit_markers_seen, seg_files_count
    );

    let progress_every = wal_index_progress_every();
    if seg_files_count > 0 {
        info!(
            "WAL indexing: reading metadata for {} segment files (parallel, progress every {})",
            seg_files_count, progress_every
        );
    }

    let indexed_count = AtomicUsize::new(0);
    let failed_count = AtomicUsize::new(0);
    let recovered_segments: Vec<RecoveredSegmentMeta> = seg_files
        .par_iter()
        .filter_map(|file_path| {
            let seg = SegmentFile {
                path: file_path.clone(),
            };
            match seg.read_metadata() {
                Ok(meta) => {
                    let n = indexed_count.fetch_add(1, AtomicOrdering::Relaxed) + 1;
                    if n % progress_every == 0 || n == seg_files_count {
                        let elapsed = started.elapsed().as_secs_f64();
                        let rate = if elapsed > 0.0 {
                            n as f64 / elapsed
                        } else {
                            0.0
                        };
                        info!(
                            "WAL indexing progress: {}/{} segment files ({:.1}%, ~{:.0} files/s)",
                            n,
                            seg_files_count,
                            (n as f64 * 100.0) / seg_files_count as f64,
                            rate
                        );
                    }
                    Some(RecoveredSegmentMeta {
                        path: file_path.clone(),
                        meta,
                    })
                }
                Err(e) => {
                    failed_count.fetch_add(1, AtomicOrdering::Relaxed);
                    warn!(
                        "Failed to read segment metadata {}: {}",
                        file_path.to_string_lossy(),
                        e
                    );
                    None
                }
            }
        })
        .collect();

    let failures = failed_count.load(AtomicOrdering::Relaxed);
    if failures > 0 {
        warn!(
            "WAL indexing: {} of {} segment files failed metadata read",
            failures, seg_files_count
        );
    }

    let mut count = 0u64;
    let mut bytes = 0u64;
    let mut namespaces: HashSet<String> = HashSet::new();
    let mut namespace_partition_files: HashMap<PartitionKey, u64> = HashMap::new();
    let mut namespace_partition_bytes: HashMap<PartitionKey, u64> = HashMap::new();
    let mut committed_offsets: u64 = 0;

    for recovered in recovered_segments {
        let RecoveredSegmentMeta {
            path: file_path,
            meta,
        } = recovered;
        if Config::debug_enabled() || Config::log_wal_enabled() {
            let partition_sample: Vec<String> = meta
                .index
                .iter()
                .take(3)
                .map(|idx| format!("{}/{}/{}B", idx.key.namespace, idx.key.partition, idx.bytes))
                .collect();
            info!(
                "WAL recover disk: segment={} partitions={} offsets={} total_bytes={} partition_sample={:?} offset_sample={:?}",
                file_path.to_string_lossy(),
                meta.index.len(),
                meta.offsets.len(),
                meta.total_bytes,
                partition_sample,
                offset_sample(meta.offsets.iter(), 3)
            );
        }
        for idx in meta.index.iter() {
            let key = idx.key.clone();
            namespaces.insert(key.namespace.clone());
            *namespace_partition_files.entry(key.clone()).or_insert(0) += 1;
            *namespace_partition_bytes.entry(key.clone()).or_insert(0) =
                (*namespace_partition_bytes.get(&key).unwrap_or(&0)).saturating_add(idx.bytes);
        }
        bytes = bytes.saturating_add(meta.total_bytes);
        count = count.saturating_add(1);
        mark_offsets_durable_in_wal(offsets_db.as_ref(), meta.offsets.iter());
        committed_offsets = committed_offsets.saturating_add(meta.offsets.len() as u64);
        Buffers::segment_cache_register(SegmentSource::Disk(file_path), meta);
    }

    let elapsed = started.elapsed().as_secs_f64();
    info!(
        "Indexed {} of {} Segment files for {} namespaces",
        count,
        seg_files_count,
        namespaces.len()
    );
    if elapsed > 0.0 {
        let rate = (count as f64 / elapsed) as u64;
        info!(
            "WAL indexing took {:.2}s ~ {} files/s, {} total bytes",
            elapsed,
            rate,
            Helpers::human_readable_size(bytes as u64)
        );
    }
    info!("Committed {} offsets from .seg files", committed_offsets);

    let mut wal_index_metrics: WalIndexMetrics = WalIndexMetrics {
        metrics: Vec::new(),
    };
    for (key, file_count) in namespace_partition_files.iter() {
        let bytes_sum: u64 = namespace_partition_bytes
            .iter()
            .filter(|(candidate, _)| candidate.namespace == key.namespace)
            .map(|(_, v)| *v)
            .sum();
        wal_index_metrics.metrics.push(WalIndexMetric {
            namespace: key.namespace.clone(),
            partitions: 0,
            files: *file_count,
            bytes: bytes_sum,
        });
    }
    {
        let mut metrics = METRICS.write();
        metrics.wal_index_namespaces_total = namespaces.len() as u64;
        metrics.wal_index_partitions_total = 0;
        metrics.wal_index_files_total = count as u64;
        metrics.wal_index_bytes_total = bytes as u64;
        metrics.wal_index_metrics = wal_index_metrics;
    }

    offsets_db.flush();
    Ok(())
}

/// Async S3 WAL offset recovery: lists committed segments in S3, reads their
/// S3 WAL recovery: lists committed segments, downloads each, parses
/// metadata, commits offsets, and populates the segment cache.
async fn wal_recover_s3(offsets_db: Arc<Offsets>) -> io::Result<()> {
    let bucket = Config::get_wal_s3_bucket();
    let prefix = Config::get_wal_s3_prefix();

    info!(
        "S3 WAL recovery: scanning s3://{}/{} for committed segments",
        bucket, prefix
    );
    let client = crate::helpers::s3::get_s3_client().await;

    let mut token: Option<String> = None;
    let mut segs: HashSet<String> = HashSet::new();
    let mut commits: HashSet<String> = HashSet::new();
    loop {
        let mut req = client.list_objects_v2().bucket(&bucket).prefix(&prefix);
        if let Some(t) = &token {
            req = req.continuation_token(t);
        }
        match req.send().await {
            Ok(resp) => {
                if let Some(contents) = resp.contents {
                    for obj in contents {
                        if let Some(k) = obj.key() {
                            if k.ends_with(".seg") {
                                segs.insert(k.to_string());
                            }
                            if k.ends_with(".seg.commit") {
                                commits.insert(k.trim_end_matches(".commit").to_string());
                            }
                        }
                    }
                }
                if resp.is_truncated.unwrap_or(false) {
                    token = resp.next_continuation_token;
                } else {
                    break;
                }
            }
            Err(e) => {
                error!("S3 WAL recovery: list error: {}", e);
                break;
            }
        }
    }

    let mut processed: u64 = 0;
    let mut committed_offsets: u64 = 0;
    let mut namespaces: HashSet<String> = HashSet::new();
    let mut bytes_total: u64 = 0;

    for seg_key in segs.iter() {
        if !commits.contains(seg_key) {
            continue;
        }
        match client
            .get_object()
            .bucket(&bucket)
            .key(seg_key)
            .send()
            .await
        {
            Ok(resp) => match resp.body.collect().await {
                Ok(agg) => {
                    let data = agg.into_bytes().to_vec();
                    let meta = match SegmentFile::read_metadata_from_bytes(&data) {
                        Ok(m) => m,
                        Err(e) => {
                            warn!("S3 WAL recovery: bad segment metadata {}: {}", seg_key, e);
                            continue;
                        }
                    };
                    drop(data);
                    if Config::debug_enabled() || Config::log_wal_enabled() {
                        let partition_sample: Vec<String> = meta
                            .index
                            .iter()
                            .take(3)
                            .map(|idx| {
                                format!(
                                    "{}/{}/{}B",
                                    idx.key.namespace, idx.key.partition, idx.bytes
                                )
                            })
                            .collect();
                        info!(
                            "WAL recover s3: segment={} partitions={} offsets={} total_bytes={} partition_sample={:?} offset_sample={:?}",
                            seg_key,
                            meta.index.len(),
                            meta.offsets.len(),
                            meta.total_bytes,
                            partition_sample,
                            offset_sample(meta.offsets.iter(), 3)
                        );
                    }
                    for idx in meta.index.iter() {
                        namespaces.insert(idx.key.namespace.clone());
                    }
                    mark_offsets_durable_in_wal(offsets_db.as_ref(), meta.offsets.iter());
                    committed_offsets = committed_offsets.saturating_add(meta.offsets.len() as u64);
                    bytes_total = bytes_total.saturating_add(meta.total_bytes);
                    processed = processed.saturating_add(1);
                    Buffers::segment_cache_register(
                        SegmentSource::S3 {
                            key: seg_key.clone(),
                            bucket: bucket.clone(),
                            body: None,
                        },
                        meta,
                    );
                }
                Err(e) => {
                    error!("S3 WAL recovery: body error {}: {}", seg_key, e);
                }
            },
            Err(e) => {
                error!("S3 WAL recovery: get error {}: {}", seg_key, e);
            }
        }
    }

    info!(
        "S3 WAL recovery: indexed {} of {} segments for {} namespaces",
        processed,
        segs.len(),
        namespaces.len()
    );
    info!(
        "S3 WAL recovery: committed {} offsets, {} total bytes",
        committed_offsets, bytes_total
    );
    if processed > 0 {
        let mut w = METRICS.write();
        w.wal_index_namespaces_total = namespaces.len() as u64;
        w.wal_index_files_total = processed;
        w.wal_index_bytes_total = bytes_total;
    }
    offsets_db.flush();
    if is_s3_wal() {
        log_wal_s3_memory_obs("after_wal_recover_s3");
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
struct WalIndexMetric {
    namespace: String,
    partitions: u64,
    files: u64,
    bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct WalIndexMetrics {
    metrics: Vec<WalIndexMetric>,
}

impl WalIndexMetrics {
    pub fn new() -> Self {
        WalIndexMetrics {
            metrics: Vec::new(),
        }
    }
}

pub async fn wal_recover(offsets_db: Arc<Offsets>) -> io::Result<()> {
    {
        let mut index = compaction_index();
        index.clear_for_recovery();
        metrics_hot::set_compaction_planner_ready_work_count(0);
    }
    Buffers::ensure_manifest_index_loaded();
    if is_s3_wal() {
        return wal_recover_s3(offsets_db).await;
    }
    wal_recover_disk(offsets_db)
}

#[cfg(test)]
mod wal_index_progress_tests {
    use super::wal_index_progress_every;
    use crate::helpers::configuration::Config;
    use serial_test::serial;

    #[test]
    #[serial]
    fn wal_index_progress_every_defaults_to_500() {
        let old = std::env::var("WAL_INDEX_PROGRESS_EVERY").ok();
        std::env::remove_var("WAL_INDEX_PROGRESS_EVERY");
        Config::set_evncache("WAL_INDEX_PROGRESS_EVERY", "");
        assert_eq!(wal_index_progress_every(), 500);
        if let Some(value) = old {
            Config::setenv("WAL_INDEX_PROGRESS_EVERY", &value);
        } else {
            std::env::remove_var("WAL_INDEX_PROGRESS_EVERY");
            Config::set_evncache("WAL_INDEX_PROGRESS_EVERY", "");
        }
    }

    #[test]
    #[serial]
    fn wal_index_progress_every_honors_env_override() {
        let old = std::env::var("WAL_INDEX_PROGRESS_EVERY").ok();
        Config::setenv("WAL_INDEX_PROGRESS_EVERY", "1000");
        assert_eq!(wal_index_progress_every(), 1000);
        if let Some(value) = old {
            Config::setenv("WAL_INDEX_PROGRESS_EVERY", &value);
        } else {
            std::env::remove_var("WAL_INDEX_PROGRESS_EVERY");
            Config::set_evncache("WAL_INDEX_PROGRESS_EVERY", "");
        }
    }
}

#[cfg(test)]
mod tests_wal_commit {
    use super::*;
    use crate::buffer::compaction_transaction::CompactionTransactionState;
    use crate::buffer::segment_file::SegmentFile;
    use crate::helpers::configuration::Config;
    use crate::plugins::SinkWriteOutcome;
    use arrow::array::Int32Array;
    use arrow::record_batch::RecordBatch;
    use arrow_schema::{DataType, Field, Schema};
    use async_trait::async_trait;
    use serial_test::serial;
    use std::collections::HashMap as StdHashMap;
    use std::fs;
    use std::sync::Arc;
    use std::sync::{Mutex, MutexGuard, OnceLock};
    use zerocopy::LayoutVerified;

    fn temp_dir() -> PathBuf {
        let base = std::env::temp_dir().join(format!("skippr_test_{}", rand::random::<u64>()));
        let _ = fs::create_dir_all(&base);
        base
    }

    fn make_batch() -> RecordBatch {
        let schema = Schema::new(vec![Field::new("v", DataType::Int32, false)]);
        let arr = Int32Array::from(vec![1, 2, 3]);
        RecordBatch::try_new(Arc::new(schema), vec![Arc::new(arr)]).unwrap()
    }

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    fn env_lock() -> MutexGuard<'static, ()> {
        ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    struct EnvGuard {
        old_data_dir: Option<String>,
        _lock: MutexGuard<'static, ()>,
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(ref v) = self.old_data_dir {
                Config::setenv("DATA_DIR", v);
            } else {
                std::env::remove_var("DATA_DIR");
                // Clear the cache entry too; get_envcache("DATA_DIR") == "" causes
                // get_pipeline_data_dir() to fall back to the default, which is what
                // subsequent tests expect when DATA_DIR was never set before this test ran.
                Config::set_evncache("DATA_DIR", "");
            }
        }
    }

    fn setup_data_dir() -> (PathBuf, EnvGuard) {
        // NOTE: Config/Data-dir is env-backed and process-global; tests run in parallel by default.
        // Hold a global lock so concurrent tests don't stomp each other's DATA_DIR and end up opening
        // the same sled DB concurrently (sled forbids that and returns AlreadyOpenError).
        let lock = env_lock();
        let td = temp_dir();
        let old_data_dir = std::env::var("DATA_DIR").ok();
        Config::setenv("DATA_DIR", td.to_str().unwrap());
        let seg_dir = PathBuf::from(format!("{}/segment_buffer/segs", Config::get_data_dir()));
        let _ = fs::create_dir_all(&seg_dir);
        // ensure clean
        if let Ok(rd) = fs::read_dir(&seg_dir) {
            for e in rd.flatten() {
                let _ = fs::remove_file(e.path());
            }
        }
        (
            seg_dir,
            EnvGuard {
                old_data_dir,
                _lock: lock,
            },
        )
    }

    fn reset_in_memory_segments() {
        {
            let mut live = SEGMENT_LIVE.lock().unwrap();
            *live = GlobalSegment::new();
        }
        SEGMENT_CACHE.clear();
        compaction_index().clear_all();
        metrics_hot::set_compaction_planner_ready_work_count(0);
        s3_wal_body_cache::clear_for_tests();
    }

    fn test_part(
        namespace: &str,
        time: i64,
        schema_fingerprint: &str,
        start: u64,
    ) -> SegmentPartitionIndexEntry {
        SegmentPartitionIndexEntry {
            key: PartitionKey {
                sink_ref: "data_sinks.ds_datalake".to_string(),
                namespace: namespace.to_string(),
                partition: "".to_string(),
                time: Some(time),
                schema_fingerprint: schema_fingerprint.to_string(),
            },
            bytes: 1024,
            updated_at_secs: 0,
            slice_ordinal: 0,
            part_meta_start: 0,
            part_meta_len: 0,
            part_meta_summary: SegmentPartMetaSummary::Append,
            start,
            len: 1024,
        }
    }

    fn test_meta(index: Vec<SegmentPartitionIndexEntry>) -> SegmentFileMetadata {
        SegmentFileMetadata {
            created_at_secs: 0,
            total_bytes: index.iter().map(|idx| idx.bytes).sum(),
            num_partitions: index.len() as u32,
            offsets: StdHashMap::new(),
            index,
        }
    }

    fn commit_exists(seg_path: &PathBuf) -> bool {
        seg_path.with_extension("seg.commit").exists()
    }

    struct SyntheticDiskBacklog {
        segment_path: PathBuf,
        meta: SegmentFileMetadata,
        entries: Vec<CompactionEntry>,
    }

    impl SyntheticDiskBacklog {
        fn entry_for_sink(&self, sink_ref: &str) -> CompactionEntry {
            self.entries
                .iter()
                .find(|entry| entry.idx.key.sink_ref == sink_ref)
                .cloned()
                .unwrap_or_else(|| panic!("missing synthetic WAL slice for {sink_ref}"))
        }

        fn work_for_sink(&self, sink_ref: &str) -> CompactionWork {
            let entries = vec![self.entry_for_sink(sink_ref)];
            let first = &entries[0];
            let txn = CompactionTransaction::new(
                first.idx.key.sink_ref.clone(),
                first.idx.key.namespace.clone(),
                first.idx.key.schema_fingerprint.clone(),
                crate::plugins::source_contract::WritePolicy::Append,
                SinkWriteSemantics::ExactOnce,
                entries.iter().map(|entry| entry.wal_ref.clone()).collect(),
                format!("synthetic-{}.parquet", first.wal_ref.segment_id),
            );
            CompactionWork { txn, entries }
        }

        fn tombstone_paths(&self) -> Vec<PathBuf> {
            self.entries
                .iter()
                .map(|entry| Buffers::tombstone_path_for_source(&entry.source, &entry.idx.key))
                .collect()
        }
    }

    fn synthetic_disk_backlog(
        base: &Path,
        segment_id: &str,
        sink_refs: &[&str],
    ) -> SyntheticDiskBacklog {
        let segf = SegmentFile::new(base, segment_id).unwrap();
        let mut batches = StdHashMap::new();
        let mut parts_meta = StdHashMap::new();
        for (index, sink_ref) in sink_refs.iter().enumerate() {
            let key = PartitionKey {
                sink_ref: (*sink_ref).to_string(),
                namespace: "synthetic_backlog".to_string(),
                partition: format!("slice-{index}"),
                time: Some(1_700_000_000),
                schema_fingerprint: "schema-v1".to_string(),
            };
            batches.insert(key.clone(), vec![make_batch()]);
            parts_meta.insert(key, (0, SystemTime::UNIX_EPOCH));
        }
        let offsets = StdHashMap::new();
        let part_meta_blobs = StdHashMap::new();
        let (meta, _rows, sha) = segf
            .write_snapshot(&offsets, &batches, &parts_meta, &part_meta_blobs)
            .unwrap();
        Buffers::write_seg_commit(&segf.path, &sha, meta.num_partitions, meta.total_bytes).unwrap();

        let source = SegmentSource::Disk(segf.path.clone());
        let entries = meta
            .index
            .iter()
            .cloned()
            .enumerate()
            .map(|(ordinal, idx)| {
                let wal_ref = WalPartRef {
                    segment_id: segment_id.to_string(),
                    source: SegmentSourceDescriptor::Disk {
                        path: segf.path.clone(),
                    },
                    start: idx.start,
                    len: idx.len,
                    key: idx.key.clone(),
                    cdc_meta_hash: None,
                };
                CompactionEntry {
                    source: source.clone(),
                    meta: meta.clone(),
                    ordinal,
                    idx,
                    wal_ref,
                }
            })
            .collect();

        SyntheticDiskBacklog {
            segment_path: segf.path,
            meta,
            entries,
        }
    }

    struct SyntheticGroupedSink {
        failure: Option<&'static str>,
    }

    #[async_trait]
    impl DataSink for SyntheticGroupedSink {
        async fn sync(
            &self,
            _stream: SendableRecordBatchStream,
            _filename: String,
            _cdc_ctx: Option<&crate::plugins::cdc::SyncContext>,
        ) -> io::Result<()> {
            Ok(())
        }

        async fn sync_grouped(
            &self,
            mut reader: crate::plugins::GroupedBatchReader,
            _ctx: crate::plugins::GroupedSinkWriteContext<'_>,
        ) -> io::Result<SinkWriteOutcome> {
            while reader.next_chunk().await?.is_some() {}
            match self.failure {
                Some(message) => Err(io::Error::other(message)),
                None => Ok(SinkWriteOutcome::Applied),
            }
        }

        fn capability(&self) -> &'static crate::plugins::cdc::SinkCapability {
            &crate::plugins::cdc::sink_capabilities::ICEBERG
        }
    }

    fn manifest_state(path: &Path) -> CompactionTransactionState {
        let bytes = fs::read(path).expect("compaction manifest should exist");
        serde_json::from_slice::<CompactionTransaction>(&bytes)
            .expect("compaction manifest should be valid")
            .state
    }

    #[test]
    fn grouped_compaction_key_ignores_unrelated_segment_contents() {
        let target_a = test_part("truck_status_idle", 1669334400, "schema-a", 0);
        let target_b = test_part("truck_status_idle", 1669334400, "schema-a", 4096);
        let meta_a = test_meta(vec![
            target_a.clone(),
            test_part("truck_rotation", 1669334400, "schema-b", 1024),
        ]);
        let meta_b = test_meta(vec![
            test_part("truck_geofence_enter", 1669334400, "schema-c", 2048),
            target_b.clone(),
        ]);

        let key_a = Buffers::grouping_key(
            &target_a,
            &Buffers::schema_fingerprint_for_group(&target_a, &meta_a),
        );
        let key_b = Buffers::grouping_key(
            &target_b,
            &Buffers::schema_fingerprint_for_group(&target_b, &meta_b),
        );

        assert_eq!(key_a, key_b);
    }

    #[test]
    fn grouped_compaction_key_separates_distinct_wal_partitions() {
        let mut part_a = test_part("cc_wat_source_pages_by_target_domain_index", 0, "schema", 0);
        part_a.key.partition =
            "p_crawl_id=cc_main_x/p_target_domain_hash_bucket=item_1".to_string();
        let mut part_b = test_part(
            "cc_wat_source_pages_by_target_domain_index",
            0,
            "schema",
            4096,
        );
        part_b.key.partition =
            "p_crawl_id=cc_main_x/p_target_domain_hash_bucket=item_2".to_string();
        let meta = test_meta(vec![part_a.clone(), part_b.clone()]);
        let key_a = Buffers::grouping_key(
            &part_a,
            &Buffers::schema_fingerprint_for_group(&part_a, &meta),
        );
        let key_b = Buffers::grouping_key(
            &part_b,
            &Buffers::schema_fingerprint_for_group(&part_b, &meta),
        );
        assert_ne!(key_a, key_b);
    }

    #[test]
    #[serial]
    fn test_write_seg_commit_header_roundtrip() {
        let (base, _guard) = setup_data_dir();
        let segf = SegmentFile::new(&base, "t1").unwrap();
        let key = PartitionKey {
            sink_ref: "data_outputs.test".to_string(),
            namespace: "ns".to_string(),
            partition: "".to_string(),
            time: Some(0),
            schema_fingerprint: "schema".to_string(),
        };
        let mut batches: StdHashMap<PartitionKey, Vec<RecordBatch>> = StdHashMap::new();
        batches.insert(key.clone(), vec![make_batch()]);
        let mut parts_meta: StdHashMap<PartitionKey, (u64, SystemTime)> = StdHashMap::new();
        parts_meta.insert(key.clone(), (0, SystemTime::now()));
        let offsets: StdHashMap<crate::helpers::offsets::OffsetKey, u64> = StdHashMap::new();
        let empty_blobs: StdHashMap<PartitionKey, Vec<u8>> = StdHashMap::new();
        let (meta, _rows, sha) = segf
            .write_snapshot(&offsets, &batches, &parts_meta, &empty_blobs)
            .unwrap();
        Buffers::write_seg_commit(&segf.path, &sha, meta.num_partitions, meta.total_bytes).unwrap();
        let (ver, _ts, size, pcount, got_sha) = Buffers::read_seg_commit(&segf.path).unwrap();
        assert_eq!(ver, 1);
        assert_eq!(size, meta.total_bytes);
        assert_eq!(pcount, meta.num_partitions);
        assert_eq!(got_sha, sha);
    }

    #[test]
    #[serial]
    fn test_wal_index_gating_with_commit() {
        let (base, _guard) = setup_data_dir();
        // Write segment without commit
        let segf = SegmentFile::new(&base, "t2").unwrap();
        let key = PartitionKey {
            sink_ref: "data_outputs.test".to_string(),
            namespace: "ns".to_string(),
            partition: "".to_string(),
            time: Some(0),
            schema_fingerprint: "schema".to_string(),
        };
        let mut batches: StdHashMap<PartitionKey, Vec<RecordBatch>> = StdHashMap::new();
        batches.insert(key.clone(), vec![make_batch()]);
        let mut parts_meta: StdHashMap<PartitionKey, (u64, SystemTime)> = StdHashMap::new();
        parts_meta.insert(key.clone(), (0, SystemTime::now()));
        let offsets: StdHashMap<crate::helpers::offsets::OffsetKey, u64> = StdHashMap::new();
        let empty_blobs: StdHashMap<PartitionKey, Vec<u8>> = StdHashMap::new();
        let (meta, _rows, sha) = segf
            .write_snapshot(&offsets, &batches, &parts_meta, &empty_blobs)
            .unwrap();
        assert!(!commit_exists(&segf.path));
        Buffers::write_seg_commit(&segf.path, &sha, meta.num_partitions, meta.total_bytes).unwrap();
        assert!(commit_exists(&segf.path));
    }

    #[test]
    #[serial]
    fn test_offsets_recovery_commit_windows() {
        let (base, _guard) = setup_data_dir();
        let segf1 = SegmentFile::new(&base, "t3").unwrap();
        let ok = crate::helpers::offsets::OffsetKey {
            namespace: "ns".to_string(),
            partition: "part".to_string(),
        };
        let mut offsets_map: StdHashMap<crate::helpers::offsets::OffsetKey, u64> =
            StdHashMap::new();
        offsets_map.insert(ok.clone(), 42);
        let key = PartitionKey {
            sink_ref: "data_outputs.test".to_string(),
            namespace: "ns".to_string(),
            partition: "part".to_string(),
            time: Some(0),
            schema_fingerprint: "schema".to_string(),
        };
        let mut batches: StdHashMap<PartitionKey, Vec<RecordBatch>> = StdHashMap::new();
        batches.insert(key.clone(), vec![make_batch()]);
        let mut parts_meta: StdHashMap<PartitionKey, (u64, SystemTime)> = StdHashMap::new();
        parts_meta.insert(key.clone(), (0, SystemTime::now()));
        let empty_blobs: StdHashMap<PartitionKey, Vec<u8>> = StdHashMap::new();
        let (meta1, _r1, sha1) = segf1
            .write_snapshot(&offsets_map, &batches, &parts_meta, &empty_blobs)
            .unwrap();
        let off = Arc::new(Offsets::init().unwrap());
        assert!(off.get(&ok).is_none());
        Buffers::write_seg_commit(&segf1.path, &sha1, meta1.num_partitions, meta1.total_bytes)
            .unwrap();
        for (k, pos) in offsets_map.iter() {
            let offset_key = crate::helpers::offsets::OffsetKey {
                namespace: k.namespace.clone(),
                partition: k.partition.clone(),
            };
            // Write Closed first, then Position so line reflects the expected value
            off.insert(&offset_key, crate::helpers::offsets::OffsetTypes::Closed, 1);
            off.insert(
                &offset_key,
                crate::helpers::offsets::OffsetTypes::Position,
                *pos,
            );
        }
        let line = off.get_line(&ok).unwrap();
        assert_eq!(u64::from(line), 42);
        // Idempotent: run again using the same offsets_map, value unchanged
        for (k, pos) in offsets_map.iter() {
            let offset_key = crate::helpers::offsets::OffsetKey {
                namespace: k.namespace.clone(),
                partition: k.partition.clone(),
            };
            off.insert(&offset_key, crate::helpers::offsets::OffsetTypes::Closed, 1);
            off.insert(
                &offset_key,
                crate::helpers::offsets::OffsetTypes::Position,
                *pos,
            );
        }
        let line2 = off.get_line(&ok).unwrap();
        assert_eq!(u64::from(line2), 42);
    }

    #[test]
    #[serial]
    fn test_mark_offsets_durable_in_wal_sets_closed_to_one() {
        let (_base, _guard) = setup_data_dir();
        let off = Offsets::init().unwrap();
        let key = crate::helpers::offsets::OffsetKey {
            namespace: "ns".to_string(),
            partition: "part".to_string(),
        };
        let offsets_map = StdHashMap::from([(key.clone(), 42_u64)]);

        mark_offsets_durable_in_wal(&off, offsets_map.iter());

        let mut backing_bytes = off.get(&key).unwrap();
        let layout: LayoutVerified<&mut [u8], crate::helpers::offsets::OffsetValue> =
            LayoutVerified::new_unaligned(&mut *backing_bytes)
                .expect("offset bytes should fit schema");
        let value: &mut crate::helpers::offsets::OffsetValue = layout.into_mut();
        assert_eq!(value.line.get(), 42);
        assert_eq!(value.closed.get(), 1);
    }

    #[test]
    #[serial]
    fn test_flush_persists_live_segment_without_waiting_for_rotation() {
        let (base, _guard) = setup_data_dir();
        reset_in_memory_segments();

        let offsets_db = Arc::new(Offsets::init().unwrap());
        let offset_key = crate::helpers::offsets::OffsetKey {
            namespace: "ns".to_string(),
            partition: "object-1".to_string(),
        };
        let batch = make_batch();
        let schema = batch.schema();

        let ingest_batch = IngestBufferBatch {
            offsets: StdHashMap::from([(offset_key.clone(), 42_u64)]),
            sink_ref: "data_outputs.test".to_string(),
            _namespace: "ns".to_string(),
            _partition: "".to_string(),
            _time: Some(0),
            _schema_fingerprint: String::new(),
            schema,
            record_batches: Some(vec![batch]),
            cdc_rows: None,
            checkpoint_update: None,
        };

        Buffers::append_batches_to_live(vec![ingest_batch]).unwrap();
        assert_eq!(Buffers::segs_remaining(), 0);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(flush_all_segments_direct(offsets_db.clone()))
            .unwrap();

        let seg_paths: Vec<PathBuf> = fs::read_dir(&base)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|s| s.to_str()) == Some("seg"))
            .collect();

        assert_eq!(seg_paths.len(), 1);
        assert!(commit_exists(&seg_paths[0]));
        assert_eq!(
            offsets_db.validate(&offset_key, crate::helpers::offsets::OffsetTypes::Closed, 1),
            Some(true)
        );
        let line = offsets_db.get_line(&offset_key).unwrap();
        assert_eq!(u64::from(line), 42);
    }

    #[test]
    #[serial]
    fn scheduler_uses_indexed_cdc_summaries_without_full_segment_scans() {
        use crate::plugins::cdc::{MutationKind, WalPartMeta, WalRowMeta};

        const PARTS: usize = 8;
        let (base, _guard) = setup_data_dir();
        reset_in_memory_segments();
        crate::metrics::counters::reset_flush_metrics();

        let segf = SegmentFile::new(&base, "indexed-cdc").unwrap();
        let mut batches = StdHashMap::new();
        let mut parts_meta = StdHashMap::new();
        let mut part_meta_blobs = StdHashMap::new();
        for ordinal in 0..PARTS {
            let key = PartitionKey {
                sink_ref: "data_sinks.ds_datalake".to_string(),
                namespace: "indexed_cdc".to_string(),
                partition: format!("slice-{ordinal}"),
                time: Some(1_700_000_000),
                schema_fingerprint: "schema-v1".to_string(),
            };
            batches.insert(key.clone(), vec![make_batch()]);
            parts_meta.insert(key.clone(), (1024, SystemTime::UNIX_EPOCH));
            let rows = (0..3)
                .map(|row| WalRowMeta {
                    mutation: MutationKind::Insert,
                    event_id: vec![ordinal as u8, row],
                    order_token: vec![ordinal as u8, row],
                })
                .collect();
            let meta = WalPartMeta::cdc(rows, 3).unwrap();
            part_meta_blobs.insert(key, bincode::serialize(&meta).unwrap());
        }
        let offsets = StdHashMap::new();
        let (meta, _rows, sha) = segf
            .write_snapshot(&offsets, &batches, &parts_meta, &part_meta_blobs)
            .unwrap();
        Buffers::write_seg_commit(&segf.path, &sha, meta.num_partitions, meta.total_bytes).unwrap();
        assert_eq!(meta.index.len(), PARTS);
        assert!(meta
            .index
            .iter()
            .all(|index| index.part_meta_summary.is_cdc()));
        Buffers::segment_cache_register(SegmentSource::Disk(segf.path.clone()), meta);

        SegmentFile::reset_full_part_meta_scan_count();
        let sink = SyntheticGroupedSink { failure: None };
        let works = Buffers::next_compaction_transactions(
            PARTS,
            true,
            &sink,
            &HashMap::new(),
            &HashSet::new(),
            PARTS,
        );
        assert_eq!(
            works.iter().map(|work| work.entries.len()).sum::<usize>(),
            PARTS
        );
        assert_eq!(SegmentFile::full_part_meta_scan_count(), 0);
        assert!(Buffers::next_compaction_transactions(
            PARTS,
            true,
            &sink,
            &HashMap::new(),
            &HashSet::new(),
            PARTS,
        )
        .is_empty());
        let planner = crate::metrics::counters::flush_metrics_snapshot();
        assert_eq!(planner.compaction_planner_cycles_total, 2);
        assert_eq!(planner.compaction_planner_segments_examined_total, 1);
        assert_eq!(
            planner.compaction_planner_slices_examined_total,
            PARTS as u64
        );
        assert_eq!(planner.compaction_planner_ready_work_count, 0);

        reset_in_memory_segments();
    }

    #[test]
    #[serial]
    fn test_remove_disk_segment_and_commit_marker_removes_existing_files() {
        let (base, _guard) = setup_data_dir();
        let seg_path = base.join("existing.seg");
        let commit_path = seg_path.with_extension("seg.commit");
        fs::write(&seg_path, b"segment").unwrap();
        fs::write(&commit_path, b"commit").unwrap();

        let cleanup = Buffers::remove_disk_segment_and_commit_marker(&seg_path).unwrap();

        assert_eq!(cleanup, DiskSegmentCleanup::Removed);
        assert!(!seg_path.exists());
        assert!(!commit_path.exists());
    }

    #[test]
    #[serial]
    fn test_remove_disk_segment_and_commit_marker_treats_missing_segment_as_success() {
        let (base, _guard) = setup_data_dir();
        let seg_path = base.join("missing.seg");
        let commit_path = seg_path.with_extension("seg.commit");
        fs::write(&commit_path, b"commit").unwrap();

        let cleanup = Buffers::remove_disk_segment_and_commit_marker(&seg_path).unwrap();

        assert_eq!(cleanup, DiskSegmentCleanup::AlreadyMissing);
        assert!(!seg_path.exists());
        assert!(!commit_path.exists());
    }

    #[test]
    #[serial]
    fn disk_segment_survives_partial_tombstones_across_sink_refs() {
        let (base, _guard) = setup_data_dir();
        reset_in_memory_segments();
        let backlog = synthetic_disk_backlog(
            &base,
            "multi-sink-segment",
            &["data_sinks.ds_datalake", "deadletter_sinks.ds_deadletters"],
        );
        assert_eq!(backlog.meta.index.len(), 2);
        let tombstones = backlog.tombstone_paths();
        let ledger = Buffers::completion_ledger();
        let bitmap_path = ledger.bitmap_path("multi-sink-segment");

        Buffers::tombstone_grouped_work(&backlog.work_for_sink("data_sinks.ds_datalake")).unwrap();

        assert!(backlog.segment_path.exists());
        assert!(commit_exists(&backlog.segment_path));
        assert!(bitmap_path.exists());
        assert_eq!(
            tombstones.iter().filter(|path| path.exists()).count(),
            1,
            "one durable slice must remain before segment deletion"
        );
        ledger.forget_segment("multi-sink-segment");
        assert!(
            backlog.entries.iter().any(|entry| ledger
                .is_complete("multi-sink-segment", &backlog.meta.index, entry.ordinal)
                .unwrap()),
            "partial completion bits must survive a cache reset"
        );
        assert!(!ledger
            .all_complete("multi-sink-segment", &backlog.meta.index)
            .unwrap());

        Buffers::tombstone_grouped_work(&backlog.work_for_sink("deadletter_sinks.ds_deadletters"))
            .unwrap();

        assert!(!backlog.segment_path.exists());
        assert!(!commit_exists(&backlog.segment_path));
        assert!(
            tombstones.iter().all(|path| !path.exists()),
            "segment cleanup removes tombstones only after every indexed slice is durable"
        );
        assert!(
            !bitmap_path.exists(),
            "segment cleanup removes the completion bitmap after deletion"
        );
        reset_in_memory_segments();
    }

    #[test]
    #[serial]
    fn corrupted_completion_bitmap_fails_safe_and_retains_segment() {
        let (base, _guard) = setup_data_dir();
        reset_in_memory_segments();
        let backlog =
            synthetic_disk_backlog(&base, "corrupt-completion", &["data_sinks.ds_datalake"]);
        let ledger = Buffers::completion_ledger();
        fs::create_dir_all(Buffers::tombstone_dir()).unwrap();
        fs::write(ledger.bitmap_path("corrupt-completion"), b"SCBL\x01").unwrap();

        let result =
            Buffers::tombstone_grouped_work(&backlog.work_for_sink("data_sinks.ds_datalake"));
        assert!(result.is_err());

        assert!(backlog.segment_path.exists());
        assert!(commit_exists(&backlog.segment_path));
        assert!(
            ledger
                .all_complete("corrupt-completion", &backlog.meta.index)
                .is_err(),
            "truncated completion state must not be interpreted as complete"
        );
        assert!(
            backlog.tombstone_paths().iter().all(|path| !path.exists()),
            "a corrupt bitmap must not be overwritten through rollback tombstones"
        );
        reset_in_memory_segments();
    }

    #[test]
    #[serial]
    fn grouped_compaction_acks_before_tombstoning_after_sink_success() {
        let (base, _guard) = setup_data_dir();
        reset_in_memory_segments();
        let backlog =
            synthetic_disk_backlog(&base, "successful-group", &["data_sinks.ds_datalake"]);
        let work = backlog.work_for_sink("data_sinks.ds_datalake");
        let manifest_path = crate::buffer::compaction_transaction::manifest_path_for(
            &crate::buffer::compaction_transaction::manifest_dir(),
            &work.txn.id,
        );
        let segment_path = backlog.segment_path.clone();
        let commit_path = segment_path.with_extension("seg.commit");
        let tombstones = backlog.tombstone_paths();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_for_hook = observed.clone();
        let manifest_for_hook = manifest_path.clone();
        let observer: GroupedCompactionTestObserver = Arc::new(move |event| {
            observed_for_hook.lock().unwrap().push(event);
            match event {
                GroupedCompactionTestEvent::SinkSucceeded => {
                    assert_eq!(
                        manifest_state(&manifest_for_hook),
                        CompactionTransactionState::Sent
                    );
                    assert!(segment_path.exists());
                    assert!(tombstones.iter().all(|path| !path.exists()));
                }
                GroupedCompactionTestEvent::ManifestAcked => {
                    assert_eq!(
                        manifest_state(&manifest_for_hook),
                        CompactionTransactionState::Acked
                    );
                    assert!(segment_path.exists());
                    assert!(tombstones.iter().all(|path| !path.exists()));
                }
                GroupedCompactionTestEvent::SlicesTombstoned => {
                    assert_eq!(
                        manifest_state(&manifest_for_hook),
                        CompactionTransactionState::Acked
                    );
                    assert!(!segment_path.exists());
                    assert!(!commit_path.exists());
                }
            }
        });
        let sink: Arc<Box<dyn DataSink + Send + Sync>> =
            Arc::new(Box::new(SyntheticGroupedSink { failure: None }));
        let budget = Arc::new(FlushExecutionBudget::new(1, 1));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let compacted = runtime
            .block_on(
                GROUPED_COMPACTION_TEST_OBSERVER
                    .scope(observer, Buffers::compact_grouped_work(work, sink, budget)),
            )
            .unwrap();

        assert!(compacted);
        assert_eq!(
            *observed.lock().unwrap(),
            vec![
                GroupedCompactionTestEvent::SinkSucceeded,
                GroupedCompactionTestEvent::ManifestAcked,
                GroupedCompactionTestEvent::SlicesTombstoned,
            ]
        );
        assert!(!manifest_path.exists());
        reset_in_memory_segments();
    }

    #[test]
    #[serial]
    fn grouped_compaction_failure_preserves_slices_and_segment() {
        let (base, _guard) = setup_data_dir();
        reset_in_memory_segments();
        let backlog = synthetic_disk_backlog(&base, "failed-group", &["data_sinks.ds_datalake"]);
        let work = backlog.work_for_sink("data_sinks.ds_datalake");
        let failure_key = format!("{}:{}", work.txn.sink_ref, work.txn.id);
        let manifest_path = crate::buffer::compaction_transaction::manifest_path_for(
            &crate::buffer::compaction_transaction::manifest_dir(),
            &work.txn.id,
        );
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_for_hook = observed.clone();
        let observer: GroupedCompactionTestObserver = Arc::new(move |event| {
            observed_for_hook.lock().unwrap().push(event);
        });
        let sink: Arc<Box<dyn DataSink + Send + Sync>> = Arc::new(Box::new(SyntheticGroupedSink {
            failure: Some("synthetic sink failure"),
        }));
        let budget = Arc::new(FlushExecutionBudget::new(1, 1));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let error = runtime
            .block_on(
                GROUPED_COMPACTION_TEST_OBSERVER
                    .scope(observer, Buffers::compact_grouped_work(work, sink, budget)),
            )
            .unwrap_err();

        assert!(error.to_string().contains("synthetic sink failure"));
        assert!(observed.lock().unwrap().is_empty());
        assert!(backlog.segment_path.exists());
        assert!(commit_exists(&backlog.segment_path));
        assert!(backlog.tombstone_paths().iter().all(|path| !path.exists()));
        assert_eq!(
            manifest_state(&manifest_path),
            CompactionTransactionState::Sent
        );
        COMPACT_FAILURES.remove(&failure_key);
        fs::remove_file(manifest_path).unwrap();
        reset_in_memory_segments();
    }
}

#[cfg(test)]
mod compaction_semantics_tests {
    use super::*;
    use crate::buffer::compaction_transaction::{
        SegmentSourceDescriptor, SinkWriteSemantics, WalPartRef,
    };
    use crate::plugins::cdc::sink_capabilities;
    use crate::plugins::DataSink;
    use async_trait::async_trait;
    use datafusion::execution::SendableRecordBatchStream;
    use std::collections::HashMap;

    struct MockMultiSink {
        primary: &'static crate::plugins::cdc::SinkCapability,
        by_ref: HashMap<String, &'static crate::plugins::cdc::SinkCapability>,
    }

    #[async_trait]
    impl DataSink for MockMultiSink {
        async fn sync(
            &self,
            _stream: SendableRecordBatchStream,
            _filename: String,
            _cdc_ctx: Option<&crate::plugins::cdc::SyncContext>,
        ) -> Result<(), std::io::Error> {
            Ok(())
        }

        async fn sync_grouped(
            &self,
            mut reader: crate::plugins::GroupedBatchReader,
            _ctx: crate::plugins::GroupedSinkWriteContext<'_>,
        ) -> Result<crate::plugins::SinkWriteOutcome, std::io::Error> {
            while let Some(_chunk) = reader.next_chunk().await? {}
            Ok(crate::plugins::SinkWriteOutcome::Applied)
        }

        fn capability(&self) -> &'static crate::plugins::cdc::SinkCapability {
            self.primary
        }

        fn capability_for_sink_ref(
            &self,
            sink_ref: &str,
        ) -> Option<&'static crate::plugins::cdc::SinkCapability> {
            self.by_ref.get(sink_ref).copied()
        }
    }

    fn test_entry(sink_ref: &str) -> CompactionEntry {
        let key = PartitionKey {
            sink_ref: sink_ref.to_string(),
            namespace: "ns".to_string(),
            partition: String::new(),
            time: Some(0),
            schema_fingerprint: "fp".to_string(),
        };
        let idx = SegmentPartitionIndexEntry {
            key: key.clone(),
            bytes: 100,
            updated_at_secs: 0,
            slice_ordinal: 0,
            part_meta_start: 0,
            part_meta_len: 0,
            part_meta_summary: SegmentPartMetaSummary::Append,
            start: 0,
            len: 100,
        };
        let wal_ref = WalPartRef {
            segment_id: "seg-1".to_string(),
            source: SegmentSourceDescriptor::Disk {
                path: PathBuf::from("/tmp/test.seg"),
            },
            start: 0,
            len: 100,
            key,
            cdc_meta_hash: None,
        };
        CompactionEntry {
            source: SegmentSource::Disk(PathBuf::from("/tmp/test.seg")),
            meta: SegmentFileMetadata {
                created_at_secs: 0,
                total_bytes: 100,
                num_partitions: 1,
                offsets: HashMap::new(),
                index: vec![idx.clone()],
            },
            ordinal: 0,
            idx,
            wal_ref,
        }
    }

    fn mock_router_sink() -> MockMultiSink {
        MockMultiSink {
            primary: &sink_capabilities::ICEBERG,
            by_ref: HashMap::from([
                (
                    "data_sinks.ds_datalake".to_string(),
                    &sink_capabilities::ICEBERG,
                ),
                (
                    "deadletter_sinks.ds_deadletters".to_string(),
                    &sink_capabilities::ATHENA,
                ),
            ]),
        }
    }

    #[test]
    fn grouped_write_semantics_athena_not_exact_once() {
        use crate::buffer::compaction_transaction::SinkWriteSemantics;
        use crate::plugins::cdc::sink_capabilities;
        assert_eq!(
            Buffers::grouped_write_semantics(&sink_capabilities::ATHENA),
            SinkWriteSemantics::IdempotentAtLeastOnce
        );
        assert_eq!(
            Buffers::grouped_write_semantics(&sink_capabilities::ICEBERG),
            SinkWriteSemantics::ExactOnce
        );
    }

    #[test]
    fn build_compaction_work_uses_per_sink_semantics() {
        let output = mock_router_sink();
        let iceberg = Buffers::build_compaction_work(
            vec![test_entry("data_sinks.ds_datalake")],
            String::new(),
            &output,
        )
        .expect("iceberg work");
        assert_eq!(iceberg.txn.semantics, SinkWriteSemantics::ExactOnce);

        let athena = Buffers::build_compaction_work(
            vec![test_entry("deadletter_sinks.ds_deadletters")],
            String::new(),
            &output,
        )
        .expect("athena work");
        assert_eq!(
            athena.txn.semantics,
            SinkWriteSemantics::IdempotentAtLeastOnce
        );
    }

    #[test]
    fn build_compaction_work_skips_unknown_sink_ref() {
        let output = mock_router_sink();
        assert!(Buffers::build_compaction_work(
            vec![test_entry("missing.sink")],
            String::new(),
            &output,
        )
        .is_none());
    }

    #[test]
    fn patch_manifest_semantics_upgrades_stale_exact_once_for_athena() {
        let output = mock_router_sink();
        let refs = vec![WalPartRef {
            segment_id: "seg-1".to_string(),
            source: SegmentSourceDescriptor::Disk {
                path: PathBuf::from("/tmp/test.seg"),
            },
            start: 0,
            len: 100,
            key: PartitionKey {
                sink_ref: "deadletter_sinks.ds_deadletters".to_string(),
                namespace: "ns".to_string(),
                partition: String::new(),
                time: Some(0),
                schema_fingerprint: "fp".to_string(),
            },
            cdc_meta_hash: None,
        }];
        let txn = CompactionTransaction::new(
            "deadletter_sinks.ds_deadletters".to_string(),
            "ns".to_string(),
            "fp".to_string(),
            crate::plugins::source_contract::WritePolicy::Append,
            SinkWriteSemantics::ExactOnce,
            refs,
            "out.parquet".to_string(),
        );
        let patched = Buffers::patch_manifest_semantics(txn, &output).expect("patched");
        assert_eq!(patched.semantics, SinkWriteSemantics::IdempotentAtLeastOnce);
    }

    #[test]
    #[serial_test::serial]
    fn wal_write_metrics_record_persisted_bytes() {
        let bytes_before = metrics_hot::WAL_WRITE_BYTES_TOTAL.load(AtomicOrdering::Relaxed);
        let rows_before = metrics_hot::WAL_WRITE_ROWS_TOTAL.load(AtomicOrdering::Relaxed);

        record_wal_write_metrics(7, 4096);

        assert_eq!(
            metrics_hot::WAL_WRITE_BYTES_TOTAL.load(AtomicOrdering::Relaxed),
            bytes_before + 4096
        );
        assert_eq!(
            metrics_hot::WAL_WRITE_ROWS_TOTAL.load(AtomicOrdering::Relaxed),
            rows_before + 7
        );
    }
}

/// Force-flush all segments to WAL files regardless of thresholds.
pub async fn flush_all_segments(offsets_db: Arc<Offsets>) -> Result<(), ArrowError> {
    crate::buffer::wal_writer::flush_and_drain(offsets_db).await
}

/// Force-flush all segments without going through the WAL writer command queue.
/// Used by the writer itself and as a fallback before the writer has started.
pub(crate) async fn flush_all_segments_direct(offsets_db: Arc<Offsets>) -> Result<(), ArrowError> {
    let (rows, uploaded_bytes) = Buffers::flush_live_segment_to_wal(offsets_db.as_ref()).await?;

    record_wal_write_metrics(rows, uploaded_bytes);
    WAL_BYTES_TOTAL.fetch_add(uploaded_bytes, std::sync::atomic::Ordering::Relaxed);

    Ok(())
}
