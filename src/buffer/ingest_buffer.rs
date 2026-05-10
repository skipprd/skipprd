#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PersistenceState {
    MemoryOnly,
    Persisted,
}
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
use crate::plugins::DataSink;
use crate::METRICS;
use arrow::array::RecordBatch;
use arrow::ipc::reader::StreamReader;
use arrow::ipc::writer::{IpcWriteOptions, StreamWriter};
use arrow_schema::{ArrowError, SchemaRef};
use dashmap::DashMap;
use datafusion::error::DataFusionError;
use datafusion::physical_plan::RecordBatchStream;
use datafusion::physical_plan::SendableRecordBatchStream;
use datafusion::prelude::{SessionConfig, SessionContext};
use futures::stream::StreamExt as FuturesStreamExt;
use hex;
use once_cell::sync::Lazy;
use once_cell::sync::Lazy as OnceLazy;
use serde_derive::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::{BufReader, Read, Seek, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll as TaskPoll};
use std::time::SystemTime;
use std::{fs, io};
use tokio::time::{sleep as tokio_sleep, Duration as TokioDuration};
use tracing::{debug, error, info, warn};

use tokio::sync::mpsc;

/// Identifies a WAL segment and provides access to its data.
/// `Disk` holds a local path; `S3` holds the object key, bucket, and full
/// in-memory bytes (downloaded once during candidate scanning).
#[derive(Clone)]
pub(crate) enum SegmentSource {
    Disk(PathBuf),
    S3 {
        key: String,
        bucket: String,
        data: Arc<Vec<u8>>,
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

    fn data_len(&self) -> io::Result<u64> {
        match self {
            SegmentSource::Disk(p) => {
                let file = OpenOptions::new().read(true).open(p)?;
                Ok(file.metadata()?.len())
            }
            SegmentSource::S3 { data, .. } => Ok(data.len() as u64),
        }
    }

    #[allow(dead_code)]
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

enum CompactorCommand {
    Wake,
    DrainAndStop(tokio::sync::oneshot::Sender<bool>),
}

// Per-partition notify for quick wakeups
#[allow(dead_code)]
static PARTITION_NOTIFIES: Lazy<DashMap<PartitionKey, Arc<tokio::sync::Notify>>> =
    Lazy::new(|| DashMap::new());

// Global in-memory segment accumulator (reduces tiny WAL files across threads)
static SEGMENT_LIVE: Lazy<Arc<std::sync::Mutex<GlobalSegment>>> =
    Lazy::new(|| Arc::new(std::sync::Mutex::new(GlobalSegment::new())));
// Segment snapshot representation
#[derive(Clone)]
#[allow(dead_code)]
struct SegmentPartitionMeta {
    bytes: u64,
    updated_at: SystemTime,
}

#[derive(Clone)]
#[allow(dead_code)]
struct SegmentSnapshot {
    id: String,
    persistence: PersistenceState,
    created_at: SystemTime,
    updated_at: SystemTime,
    total_bytes: u64,
    offsets: HashMap<OffsetKey, u64>,
    batches: HashMap<PartitionKey, Vec<RecordBatch>>,
    meta: HashMap<PartitionKey, SegmentPartitionMeta>,
    part_meta_blobs: HashMap<PartitionKey, Vec<u8>>,
}

impl SegmentSnapshot {
    fn new(
        id: String,
        offsets: HashMap<OffsetKey, u64>,
        batches: HashMap<PartitionKey, Vec<RecordBatch>>,
        meta: HashMap<PartitionKey, SegmentPartitionMeta>,
        total_bytes: u64,
        part_meta_blobs: HashMap<PartitionKey, Vec<u8>>,
    ) -> Self {
        let now = SystemTime::now();
        SegmentSnapshot {
            id,
            persistence: PersistenceState::MemoryOnly,
            created_at: now,
            updated_at: now,
            total_bytes,
            offsets,
            batches,
            meta,
            part_meta_blobs,
        }
    }
}

// Queue of snapshots produced by rotation in write()
static SEGMENT_SNAPSHOTS: Lazy<
    std::sync::Mutex<std::collections::VecDeque<Arc<std::sync::Mutex<SegmentSnapshot>>>>,
> = Lazy::new(|| std::sync::Mutex::new(std::collections::VecDeque::with_capacity(8)));
// Global single-flight guard to avoid double compaction of the same partition region
static COMPACTION_IN_FLIGHT: OnceLazy<DashMap<(String, u64, u64), ()>> =
    OnceLazy::new(|| DashMap::new());

/// In-memory cache of committed WAL segments.
/// Populated on startup (wal_recover) and during ingest (flush).
/// Entries are removed when all partitions in a segment have been compacted.
struct CachedSegment {
    source: SegmentSource,
    meta: SegmentFileMetadata,
}

static SEGMENT_CACHE: OnceLazy<DashMap<String, CachedSegment>> = OnceLazy::new(DashMap::new);

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
}

pub struct IngestBufferBatch {
    pub(crate) offsets: HashMap<OffsetKey, u64>,
    pub(crate) sink_ref: String,
    pub(crate) _namespace: String,
    pub(crate) _partition: String,
    pub(crate) _time: Option<i64>,
    pub(crate) _shard: String,
    pub(crate) schema: SchemaRef,
    pub(crate) record_batches: Option<Vec<RecordBatch>>,
    /// Per-row CDC metadata aligned 1:1 with the rows in `record_batches`.
    /// `None` means append-mode (no CDC metadata).
    pub(crate) cdc_rows: Option<Vec<crate::plugins::cdc::WalRowMeta>>,
}

// Single global segment that aggregates batches for all partitions
struct GlobalSegment {
    batches: HashMap<PartitionKey, Vec<RecordBatch>>,
    offsets: HashMap<OffsetKey, u64>,
    cdc_meta: HashMap<PartitionKey, Vec<crate::plugins::cdc::WalRowMeta>>,
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
        self.bytes = self.bytes.saturating_add(add_bytes);
        self.updated_at = SystemTime::now();
    }
    fn take(
        &mut self,
    ) -> (
        HashMap<PartitionKey, Vec<RecordBatch>>,
        HashMap<OffsetKey, u64>,
        HashMap<PartitionKey, Vec<crate::plugins::cdc::WalRowMeta>>,
        u64,
    ) {
        let batches = std::mem::take(&mut self.batches);
        let offsets = std::mem::take(&mut self.offsets);
        let cdc_meta = std::mem::take(&mut self.cdc_meta);
        let bytes = std::mem::replace(&mut self.bytes, 0);
        self.updated_at = SystemTime::now();
        (batches, offsets, cdc_meta, bytes)
    }
}

fn serialize_cdc_meta_to_blobs(
    cdc_meta: &HashMap<PartitionKey, Vec<crate::plugins::cdc::WalRowMeta>>,
) -> HashMap<PartitionKey, Vec<u8>> {
    use crate::plugins::cdc::{WalPartKind, WalPartMeta};
    let mut blobs: HashMap<PartitionKey, Vec<u8>> = HashMap::new();
    for (key, rows) in cdc_meta.iter() {
        if rows.is_empty() {
            continue;
        }
        let meta = WalPartMeta {
            kind: WalPartKind::Cdc,
            row_count: rows.len() as u64,
            rows: rows.clone(),
        };
        if let Ok(bytes) = bincode::serialize(&meta) {
            blobs.insert(key.clone(), bytes);
        }
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

    pub fn new() -> Self {
        Buffers
    }

    // store selection moved to WalStoreFactory

    // moved to SegmentFile::build_commit_header_bytes

    // upload helpers removed; S3 writing is handled by WalStoreS3 via SegmentObject

    pub fn write(&mut self, batches: Vec<IngestBufferBatch>) {
        for mut ingest_buffer_batch in batches.into_iter() {
            let sink_ref = ingest_buffer_batch.sink_ref.clone();
            let namespace = ingest_buffer_batch._namespace.clone();
            let partition = ingest_buffer_batch._partition.clone();
            let time = ingest_buffer_batch._time.clone();
            // Ensure shard key reflects schema so schemas do not mix in one segment
            let shard = if ingest_buffer_batch._shard.is_empty() {
                schema_fingerprint(&ingest_buffer_batch.schema)
            } else {
                ingest_buffer_batch._shard.clone()
            };
            let key = PartitionKey {
                sink_ref,
                namespace,
                partition,
                time,
                shard,
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

            // Thresholds (apply in write): 4MB or 60s elapsed since last flush or update
            let byte_threshold = 4 * 1024 * 1024u64;
            let time_threshold = 60u64; // seconds default

            let mut seg = SEGMENT_LIVE.lock().unwrap();
            let age_secs = seg.flushed_at.elapsed().map(|d| d.as_secs()).unwrap_or(0);
            let last_update_elapsed = seg.updated_at.elapsed().map(|d| d.as_secs()).unwrap_or(0);
            let should_rotate = (seg.bytes >= byte_threshold
                || age_secs >= time_threshold
                || last_update_elapsed >= time_threshold)
                && seg.bytes > 0;
            if should_rotate {
                let (batches, offsets, cdc_meta, total_bytes) = seg.take();
                seg.flushed_at = SystemTime::now();
                let mut meta: HashMap<PartitionKey, SegmentPartitionMeta> = HashMap::new();
                for (k, v) in batches.iter() {
                    let bytes_estimate = v.iter().map(|b| b.get_array_memory_size() as u64).sum();
                    meta.insert(
                        k.clone(),
                        SegmentPartitionMeta {
                            bytes: bytes_estimate,
                            updated_at: SystemTime::now(),
                        },
                    );
                }
                let blobs = serialize_cdc_meta_to_blobs(&cdc_meta);
                let snapshot = SegmentSnapshot::new(
                    Helpers::random_str(16),
                    offsets,
                    batches,
                    meta,
                    total_bytes,
                    blobs,
                );
                if Config::log_wal_enabled() || Config::debug_enabled() {
                    let part_count = snapshot.meta.len();
                    let reason = if seg.bytes >= byte_threshold {
                        "size"
                    } else {
                        "time"
                    };
                    info!(
                        "Segment rotated: id={} reason={} total_bytes={} partitions={}",
                        snapshot.id, reason, snapshot.total_bytes, part_count
                    );
                }
                if let Ok(mut q) = SEGMENT_SNAPSHOTS.lock() {
                    q.push_back(Arc::new(std::sync::Mutex::new(snapshot)));
                }
            }
            seg.add(
                key,
                batches_vec.drain(..).collect(),
                &ingest_buffer_batch.offsets,
                ingest_buffer_batch.cdc_rows.take(),
            );
        }
    }

    pub async fn flush(
        &mut self,
        offsets_db: Arc<Offsets>,
        _shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> Result<(), ArrowError> {
        let bytes: u64 = 0;
        let mut rows: u64 = 0;
        let mut uploaded_bytes: u64 = 0;
        let (snapshot_rows, snapshot_bytes) =
            Self::flush_snapshot_queue_to_wal(offsets_db.as_ref()).await?;
        rows += snapshot_rows;
        uploaded_bytes += snapshot_bytes;

        // `flush()` is the durability boundary for ingest tasks. If the live segment never
        // reaches a later rotation point before a crash, it still needs to become WAL-visible.
        let (live_rows, live_bytes) = Self::flush_live_segment_to_wal(offsets_db.as_ref()).await?;
        rows += live_rows;
        uploaded_bytes += live_bytes;

        metrics_hot::add_wal_write_bytes(bytes);
        metrics_hot::add_wal_write_rows(rows);

        WAL_BYTES_TOTAL.fetch_add(uploaded_bytes, std::sync::atomic::Ordering::Relaxed);

        Buffers::wake_compactor();

        Ok(())
    }

    async fn persist_snapshot_to_wal(
        snapshot: &mut SegmentSnapshot,
        offsets_db: &Offsets,
        context: &str,
    ) -> Result<Option<SegmentWriteResult>, ArrowError> {
        if snapshot.persistence != PersistenceState::MemoryOnly {
            return Ok(None);
        }

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
                return Ok(None);
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
        snapshot.persistence = PersistenceState::Persisted;
        if Config::debug_enabled() || Config::log_wal_enabled() {
            info!(
                "WAL persist marking offsets durable id={} offset_sample={:?}",
                snapshot_id,
                offset_sample(snapshot.offsets.iter(), 3)
            );
            append_wal_debug_trace(&format!("persist_mark_offsets id={}", snapshot_id));
        }
        mark_offsets_durable_in_wal(offsets_db, snapshot.offsets.iter());
        if Config::debug_enabled() || Config::log_wal_enabled() {
            info!("WAL persist offsets durable id={}", snapshot_id);
            append_wal_debug_trace(&format!("persist_offsets_durable id={}", snapshot_id));
        }
        Self::segment_cache_register_from_write(&write_result);
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

        Ok(Some(write_result))
    }

    async fn flush_snapshot_queue_to_wal(offsets_db: &Offsets) -> Result<(u64, u64), ArrowError> {
        let mut rows: u64 = 0;
        let mut uploaded_bytes: u64 = 0;

        loop {
            let next_snapshot_opt = {
                let mut q = SEGMENT_SNAPSHOTS.lock().unwrap();
                q.pop_front()
            };
            let Some(snap_arc) = next_snapshot_opt else {
                break;
            };

            let mut snapshot = snap_arc.lock().unwrap();
            if let Some(write_result) =
                Self::persist_snapshot_to_wal(&mut snapshot, offsets_db, "drain rotated").await?
            {
                uploaded_bytes += write_result.meta.total_bytes;
                rows += write_result.total_rows;
            }
        }

        Ok((rows, uploaded_bytes))
    }

    async fn flush_live_segment_to_wal(offsets_db: &Offsets) -> Result<(u64, u64), ArrowError> {
        let (to_flush_batches, to_flush_offsets, to_flush_cdc, total_bytes) = {
            let mut guard = SEGMENT_LIVE.lock().unwrap();
            if guard.batches.is_empty() {
                return Ok((0, 0));
            }
            guard.flushed_at = SystemTime::now();
            let (b, o, c, bytes) = guard.take();
            (b, o, c, bytes)
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

        let mut snapshot = SegmentSnapshot::new(
            Helpers::random_str(16),
            to_flush_offsets,
            to_flush_batches,
            meta,
            total_bytes,
            serialize_cdc_meta_to_blobs(&to_flush_cdc),
        );

        match Self::persist_snapshot_to_wal(&mut snapshot, offsets_db, "live flush").await? {
            Some(result) => Ok((result.total_rows, result.meta.total_bytes)),
            None => Ok((0, 0)),
        }
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
        let num_cpus = num_cpus::get();
        let drain_started = std::time::Instant::now();
        let mut last_progress_log = std::time::Instant::now();
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
                    crate::ingest::tuner::drain_tick(num_cpus, has_backlog);
                    if last_progress_log.elapsed() >= std::time::Duration::from_secs(5) {
                        let remaining_sample = Self::reclaimable_wal_partition_count(10_000);
                        let compacted_started = crate::metrics::counters::WAL_COMPACTIONS_STARTED
                            .load(AtomicOrdering::Relaxed);
                        let compacted_completed =
                            crate::metrics::counters::WAL_COMPACTIONS_COMPLETED
                                .load(AtomicOrdering::Relaxed);
                        let failures = COMPACT_FAILURES.len();
                        info!(
                            "Compactor drain: waiting elapsed={:?} wal_in_flight={} uploads_in_flight={} reclaimable_wal={} remaining_partitions_sample={} compacted_started={} compacted_completed={} compact_failures={}",
                            drain_started.elapsed(),
                            wal_in_flight,
                            uploads_in_flight,
                            reclaimable_wal,
                            remaining_sample,
                            compacted_started,
                            compacted_completed,
                            failures
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
        loop {
            let force = drain_reply.is_some();
            let did_work =
                Self::run_compaction_cycle(force, shared_output.clone(), offsets_db.clone()).await;
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
                _ = tokio_sleep(TokioDuration::from_millis(500)) => {}
            }
        }
        COMPACTOR_STARTED.store(false, std::sync::atomic::Ordering::Relaxed);
    }

    async fn run_compaction_cycle(
        force: bool,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
        offsets_db: Arc<Offsets>,
    ) -> bool {
        use futures::stream::StreamExt;
        let mut made_progress = false;
        let concurrency = crate::metrics::counters::WAL_COMPACTION_CONCURRENCY_TARGET
            .load(std::sync::atomic::Ordering::Relaxed)
            .clamp(1, 64) as usize;
        if !force {
            let active_ingest =
                crate::metrics::counters::ACTIVE_THREADS.load(std::sync::atomic::Ordering::Relaxed);
            let queued_ingest =
                crate::metrics::counters::QUEUE_LENGTH.load(std::sync::atomic::Ordering::Relaxed);
            if active_ingest > 0 || queued_ingest > 0 {
                if Config::debug_enabled() || Config::log_wal_enabled() {
                    debug!(
                        "Compactor: paused while ingest is busy active_threads={} queued_tasks={}",
                        active_ingest, queued_ingest
                    );
                }
                return false;
            }
        }

        loop {
            let candidates = Self::next_compaction_candidates(concurrency, force);
            if candidates.is_empty() {
                break;
            }
            let mut cycle_progress = false;
            let mut in_flight: futures::stream::FuturesUnordered<
                Pin<Box<dyn Future<Output = bool> + Send>>,
            > = futures::stream::FuturesUnordered::new();
            for (source, meta, idx) in candidates {
                let out = shared_output.clone();
                let off = offsets_db.clone();
                in_flight.push(Box::pin(async move {
                    Self::compact_segment_partition_source(&source, &meta, &idx, out, off)
                        .await
                        .unwrap_or(false)
                }));
            }
            while let Some(compacted) = in_flight.next().await {
                cycle_progress |= compacted;
            }
            if !cycle_progress {
                break;
            }
            made_progress = true;
        }
        if made_progress && !is_s3_wal() {
            Self::sweep_segment_cleanup();
        }
        made_progress
    }

    #[allow(dead_code)]
    async fn compact_one_partition(
        force: bool,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
        offsets_db: Arc<Offsets>,
    ) -> io::Result<bool> {
        let candidates = Self::next_compaction_candidates(1, force);
        if let Some((source, meta, idx)) = candidates.into_iter().next() {
            let started = Self::compact_segment_partition_source(
                &source,
                &meta,
                &idx,
                shared_output,
                offsets_db,
            )
            .await
            .unwrap_or(false);
            return Ok(started);
        }
        Ok(false)
    }

    fn should_compact(bytes: u64, updated_at_secs: u64, now_secs: u64, force: bool) -> bool {
        if force {
            return true;
        }
        let byte_threshold = Config::get_pipeline_buffer_threshold_bytes();
        let time_threshold = Config::get_pipeline_buffer_threshold_seconds() as u64;
        bytes >= byte_threshold || now_secs.saturating_sub(updated_at_secs) >= time_threshold
    }

    // ── Segment cache: single write / single remove / unified read ──────

    fn segment_cache_register(source: SegmentSource, meta: SegmentFileMetadata) {
        let id = source.segment_id().to_string();
        SEGMENT_CACHE.insert(id, CachedSegment { source, meta });
    }

    fn segment_cache_remove(segment_id: &str) {
        SEGMENT_CACHE.remove(segment_id);
    }

    fn segment_cache_register_from_write(result: &SegmentWriteResult) {
        let source = match &result.location {
            SegmentWriteLocation::Disk { path } => SegmentSource::Disk(path.clone()),
            SegmentWriteLocation::S3 { key, bucket, data } => SegmentSource::S3 {
                key: key.clone(),
                bucket: bucket.clone(),
                data: Arc::new(data.clone()),
            },
        };
        Self::segment_cache_register(source, result.meta.clone());
    }

    fn next_compaction_candidates(
        limit: usize,
        force: bool,
    ) -> Vec<(
        SegmentSource,
        SegmentFileMetadata,
        SegmentPartitionIndexEntry,
    )> {
        let now_secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut out = Vec::with_capacity(limit);
        for entry in SEGMENT_CACHE.iter() {
            if out.len() >= limit {
                break;
            }
            let cached = entry.value();
            for idx in cached.meta.index.iter() {
                if Self::is_source_tombstoned(&cached.source, &idx.key) {
                    continue;
                }
                let inflight_key = (cached.source.display_name(), idx.start, idx.len);
                if COMPACTION_IN_FLIGHT.contains_key(&inflight_key) {
                    continue;
                }
                if Self::should_compact(idx.bytes, idx.updated_at_secs, now_secs, force) {
                    out.push((cached.source.clone(), cached.meta.clone(), idx.clone()));
                    if out.len() >= limit {
                        break;
                    }
                }
            }
        }
        out
    }

    pub fn has_reclaimable_wal() -> bool {
        !Self::next_compaction_candidates(1, true).is_empty()
    }

    fn reclaimable_wal_partition_count(limit: usize) -> usize {
        Self::next_compaction_candidates(limit, true).len()
    }

    fn tombstone_dir() -> PathBuf {
        PathBuf::from(format!("{}/segment_buffer/done", Config::get_data_dir()))
    }

    fn tombstone_path_for_id(segment_id: &str, key: &PartitionKey) -> PathBuf {
        let time = key.time.unwrap_or(0);
        let safe = |s: &str| s.replace('/', "_");
        let file = format!(
            "{}.seg.{}.{}.{}.{}.{}.tombstone",
            segment_id,
            safe(&key.sink_ref),
            safe(&key.namespace),
            safe(&key.partition),
            time,
            safe(&key.shard)
        );
        Buffers::tombstone_dir().join(file)
    }

    fn partition_tombstone_path(seg_path: &PathBuf, key: &PartitionKey) -> PathBuf {
        let id = seg_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");
        Self::tombstone_path_for_id(id, key)
    }

    fn tombstone_path_for_source(source: &SegmentSource, key: &PartitionKey) -> PathBuf {
        Self::tombstone_path_for_id(source.segment_id(), key)
    }

    fn is_partition_tombstoned(seg_path: &PathBuf, key: &PartitionKey) -> bool {
        Self::partition_tombstone_path(seg_path, key).exists()
    }

    fn is_source_tombstoned(source: &SegmentSource, key: &PartitionKey) -> bool {
        Self::tombstone_path_for_source(source, key).exists()
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

    fn compaction_id_for_source(
        source: &SegmentSource,
        idx: &crate::buffer::segment_file::SegmentPartitionIndexEntry,
    ) -> String {
        let mut hasher = Sha256::new();
        match source {
            SegmentSource::Disk(p) => {
                if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                    hasher.update(name.as_bytes());
                }
            }
            SegmentSource::S3 { key, .. } => {
                hasher.update(key.as_bytes());
            }
        }
        hasher.update(&idx.start.to_le_bytes());
        hasher.update(&idx.len.to_le_bytes());
        hasher.update(&idx.bytes.to_le_bytes());
        hasher.update(&idx.updated_at_secs.to_le_bytes());
        let digest = hasher.finalize();
        hex::encode(&digest[..8])
    }

    #[allow(dead_code)]
    fn compute_compaction_id(
        seg_path: &PathBuf,
        idx: &crate::buffer::segment_file::SegmentPartitionIndexEntry,
    ) -> String {
        Self::compaction_id_for_source(&SegmentSource::Disk(seg_path.clone()), idx)
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

    fn sweep_segment_cleanup() {
        let seg_dir = PathBuf::from(format!("{}/segment_buffer/segs", Config::get_data_dir()));
        if !seg_dir.exists() {
            return;
        }
        let dir_iter = match fs::read_dir(&seg_dir) {
            Ok(r) => r,
            Err(_) => return,
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
                    let mut remaining = 0usize;
                    for idx in m.index.iter() {
                        if !Self::is_partition_tombstoned(&p, &idx.key) {
                            remaining += 1;
                        }
                    }
                    if remaining == 0 {
                        match Self::remove_disk_segment_and_commit_marker(&p) {
                            Ok(cleanup) => {
                                if matches!(cleanup, DiskSegmentCleanup::Removed) {
                                    info!(
                                        "Removed fully-compacted segment {}",
                                        p.to_string_lossy()
                                    );
                                }
                                // remove all tombstones for this segment
                                for part in m.index.iter() {
                                    let tp = Self::partition_tombstone_path(&p, &part.key);
                                    if tp.exists() {
                                        if let Err(e) = fs::remove_file(&tp) {
                                            error!("Failed to remove tombstone {:?}: {}", tp, e);
                                        }
                                    }
                                }
                            }
                            Err(e) => error!(
                                "Failed to remove fully-compacted segment {}: {}",
                                p.to_string_lossy(),
                                e
                            ),
                        }
                    }
                }
            }
        }
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

    async fn compact_segment_partition_source(
        source: &SegmentSource,
        meta: &SegmentFileMetadata,
        idx: &crate::buffer::segment_file::SegmentPartitionIndexEntry,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
        _offsets_db: Arc<Offsets>,
    ) -> io::Result<bool> {
        let sink_ref = idx.key.sink_ref.clone();
        let namespace = idx.key.namespace.clone();
        let partition = idx.key.partition.clone();
        let time = idx.key.time;
        let shard = idx.key.shard.clone();
        let seg_display = source.display_name();
        let mut out_key = BufferChunker::encode_chunk_name(
            "output",
            Some(&sink_ref),
            Some(&namespace),
            Some(&partition),
            time,
            Some(&shard),
        );
        let compaction_id = Buffers::compaction_id_for_source(source, idx);
        out_key = format!("{}-c={}", out_key, compaction_id);

        crate::metrics::counters::add_wal_compaction_started(1);
        crate::metrics::counters::inc_wal_compactions_in_flight();

        let inflight_key = (seg_display.clone(), idx.start, idx.len);
        if COMPACTION_IN_FLIGHT
            .insert(inflight_key.clone(), ())
            .is_some()
        {
            if Config::debug_enabled() || Config::log_wal_enabled() {
                debug!(
                    "Compactor: skip duplicate in-flight seg={} start={} len={}",
                    seg_display, idx.start, idx.len
                );
            }
            crate::metrics::counters::dec_wal_compactions_in_flight();
            return Ok(false);
        }
        struct InflightGuard((String, u64, u64));
        impl Drop for InflightGuard {
            fn drop(&mut self) {
                COMPACTION_IN_FLIGHT.remove(&self.0);
                crate::metrics::counters::dec_wal_compactions_in_flight();
            }
        }
        let _guard = InflightGuard(inflight_key.clone());
        if Config::debug_enabled() || Config::log_wal_enabled() {
            debug!(
                "Compactor: start ns={} part={} time={} shard={} seg={} start={} len={} bytes={} out_key={}",
                namespace, partition, time.unwrap_or(0), shard, seg_display, idx.start, idx.len, idx.bytes, out_key
            );
        }

        let file_len = source.data_len()?;
        if idx.start >= file_len {
            error!(
                "Compactor: partition start beyond file end for {} start={} len={} file_len={}",
                seg_display, idx.start, idx.len, file_len
            );
            return Ok(false);
        }
        let safe_len = std::cmp::min(idx.len, file_len.saturating_sub(idx.start));
        if Config::debug_enabled() || Config::log_wal_enabled() {
            debug!(
                "Compactor: bounds seg={} file_len={} start={} idx_len={} safe_len={} end_hint={}",
                seg_display,
                file_len,
                idx.start,
                idx.len,
                safe_len,
                idx.start.saturating_add(safe_len)
            );
        }

        // ── Schema + batch-stream setup (branches on source type) ──────────
        let (tx, rx) = mpsc::channel::<Result<RecordBatch, DataFusionError>>(8);
        let start_pos = idx.start;
        let part_len = safe_len;

        let schema: SchemaRef = match source {
            SegmentSource::S3 { data, .. } => {
                let start = start_pos as usize;
                let end = start.saturating_add(part_len as usize).min(data.len());
                let mut cursor = io::Cursor::new(&data[start..end]);
                match StreamReader::try_new(&mut cursor, None) {
                    Ok(sr) => sr.schema(),
                    Err(_) => {
                        let mut cursor2 = io::Cursor::new(&data[start..]);
                        StreamReader::try_new(&mut cursor2, None)
                            .map(|sr| sr.schema())
                            .map_err(|e| {
                                io::Error::new(io::ErrorKind::Other, format!("arrow: {}", e))
                            })?
                    }
                }
            }
            SegmentSource::Disk(seg_path) => {
                let mut file = OpenOptions::new().read(true).open(seg_path)?;
                file.seek(io::SeekFrom::Start(start_pos))?;
                let reader = io::BufReader::new(file);
                use std::io::Read as IoRead;
                let mut take = reader.take(part_len);
                match StreamReader::try_new(&mut take, None) {
                    Ok(sr) => sr.schema(),
                    Err(e) => {
                        let es = e.to_string();
                        if es.contains("failed to fill whole buffer")
                            || es.contains("UnexpectedEof")
                        {
                            let mut file2 = OpenOptions::new().read(true).open(seg_path)?;
                            file2.seek(io::SeekFrom::Start(start_pos))?;
                            let reader2 = io::BufReader::new(file2);
                            StreamReader::try_new(reader2, None)
                                .map(|sr| sr.schema())
                                .map_err(|e2| {
                                    io::Error::new(io::ErrorKind::Other, format!("arrow: {}", e2))
                                })?
                        } else {
                            return Err(io::Error::new(
                                io::ErrorKind::Other,
                                format!("arrow: {}", e),
                            ));
                        }
                    }
                }
            }
        };

        // ── Spawn batch reader ─────────────────────────────────────────────
        match source {
            SegmentSource::S3 { data, .. } => {
                let data = data.clone();
                tokio::spawn(async move {
                    let start = start_pos as usize;
                    let end = start.saturating_add(part_len as usize).min(data.len());
                    let mut cursor = io::Cursor::new(&data[start..end]);
                    match StreamReader::try_new(&mut cursor, None) {
                        Ok(sr) => {
                            for item in sr {
                                match item {
                                    Ok(batch) => {
                                        if tx.send(Ok(batch)).await.is_err() {
                                            break;
                                        }
                                    }
                                    Err(e) => {
                                        let _ = tx
                                            .send(Err(DataFusionError::ArrowError(
                                                Box::new(e),
                                                None,
                                            )))
                                            .await;
                                        break;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            let _ = tx
                                .send(Err(DataFusionError::ArrowError(Box::new(e), None)))
                                .await;
                        }
                    }
                    drop(tx);
                });
            }
            SegmentSource::Disk(seg_path) => {
                let seg_path_clone = seg_path.clone();
                tokio::spawn(async move {
                    match OpenOptions::new().read(true).open(&seg_path_clone) {
                        Ok(mut file) => {
                            if let Err(e) = file.seek(io::SeekFrom::Start(start_pos)) {
                                let _ = tx.send(Err(DataFusionError::IoError(e)));
                                return;
                            }
                            let reader = io::BufReader::new(file);
                            use std::io::Read as IoRead;
                            let mut take = reader.take(part_len);
                            match StreamReader::try_new(&mut take, None) {
                                Ok(sr) => {
                                    let mut had_iter_error = false;
                                    for item in sr {
                                        match item {
                                            Ok(batch) => {
                                                if tx.send(Ok(batch)).await.is_err() {
                                                    break;
                                                }
                                            }
                                            Err(e) => {
                                                let es = e.to_string();
                                                had_iter_error = true;
                                                if es.contains("failed to fill whole buffer")
                                                    || es.contains("UnexpectedEof")
                                                {
                                                    match OpenOptions::new()
                                                        .read(true)
                                                        .open(&seg_path_clone)
                                                    {
                                                        Ok(mut f2) => {
                                                            if let Err(e) = f2.seek(
                                                                io::SeekFrom::Start(start_pos),
                                                            ) {
                                                                let _ = tx.send(Err(
                                                                    DataFusionError::IoError(e),
                                                                ));
                                                                break;
                                                            }
                                                            let reader2 = io::BufReader::new(f2);
                                                            match StreamReader::try_new(
                                                                reader2, None,
                                                            ) {
                                                                Ok(sr2) => {
                                                                    for item2 in sr2 {
                                                                        match item2 {
                                                                            Ok(batch) => {
                                                                                if tx
                                                                                    .send(Ok(batch))
                                                                                    .await
                                                                                    .is_err()
                                                                                {
                                                                                    break;
                                                                                }
                                                                            }
                                                                            Err(e2) => {
                                                                                let _ = tx.send(Err(DataFusionError::ArrowError(Box::new(e2), None))).await;
                                                                                break;
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                                Err(e2) => {
                                                                    let _ = tx.send(Err(DataFusionError::ArrowError(Box::new(e2), None))).await;
                                                                }
                                                            }
                                                        }
                                                        Err(eopen) => {
                                                            let _ = tx.send(Err(
                                                                DataFusionError::IoError(eopen),
                                                            ));
                                                        }
                                                    }
                                                } else {
                                                    let _ = tx
                                                        .send(Err(DataFusionError::ArrowError(
                                                            Box::new(e),
                                                            None,
                                                        )))
                                                        .await;
                                                }
                                                break;
                                            }
                                        }
                                    }
                                    if had_iter_error { /* already handled */ }
                                }
                                Err(e) => {
                                    let es = e.to_string();
                                    if es.contains("failed to fill whole buffer")
                                        || es.contains("UnexpectedEof")
                                    {
                                        match OpenOptions::new().read(true).open(&seg_path_clone) {
                                            Ok(mut f2) => {
                                                if let Err(e) =
                                                    f2.seek(io::SeekFrom::Start(start_pos))
                                                {
                                                    let _ =
                                                        tx.send(Err(DataFusionError::IoError(e)));
                                                    return;
                                                }
                                                let reader2 = io::BufReader::new(f2);
                                                match StreamReader::try_new(reader2, None) {
                                                    Ok(sr2) => {
                                                        for item in sr2 {
                                                            match item {
                                                                Ok(batch) => {
                                                                    if tx
                                                                        .send(Ok(batch))
                                                                        .await
                                                                        .is_err()
                                                                    {
                                                                        break;
                                                                    }
                                                                }
                                                                Err(e) => {
                                                                    let _ = tx.send(Err(DataFusionError::ArrowError(Box::new(e), None))).await;
                                                                    break;
                                                                }
                                                            }
                                                        }
                                                    }
                                                    Err(e2) => {
                                                        let _ = tx
                                                            .send(Err(DataFusionError::ArrowError(
                                                                Box::new(e2),
                                                                None,
                                                            )))
                                                            .await;
                                                    }
                                                }
                                            }
                                            Err(eopen) => {
                                                let _ =
                                                    tx.send(Err(DataFusionError::IoError(eopen)));
                                            }
                                        }
                                    } else {
                                        let _ = tx
                                            .send(Err(DataFusionError::ArrowError(
                                                Box::new(e),
                                                None,
                                            )))
                                            .await;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Err(DataFusionError::IoError(e)));
                        }
                    }
                    drop(tx);
                });
            }
        }

        // ── Stream wrapper + output sync ───────────────────────────────────
        struct SegRecordBatchStream {
            schema: SchemaRef,
            rx: mpsc::Receiver<Result<RecordBatch, DataFusionError>>,
        }
        impl futures::Stream for SegRecordBatchStream {
            type Item = Result<RecordBatch, DataFusionError>;
            fn poll_next(
                self: Pin<&mut Self>,
                cx: &mut TaskContext<'_>,
            ) -> TaskPoll<Option<Self::Item>> {
                let inner = unsafe { self.get_unchecked_mut() };
                match inner.rx.poll_recv(cx) {
                    TaskPoll::Ready(Some(item)) => TaskPoll::Ready(Some(item)),
                    TaskPoll::Ready(None) => TaskPoll::Ready(None),
                    TaskPoll::Pending => TaskPoll::Pending,
                }
            }
        }
        impl RecordBatchStream for SegRecordBatchStream {
            fn schema(&self) -> SchemaRef {
                self.schema.clone()
            }
        }

        // Read CDC metadata for this partition from the segment, if present
        let cdc_ctx: Option<crate::plugins::cdc::SyncContext> = {
            use crate::plugins::cdc::{SyncContext, WalPartKind, WalPartMeta};
            let blobs_result: io::Result<std::collections::HashMap<PartitionKey, Vec<u8>>> =
                match source {
                    SegmentSource::Disk(seg_path) => match std::fs::File::open(seg_path) {
                        Ok(mut f) => SegmentFile::read_part_meta_blobs_from_reader(&mut f),
                        Err(e) => Err(e),
                    },
                    SegmentSource::S3 { data, .. } => {
                        let mut cursor = io::Cursor::new(data.as_ref());
                        SegmentFile::read_part_meta_blobs_from_reader(&mut cursor)
                    }
                };
            match blobs_result {
                Ok(blobs) => blobs.get(&idx.key).and_then(|blob| {
                    if blob.is_empty() {
                        return None;
                    }
                    bincode::deserialize::<WalPartMeta>(blob)
                        .ok()
                        .and_then(|pm| {
                            if pm.kind == WalPartKind::Cdc && !pm.rows.is_empty() {
                                let contract = crate::plugins::cdc::get_namespace_cdc_contract(
                                    &idx.key.namespace,
                                );
                                Some(SyncContext {
                                    part_meta: pm,
                                    contract,
                                })
                            } else {
                                None
                            }
                        })
                }),
                Err(_) => None,
            }
        };

        let batch_stream: SendableRecordBatchStream = Box::pin(SegRecordBatchStream {
            schema: schema.clone(),
            rx,
        });
        if let Err(e) = shared_output
            .sync(batch_stream, out_key.clone(), cdc_ctx.as_ref())
            .await
        {
            let err_str = e.to_string();
            let failure_key = format!("{}:{}", seg_display, out_key);
            let attempts = {
                let mut entry = COMPACT_FAILURES.entry(failure_key.clone()).or_insert(0);
                *entry += 1;
                *entry
            };
            error!(
                "Compactor: compact failed seg={} key={:?} out_key={} attempt={} error={}",
                seg_display, idx.key, out_key, attempts, err_str
            );
            if err_str.contains("failed to fill whole buffer") || err_str.contains("UnexpectedEof")
            {
                let diag = format!(
                    "seg={} file_len={} start={} idx_len={} safe_len={} out_key={} error={}",
                    seg_display, file_len, idx.start, idx.len, safe_len, out_key, err_str
                );
                if let SegmentSource::Disk(seg_path) = source {
                    let qdir = PathBuf::from(format!(
                        "{}/segment_buffer/quarantine",
                        Config::get_data_dir()
                    ));
                    let _ = fs::create_dir_all(&qdir);
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
                        if let Err(e) = fs::copy(seg_path, &qseg) {
                            error!(
                                "Compactor: quarantine copy failed seg={} to={} err={}",
                                seg_display,
                                qseg.to_string_lossy(),
                                e
                            );
                        }
                    }
                    let _ = fs::write(&qdiag, diag.as_bytes());
                }
                warn!("Compactor: truncated-stream quarantine seg={}", seg_display);
                crate::metrics::counters::add_quarantined_partitions(1);
            }
            return Ok(false);
        }

        if Config::debug_enabled() || Config::log_wal_enabled() {
            info!(
                "Compactor: sync completed seg={} key={:?} out_key={} idx_bytes={} segment_offsets={} segment_parts={}",
                seg_display,
                idx.key,
                out_key,
                idx.bytes,
                meta.offsets.len(),
                meta.index.len()
            );
        }

        // ── Post-compaction: tombstone + cleanup ───────────────────────────
        COMPACT_FAILURES.remove(&format!("{}:{}", seg_display, out_key));
        crate::metrics::counters::add_wal_compaction_completed(1);

        let tdir = Self::tombstone_dir();
        let _ = fs::create_dir_all(&tdir);
        let tpath = Self::tombstone_path_for_source(source, &idx.key);
        if let Err(e) = fs::write(&tpath, b"") {
            error!("Failed to write tombstone {:?}: {}", tpath, e);
        }

        let all_tombstoned = meta
            .index
            .iter()
            .all(|p| Self::is_source_tombstoned(source, &p.key));

        if Config::debug_enabled() || Config::log_wal_enabled() {
            info!(
                "Compactor: tombstoned seg={} key={:?} out_key={} all_tombstoned={}",
                seg_display, idx.key, out_key, all_tombstoned
            );
        }

        if all_tombstoned {
            let segment_deleted = match source {
                SegmentSource::Disk(seg_path) => {
                    match Self::remove_disk_segment_and_commit_marker(seg_path) {
                        Ok(_) => true,
                        Err(e) => {
                            warn!(
                                "Failed to remove fully-compacted segment {}: {}",
                                seg_display, e
                            );
                            false
                        }
                    }
                }
                SegmentSource::S3 { key, bucket, .. } => {
                    let client = crate::helpers::s3::get_s3_client().await;
                    let commit_key = format!("{}.commit", key);
                    let seg_ok = client
                        .delete_object()
                        .bucket(bucket)
                        .key(key)
                        .send()
                        .await
                        .is_ok();
                    if !seg_ok {
                        warn!("Failed to delete S3 segment {}", key);
                    }
                    if let Err(e) = client
                        .delete_object()
                        .bucket(bucket)
                        .key(&commit_key)
                        .send()
                        .await
                    {
                        warn!("Failed to delete S3 commit marker {}: {:?}", commit_key, e);
                    }
                    seg_ok
                }
            };
            if segment_deleted {
                if Config::debug_enabled() || Config::log_wal_enabled() {
                    info!("Compactor: removed fully compacted segment {}", seg_display);
                }
                Self::segment_cache_remove(source.segment_id());
                for part in meta.index.iter() {
                    let tp = Self::tombstone_path_for_source(source, &part.key);
                    if tp.exists() {
                        if let Err(e) = fs::remove_file(&tp) {
                            warn!("Failed to remove tombstone {:?}: {}", tp, e);
                        }
                    }
                }
            }
        }
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

pub fn wal_recover_disk(offsets_db: Arc<Offsets>) -> io::Result<()> {
    let started = std::time::Instant::now();
    let mut count = 0u64;
    let mut bytes = 0u64;
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
    let mut namespaces: HashSet<String> = HashSet::new();
    let mut namespace_partition_files: HashMap<PartitionKey, u64> = HashMap::new();
    let mut namespace_partition_bytes: HashMap<PartitionKey, u64> = HashMap::new();

    let mut committed_offsets: u64 = 0;

    for file_path in seg_files {
        let seg = SegmentFile {
            path: file_path.clone(),
        };
        match seg.read_metadata() {
            Ok(meta) => {
                if Config::debug_enabled() || Config::log_wal_enabled() {
                    let partition_sample: Vec<String> = meta
                        .index
                        .iter()
                        .take(3)
                        .map(|idx| {
                            format!("{}/{}/{}B", idx.key.namespace, idx.key.partition, idx.bytes)
                        })
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
                        (*namespace_partition_bytes.get(&key).unwrap_or(&0))
                            .saturating_add(idx.bytes);
                }
                bytes = bytes.saturating_add(meta.total_bytes);
                count = count.saturating_add(1);
                mark_offsets_durable_in_wal(offsets_db.as_ref(), meta.offsets.iter());
                committed_offsets = committed_offsets.saturating_add(meta.offsets.len() as u64);
                Buffers::segment_cache_register(SegmentSource::Disk(file_path), meta);
            }
            Err(e) => {
                warn!(
                    "Failed to read segment metadata {}: {}",
                    file_path.to_string_lossy(),
                    e
                );
            }
        }
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
                    if let Ok(meta) = SegmentFile::read_metadata_from_bytes(&data) {
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
                        committed_offsets =
                            committed_offsets.saturating_add(meta.offsets.len() as u64);
                        bytes_total = bytes_total.saturating_add(meta.total_bytes);
                        processed = processed.saturating_add(1);
                        Buffers::segment_cache_register(
                            SegmentSource::S3 {
                                key: seg_key.clone(),
                                bucket: bucket.clone(),
                                data: Arc::new(data),
                            },
                            meta,
                        );
                    }
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
    Ok(())
}

#[allow(dead_code)]
fn list_wal_files() -> io::Result<Vec<PathBuf>> {
    Ok(Vec::new())
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
    if is_s3_wal() {
        return wal_recover_s3(offsets_db).await;
    }
    wal_recover_disk(offsets_db)
}

#[cfg(test)]
mod tests_wal_commit {
    use super::*;
    use crate::buffer::segment_file::SegmentFile;
    use crate::helpers::configuration::Config;
    use arrow::array::Int32Array;
    use arrow::record_batch::RecordBatch;
    use arrow_schema::{DataType, Field, Schema};
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
        {
            let mut snapshots = SEGMENT_SNAPSHOTS.lock().unwrap();
            snapshots.clear();
        }
        SEGMENT_CACHE.clear();
    }

    fn commit_exists(seg_path: &PathBuf) -> bool {
        seg_path.with_extension("seg.commit").exists()
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
            shard: "shard".to_string(),
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
            shard: "shard".to_string(),
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
            shard: "shard".to_string(),
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
            _shard: String::new(),
            schema,
            record_batches: Some(vec![batch]),
            cdc_rows: None,
        };

        let mut buffers = Buffers::new();
        buffers.write(vec![ingest_batch]);
        assert_eq!(Buffers::segs_remaining(), 0);

        let noop: Box<dyn crate::plugins::DataSink + Send + Sync> =
            Box::new(crate::plugins::NoopOutputPlugin);
        let output = Arc::new(noop);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(buffers.flush(offsets_db.clone(), output))
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
}
/// Drain and compact all WAL partitions to the configured output plugin.
/// Consumes partition queues by repeatedly compacting until empty.
pub async fn drain_all_partitions(
    shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
    offsets: Arc<Offsets>,
) {
    // Flush any remaining in-memory segments to WAL before compaction
    let _ = flush_all_segments(offsets.clone()).await;

    // Collect partition arcs
    let parts: Vec<Arc<tokio::sync::Mutex<WalPartition>>> = Vec::new();

    // Bounded parallel compaction across partitions (single-threaded per partition via the mutex)
    let concurrency = crate::metrics::counters::WAL_COMPACTION_CONCURRENCY_TARGET
        .load(std::sync::atomic::Ordering::Relaxed)
        .clamp(1, 64);

    let mut in_flight: futures::stream::FuturesUnordered<Pin<Box<dyn Future<Output = ()> + Send>>> =
        futures::stream::FuturesUnordered::new();
    let mut iter = parts.into_iter();

    for _ in 0..concurrency {
        if let Some(part_arc) = iter.next() {
            let offsets_db = offsets.clone();
            let out = shared_output.clone();
            in_flight.push(Box::pin(async move {
                loop {
                    // Lock this partition and compact until empty
                    if let Ok(mut p) = part_arc.try_lock() {
                        if p.len() == 0 {
                            break;
                        }
                        let _ = p
                            .compact_batches_to_parquet(offsets_db.clone(), out.clone())
                            .await;
                        // loop continues until queue empty
                    } else {
                        tokio_sleep(TokioDuration::from_millis(10)).await;
                    }
                }
            }));
        }
    }

    while let Some(_) = in_flight.next().await {
        if let Some(part_arc) = iter.next() {
            let offsets_db = offsets.clone();
            let out = shared_output.clone();
            in_flight.push(Box::pin(async move {
                loop {
                    if let Ok(mut p) = part_arc.try_lock() {
                        if p.len() == 0 {
                            break;
                        }
                        let _ = p
                            .compact_batches_to_parquet(offsets_db.clone(), out.clone())
                            .await;
                    } else {
                        tokio_sleep(TokioDuration::from_millis(10)).await;
                    }
                }
            }));
        }
    }
}

/// Force-flush all segments to WAL files regardless of thresholds.
pub async fn flush_all_segments(offsets_db: Arc<Offsets>) -> Result<(), ArrowError> {
    let bytes: u64 = 0;
    let mut rows: u64 = 0;
    let mut uploaded_bytes: u64 = 0;
    let (snapshot_rows, snapshot_bytes) =
        Buffers::flush_snapshot_queue_to_wal(offsets_db.as_ref()).await?;
    rows += snapshot_rows;
    uploaded_bytes += snapshot_bytes;

    let (live_rows, live_bytes) = Buffers::flush_live_segment_to_wal(offsets_db.as_ref()).await?;
    rows += live_rows;
    uploaded_bytes += live_bytes;

    metrics_hot::add_wal_write_bytes(bytes);
    metrics_hot::add_wal_write_rows(rows);
    WAL_BYTES_TOTAL.fetch_add(uploaded_bytes, std::sync::atomic::Ordering::Relaxed);

    Ok(())
}

#[derive(Clone)]
#[allow(dead_code)]
enum WalEntry {
    Segment {
        path: PathBuf,
        key: PartitionKey,
        offsets: HashMap<OffsetKey, u64>,
        bytes: u64,
        updated_at: SystemTime,
    },
}

impl WalEntry {
    fn bytes(&self) -> u64 {
        match self {
            WalEntry::Segment { bytes, .. } => *bytes,
        }
    }
    #[allow(dead_code)]
    fn updated_at(&self) -> SystemTime {
        match self {
            WalEntry::Segment { updated_at, .. } => *updated_at,
        }
    }
    #[allow(dead_code)]
    fn offsets(&self) -> &HashMap<OffsetKey, u64> {
        match self {
            WalEntry::Segment { offsets, .. } => offsets,
        }
    }
}

#[allow(dead_code)]
fn load_partition_segment_counter(
    sink_ref: &str,
    namespace: &str,
    partition: &str,
    time: Option<i64>,
    shard: &str,
) -> Option<u64> {
    let dir = WalFile::get_wal_partition_dir(namespace, partition, time, shard);
    let base = BufferChunker::encode_chunk_name(
        "ingest",
        Some(sink_ref),
        Some(namespace),
        Some(partition),
        time,
        Some(shard),
    );
    let path = PathBuf::from(format!("{}/{}-segment.counter", dir, base));
    if let Ok(s) = fs::read_to_string(path) {
        return s.trim().parse::<u64>().ok();
    }
    None
}

#[allow(dead_code)]
fn persist_partition_segment_counter(
    sink_ref: &str,
    namespace: &str,
    partition: &str,
    time: Option<i64>,
    shard: &str,
    next_segment_id: u64,
) {
    let dir = WalFile::get_wal_partition_dir(namespace, partition, time, shard);
    let base = BufferChunker::encode_chunk_name(
        "ingest",
        Some(sink_ref),
        Some(namespace),
        Some(partition),
        time,
        Some(shard),
    );
    let path = PathBuf::from(format!("{}/{}-segment.counter", dir, base));
    let _ = fs::write(path, next_segment_id.to_string());
}

// Removed WalSegment: we use a single FIFO queue per partition
pub struct WalPartition {
    // FIFO queue of immutable WAL entries
    queue: std::collections::VecDeque<WalEntry>,
    bytes: u64,
    updated_at: SystemTime,

    pub(crate) namespace: String,
    pub(crate) sink_ref: String,
    pub(crate) partition: String,
    pub(crate) time: Option<i64>,
    pub(crate) shard: String,
}

impl WalPartition {
    pub fn len(&self) -> usize {
        self.queue.len()
    }
    pub fn clear_queue(&mut self) {
        self.queue.clear();
        self.bytes = 0;
    }
    fn schemas_equivalent(a: &SchemaRef, b: &SchemaRef) -> bool {
        let sa = a.as_ref();
        let sb = b.as_ref();
        if sa.fields().len() != sb.fields().len() {
            return false;
        }
        for (fa, fb) in sa.fields().iter().zip(sb.fields().iter()) {
            if fa.name() != fb.name() {
                return false;
            }
            if fa.data_type() != fb.data_type() {
                return false;
            }
            if fa.is_nullable() != fb.is_nullable() {
                return false;
            }
        }
        true
    }
    fn prune_tombstone_wals(&mut self) {
        // println!("Purging tombstone WAL files");
        let data_dir = Config::get_data_dir();
        let wal_dir = PathBuf::from(format!("{}/ingest_buffer/done", data_dir));
        fs::remove_dir_all(&wal_dir).unwrap_or_default();
        fs::create_dir_all(&wal_dir).unwrap_or_default()
    }

    // No segment rotation in queue model

    fn is_file_size_exceeded(&self) -> bool {
        let buffer_size = Config::get_pipeline_buffer_threshold_bytes();
        self.bytes > buffer_size
    }

    fn is_file_time_exceeded(&self) -> bool {
        let ttl = Config::get_pipeline_buffer_threshold_seconds();
        SystemTime::now()
            .duration_since(self.updated_at)
            .unwrap()
            .as_secs()
            > ttl as u64
    }

    pub fn check_wal_rotate(&self, force_compact: bool) -> bool {
        if force_compact || self.is_file_size_exceeded() || self.is_file_time_exceeded() {
            // debug!("Compacting WAL: {} Bytes: {}, Segment Files: {}", self.namespace, self.bytes, self.files.len());
            let elapsed = SystemTime::now()
                .duration_since(self.updated_at)
                .unwrap()
                .as_secs();
            info!(
                "Compacting WAL partition Namespace: {}, Partition: {}, Time: {}, of Bytes: {}, Elapsed Secs: {}, Queue Len: {}",
                self.namespace,
                self.partition,
                self.time.unwrap_or(0),
                Helpers::human_readable_size(self.bytes),
                elapsed,
                self.queue.len()
            );
            return true;
        }

        false
    }

    pub(crate) async fn compact_batches_to_parquet(
        &mut self,
        _offsets_db: Arc<Offsets>,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> u64 {
        let data_dir = Config::get_data_dir();
        let mut output_file_name = BufferChunker::encode_chunk_name(
            "output",
            Some(&self.sink_ref),
            Some(&self.namespace),
            Some(&self.partition),
            self.time,
            Some(&self.shard),
        );

        // NOTE: offsets are committed AFTER successful upload now (moved below)

        // println!("Compacting WAL partition to Parquet, Namespace: {} Partition: {} {}", self.namespace, self.partition, self.time.unwrap_or(0));

        let mut wal_compacted_bytes_total = 0;
        let mut wal_compacted_files_total = 0;

        // Deterministic compaction id
        let mut hasher = Sha256::new();
        for e in self.queue.iter() {
            let WalEntry::Segment {
                path,
                bytes,
                updated_at,
                ..
            } = e;
            hasher.update(path.as_os_str().as_encoded_bytes());
            hasher.update(&bytes.to_le_bytes());
            let ts = updated_at
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            hasher.update(&ts.to_le_bytes());
        }
        let compaction_id = {
            let digest = hasher.finalize();
            hex::encode(&digest[..8])
        };
        output_file_name = format!("{}-c={}", output_file_name, compaction_id);

        // Choose a slice from the front of the queue up to byte threshold
        let mut segment_files: Vec<WalEntry> = Vec::new();
        if self.queue.is_empty() {
            info!(
                "No WAL files to compact for partition: {} {}",
                self.namespace, self.partition
            );
            return wal_compacted_bytes_total;
        }
        let mut acc_bytes: u64 = 0;
        for e in self.queue.iter() {
            if acc_bytes >= Config::get_pipeline_buffer_threshold_bytes() {
                break;
            }
            acc_bytes = acc_bytes.saturating_add(e.bytes());
            segment_files.push(e.clone());
        }
        if segment_files.is_empty() {
            return 0;
        }

        let schema: SchemaRef = {
            let schema_opt: Option<SchemaRef> = None;
            // For Segment entries, we will read schema later from SegmentFile; fallback not needed here
            // Keep returning an error if none found
            match schema_opt {
                Some(s) => s,
                None => {
                    error!(
                        "Failed to read schema from local WALs for namespace: {}",
                        self.namespace
                    );
                    return wal_compacted_bytes_total;
                }
            }
        };

        for wf in segment_files.iter() {
            wal_compacted_bytes_total += wf.bytes();
            wal_compacted_files_total += 1;
        }

        let (tx, rx) = mpsc::channel::<Result<RecordBatch, DataFusionError>>(8);
        let schema_clone = schema.clone();
        let namespace = self.namespace.clone();
        let partition = self.partition.clone();
        let time_val = self.time;
        let files = segment_files.clone();
        let row_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let row_counter_task = row_counter.clone();
        let batch_ok_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let batch_mismatch_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let batch_error_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        if Config::debug_enabled() || Config::log_wal_enabled() {
            debug!(
                "Compactor: start ns={} part={} shard={} time={} files={} out_key={}",
                self.namespace,
                self.partition,
                self.shard,
                self.time.unwrap_or(0),
                files.len(),
                output_file_name
            );
        }

        tokio::spawn(async move {
            for wal_entry in files.into_iter() {
                match wal_entry {
                    WalEntry::Segment { path, .. } => {
                        match OpenOptions::new().read(true).open(&path) {
                            Ok(file) => {
                                let mut reader = io::BufReader::new(file);
                                let mut offset_size = [0u8; 8];
                                if let Err(e) = reader.read_exact(&mut offset_size) {
                                    error!(
                                        "ERROR: Failed to read segment header for {}: {}",
                                        path.to_string_lossy(),
                                        e
                                    );
                                    continue;
                                }
                                let skip = u64::from_le_bytes(offset_size);
                                if let Err(e) = reader.seek(io::SeekFrom::Current(skip as i64)) {
                                    error!(
                                        "ERROR: Failed to seek segment stream {}: {}",
                                        path.to_string_lossy(),
                                        e
                                    );
                                    continue;
                                }
                                match StreamReader::try_new(reader, None) {
                                    Ok(sr) => {
                                        for item in sr {
                                            match item {
                                                Ok(batch) => {
                                                    if !WalPartition::schemas_equivalent(
                                                        &batch.schema(),
                                                        &schema_clone,
                                                    ) {
                                                        error!("ERROR: Skipping WAL batch due to schema mismatch for ns={} part={} time={}", namespace, partition, time_val.unwrap_or(0));
                                                        batch_mismatch_counter
                                                            .fetch_add(1, AtomicOrdering::Relaxed);
                                                        continue;
                                                    }
                                                    row_counter_task.fetch_add(
                                                        batch.num_rows() as u64,
                                                        AtomicOrdering::Relaxed,
                                                    );
                                                    batch_ok_counter
                                                        .fetch_add(1, AtomicOrdering::Relaxed);
                                                    if tx.send(Ok(batch)).await.is_err() {
                                                        break;
                                                    }
                                                }
                                                Err(e) => {
                                                    batch_error_counter
                                                        .fetch_add(1, AtomicOrdering::Relaxed);
                                                    let _ = tx
                                                        .send(Err(DataFusionError::ArrowError(
                                                            Box::new(e),
                                                            None,
                                                        )))
                                                        .await;
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        error!(
                                            "Failed to init Arrow stream for local segment {}: {}",
                                            path.to_string_lossy(),
                                            e
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                error!(
                                    "Failed to open segment file {}: {}",
                                    path.to_string_lossy(),
                                    e
                                );
                            }
                        }
                    }
                }
            }
            drop(tx);
        });

        struct WalRecordBatchStream {
            schema: SchemaRef,
            rx: mpsc::Receiver<Result<RecordBatch, DataFusionError>>,
        }

        impl futures::Stream for WalRecordBatchStream {
            type Item = Result<RecordBatch, DataFusionError>;
            fn poll_next(
                self: Pin<&mut Self>,
                cx: &mut TaskContext<'_>,
            ) -> TaskPoll<Option<Self::Item>> {
                // Safety: we only move rx
                let inner = unsafe { self.get_unchecked_mut() };
                match inner.rx.poll_recv(cx) {
                    TaskPoll::Ready(Some(item)) => TaskPoll::Ready(Some(item)),
                    TaskPoll::Ready(None) => TaskPoll::Ready(None),
                    TaskPoll::Pending => TaskPoll::Pending,
                }
            }
        }

        impl RecordBatchStream for WalRecordBatchStream {
            fn schema(&self) -> SchemaRef {
                self.schema.clone()
            }
        }

        let batch_stream: SendableRecordBatchStream = Box::pin(WalRecordBatchStream {
            schema: schema.clone(),
            rx,
        });

        // (No re-upload here; WALs were uploaded earlier in flush prior to offset commit.)

        match shared_output
            .sync(batch_stream, output_file_name.clone(), None)
            .await
        {
            Ok(()) => {
                metrics_hot::add_wal_compacted_bytes(wal_compacted_bytes_total);
                let wal_compacted_rows_total = row_counter.load(AtomicOrdering::Relaxed);
                metrics_hot::add_wal_compacted_rows(wal_compacted_rows_total);
                metrics_hot::add_wal_compacted_files(wal_compacted_files_total);

                // Compaction must not change the source-level Closed flag. A segment becoming
                // durable in WAL is what closes the source offset; downstream sink replay is a
                // separate concern handled by idempotent sink writes and recovery.

                for wal_entry in segment_files.iter() {
                    let WalEntry::Segment { path, .. } = wal_entry;
                    let tombstone_path = format!(
                        "{}/ingest_buffer/done/{}.tombstone",
                        data_dir,
                        Helpers::random_str(32)
                    );
                    if let Err(e) = fs::rename(&path, tombstone_path) {
                        error!(
                            "Failed to tombstone segment file: {}, Error: {}",
                            path.to_string_lossy(),
                            e
                        );
                    }
                }
                self.prune_tombstone_wals();
                // Remove exactly the entries we compacted from the front of the queue
                for _ in 0..segment_files.len() {
                    if let Some(front) = self.queue.pop_front() {
                        self.bytes = self.bytes.saturating_sub(front.bytes());
                    }
                }
            }
            Err(e) => {
                let output_plugin_name = Config::get_pipeline_output_plugin_name();
                error!("Failed to sync WAL partition to output plugin: {}, Namespace {}, Partition {}, Error {}", output_plugin_name, self.namespace, self.partition, e);
            }
        }

        wal_compacted_bytes_total
    }

    #[allow(dead_code)]
    async fn apply_sql_on_ipc_stream(
        temp_parquet_path: &str,
        _sql: &str,
        _schema_ref: SchemaRef,
    ) -> Result<Vec<RecordBatch>, ArrowError> {
        let session_config = SessionConfig::new();
        let ctx = SessionContext::new_with_config(session_config);

        // ctx.register_parquet("my_table", temp_parquet_path, ParquetReadOptions {
        //     schema: Some(schema_ref.as_ref()),
        //     file_extension: "parquet",
        //     table_partition_cols: vec![],
        //     parquet_pruning: None,
        //     skip_metadata: Some(true),
        //     file_sort_order: vec![],
        // }).await.unwrap();
        // // Execute the SQL query
        //
        // let df = ctx.sql(sql).await?;
        // let results = df.collect().await.unwrap();

        let df = ctx
            .read_parquet(temp_parquet_path, Default::default())
            .await
            .unwrap();
        let results = df.collect().await.unwrap();

        // Create a new DataFusion context
        // let mut ctx = ExecutionContext::new();

        // Read batches and register them as a table in the context
        // let schema_ref = record_batches[0].schema();

        // let schema_ref = ARROW_SCHEMA.read().get("my_table").unwrap().clone();

        // ctx.register_table("my_table", Arc::new(MemTable::try_new(schema_ref, record_batches)?))?;

        // let mut results = Vec::new();
        //
        // let df = ctx.read_a
        //
        //
        // for batch in record_batches {
        //     for b in batch {
        //        let df = ctx.read_batch(b.clone()).unwrap();
        //         results.extend(df.collect().await.unwrap());
        //     }
        // }

        Ok(results)
    }

    #[allow(dead_code)]
    async fn apply_sql_on_ipc_record_batches(
        record_batches: Vec<RecordBatch>,
        _sql: &str,
        _schema_ref: SchemaRef,
    ) -> Result<Vec<RecordBatch>, ArrowError> {
        let session_config = SessionConfig::new();
        // session_config = session_config.set("datafusion.catalog.information_schema", "true".into());
        // session_config = session_config.set("datafusion.catalog.default_catalog", "skippr".into());
        // session_config = session_config.set("datafusion.execution.collect_statistics", "true".into());

        let ctx = SessionContext::new_with_config(session_config);

        // Read batches and register them as a table in the context
        // let schema_ref = record_batches[0].schema();
        // let schema_ref = ARROW_SCHEMA.read().get("my_table").unwrap().clone();

        // ctx.register_table("my_table", Arc::new(MemTable::try_new(schema_ref, record_batches)?))?;

        // let mut results = Vec::new();
        //
        // let df = ctx.read_a
        //
        //

        let batch = arrow::compute::concat_batches(&_schema_ref, &record_batches).unwrap();

        let df = ctx.read_batch(batch).unwrap();
        let results = df.collect().await.unwrap();

        Ok(results)
    }
}

#[derive(Clone)]
#[allow(dead_code)]
struct CompactionTask {
    key: (String, String, Option<i64>, String),
    force: bool,
}

// Public drain used at end-of-ingest to compact any remaining WALs regardless of thresholds
pub async fn force_drain_all(
    _offsets_db: Arc<Offsets>,
    _shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
) {
}

#[derive(Clone)]
#[allow(dead_code)]
pub struct WalFile {
    pub(crate) path: PathBuf,
    pub(crate) sink_ref: String,
    pub(crate) namespace: String,
    pub(crate) partition: String,
    pub(crate) time: Option<i64>,
    pub(crate) shard: String,
    pub(crate) bytes: u64,
    #[allow(dead_code)]
    pub(crate) file: Arc<TimedRwLock<Option<File>>>,
    pub(crate) updated_at: SystemTime,
    pub(crate) offsets: HashMap<OffsetKey, u64>,
}

impl WalFile {
    pub fn new(
        sink_ref: &str,
        namespace: &str,
        partition: &str,
        time: Option<i64>,
        shard: &str,
        offsets: HashMap<OffsetKey, u64>,
    ) -> io::Result<Self> {
        let path_str =
            Self::generate_temp_wal_file_name(sink_ref, namespace, partition, time, shard);
        let path = PathBuf::from(&path_str);

        // Ensure file exists and then allow the fp to drop out of scope to limit open file handles
        let fd = OpenOptions::new()
            .append(true)
            .read(true)
            .create(true)
            .open(&path)?;
        fd.sync_all()?;
        #[cfg(unix)]
        unsafe {
            libc::fsync(fd.as_raw_fd());
        };

        Ok(WalFile {
            path,
            sink_ref: sink_ref.to_string(),
            bytes: 0,
            namespace: namespace.to_string(),
            partition: partition.to_string(),
            time,
            shard: shard.to_string(),
            file: Arc::new(TimedRwLock::new("wal_file".to_string(), None)),
            updated_at: SystemTime::now(),
            offsets,
        })
    }

    fn get_or_open_file(&self) -> io::Result<File> {
        // let mut file_lock = self.file.write();
        OpenOptions::new()
            .read(true)
            .create(true)
            .append(true)
            .open(&self.path)
        // file.try_clone()
        // if file.is_none() {
        //     *file_lock = Some(OpenOptions::new().write(true).read(true).create(true).open(&self.path)?);
        // }
        // // Clone the file handle via Arc. File itself does not implement Clone.
        // file_lock.as_ref().unwrap().try_clone()
    }

    #[allow(dead_code)]
    fn from_path(path: &PathBuf) -> io::Result<Self> {
        let mut file = OpenOptions::new().read(true).open(&path)?;

        let offsets = Self::offset_from_file(&mut file)?;

        let sink_ref = BufferChunker::decode_file_sink_ref(path.to_str().unwrap());
        let namespace = BufferChunker::decode_file_namespace(path.to_str().unwrap());
        let partition = BufferChunker::decode_file_partition(path.to_str().unwrap());
        let time = BufferChunker::decode_file_time(path.to_str().unwrap());
        let time = if time > 0 { Some(time) } else { None };

        let shard = BufferChunker::decode_file_shard(path.to_str().unwrap());

        let metadata = file.metadata()?;

        let updated_at = metadata.modified().unwrap_or_else(|_err| SystemTime::now());

        let bytes = metadata.len();

        Ok(WalFile {
            path: path.clone(),
            sink_ref,
            bytes,
            namespace,
            partition,
            time,
            shard,
            file: Arc::new(TimedRwLock::new("wal_file".to_string(), None)),
            updated_at,
            offsets,
        })
    }

    pub fn offset_from_file(file: &mut File) -> io::Result<HashMap<OffsetKey, u64>> {
        // let mut file = OpenOptions::new().read(true).open(&path)?;

        let mut offset_size = [0u8; 8];

        file.read_exact(&mut offset_size)?;

        let mut bin_offset = vec![0u8; u64::from_le_bytes(offset_size) as usize];

        let mut reader = io::BufReader::new(file);

        reader.read_exact(&mut bin_offset)?;

        Ok(bincode::deserialize(&bin_offset).unwrap())
    }

    pub fn seek_offset(reader: &mut BufReader<&File>) -> Result<usize, ArrowError> {
        reader.seek(io::SeekFrom::Start(0)).unwrap();

        // println!("Reading offset from WAL file: {}", self.path.to_str().unwrap());

        let mut offset_size = [0u8; 8];
        reader.read_exact(&mut offset_size)?;

        reader
            .seek(io::SeekFrom::Current(i64::from_le_bytes(offset_size)))
            .unwrap();

        Ok(usize::from_le_bytes(offset_size))
    }

    pub fn read_schema_from_stream(&mut self) -> Result<SchemaRef, ArrowError> {
        let file = self.get_or_open_file()?;
        let mut reader = io::BufReader::new(&file);

        Self::seek_offset(&mut reader)?;

        let stream_reader = StreamReader::try_new(reader, None)?;

        let schema = stream_reader.schema();

        Ok(schema)
    }

    pub fn read_from_stream(&mut self) -> Result<Vec<RecordBatch>, ArrowError> {
        let file = self.get_or_open_file()?;
        let mut reader = io::BufReader::new(&file);

        Self::seek_offset(&mut reader)?;

        let stream_reader = StreamReader::try_new(reader, None)
            .map_err(|e| ArrowError::from_external_error(Box::new(e)))?;

        let mut record_batches = Vec::new();

        for batch in stream_reader {
            let batch = batch?;
            record_batches.push(batch);
        }

        Ok(record_batches)
    }

    /**
     * Write record batches to the WAL file
     * @param record_batches
     * @return tuple of bytes and total rows written (bytes, row_count)
     */
    pub fn write_to_stream(
        &mut self,
        record_batches: &[RecordBatch],
    ) -> Result<(u64, u64), ArrowError> {
        // let writer = self.file.as_mut().ok_or(ArrowError::IoError("Can't write to WAL file".to_string(), io::Error::new(io::ErrorKind::NotFound, "File not found")))?;

        let file = self
            .get_or_open_file()
            .expect("Failed to clone WAL file for write");
        let mut writer = io::BufWriter::new(&file);

        writer.seek(io::SeekFrom::Start(0))?;

        let bin_offset = bincode::serialize(&self.offsets).unwrap();

        let offset_size: u64 = bin_offset.len() as u64;

        writer.write_all(&offset_size.to_le_bytes())?;
        writer.write_all(&bin_offset)?;

        writer.seek(io::SeekFrom::End(0))?;

        let mut _size: usize = 0;
        let options = IpcWriteOptions::default();

        let mut row_count = 0;

        let mut stream_writer =
            StreamWriter::try_new_with_options(writer, &record_batches[0].schema(), options)?;
        for batch in record_batches {
            _size += batch.get_array_memory_size(); // @todo - account for compression ratio, observed ~50% reduction
            row_count += batch.num_rows();
            stream_writer
                .write(batch)
                .expect("Failed to write record batch to stream writer");
        }

        stream_writer.finish()?;

        // let write_file = OpenOptions::new()
        //     .create(true)
        //     .write(true)
        //     .open(&self.path)
        //     .unwrap();
        //
        // let props = WriterProperties::builder()
        //     .set_dictionary_enabled(false)
        //     .set_encoding(parquet::basic::Encoding::PLAIN)
        //     .set_compression(Compression::SNAPPY)
        //     .build();
        //
        // let schema = ARROW_SCHEMA.read().get(&self.namespace).unwrap().clone();
        //
        // let mut writer = ArrowWriter::try_new(write_file, schema, Some(props)).unwrap();
        //
        // for batch in record_batches {
        //     writer.write(&batch).expect("Error writing to parquet file");
        // }
        //
        // writer.close().unwrap();

        let file = self
            .get_or_open_file()
            .expect("Failed to get file for metadata read");
        self.bytes += file.metadata().unwrap().len();

        Ok((self.bytes, row_count as u64))
    }

    fn get_wal_partition_dir(
        _namespace: &str,
        _partition: &str,
        _time: Option<i64>,
        shard: &str,
    ) -> String {
        let data_dir = Config::get_data_dir();

        // let metrics_guard = METRICS.read();
        // let run_id = metrics_guard.run_id.clone();
        let output_dir = &format!("{}/ingest_buffer", data_dir);

        let shard = match shard {
            "" => "none",
            _ => shard,
        };

        let wal_partition_dir = format!("{}/{}", output_dir, shard);

        fs::create_dir_all(&wal_partition_dir).expect("Failed to create WAL partition directories");

        wal_partition_dir
    }

    fn generate_temp_wal_file_name(
        sink_ref: &str,
        namespace: &str,
        partition: &str,
        time: Option<i64>,
        shard: &str,
    ) -> String {
        let wal_file_name = BufferChunker::encode_chunk_name(
            "ingest",
            Some(sink_ref),
            Some(namespace),
            Some(partition),
            time,
            Some(shard),
        );

        let wal_partition_dir = WalFile::get_wal_partition_dir(namespace, partition, time, shard);

        let wal_file_name = format!(
            "{}/{}&id={}",
            wal_partition_dir,
            wal_file_name,
            Helpers::random_str(32)
        );

        format!("{}.tmp", wal_file_name)
    }

    // close file by renaming it .tmp to .wal
    pub fn finish(&mut self) -> io::Result<()> {
        let wal_file_name = self.path.to_str().unwrap().replace(".tmp", ".wal");
        fs::rename(&self.path, &wal_file_name)?;
        self.path = PathBuf::from(wal_file_name);
        Ok(())
    }

    // pub fn close(&mut self) {
    //     let mut file_lock = self.file.write();
    //         // .expect("Failed to lock file for closing");
    //
    //     if file_lock.is_some() {
    //         file_lock.as_mut().unwrap().sync_all().expect("Failed to sync WAL file");
    //         // Drop the file by replacing it with None, which closes the file
    //         *file_lock = None;
    //     }
    // }
}

impl Seek for WalFile {
    fn seek(&mut self, pos: io::SeekFrom) -> std::io::Result<u64> {
        self.get_or_open_file().unwrap().seek(pos)
    }
}

// impl Drop for WalFile {
//     fn drop(&mut self) {
//         // let fd = self.get_or_open_file().unwrap().as_raw_fd();
//         // unsafe {
//         //     libc::fsync(fd);
//         // };
//     }
// }

impl Read for WalFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.get_or_open_file().unwrap().read(buf)
    }
}

impl Write for WalFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.get_or_open_file().unwrap().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            let fd = self.get_or_open_file().unwrap().as_raw_fd();
            unsafe { libc::fsync(fd) };
        }
        #[cfg(not(unix))]
        {
            self.get_or_open_file().unwrap().sync_all()?;
        }
        Ok(())
    }

    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.get_or_open_file().unwrap().write_all(buf)
    }
}

pub fn _get_partition_dir(
    _namespace: &str,
    _partition: &str,
    _time: Option<i64>,
    shard: &str,
) -> String {
    let data_dir = Config::get_data_dir();

    // let metrics_guard = METRICS.read();
    // let run_id = metrics_guard.run_id.clone();
    let output_dir = &format!("{}/ingest_buffer", data_dir);

    let shard = match shard {
        "" => "none",
        _ => shard,
    };

    let wal_partition_dir = format!("{}/{}", output_dir, shard);

    fs::create_dir_all(&wal_partition_dir).expect("Failed to create WAL partition directories");

    wal_partition_dir
}
