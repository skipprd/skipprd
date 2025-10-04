#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Durability { Memory, Disk }
use std::fs::{File, OpenOptions};
use std::{fs, io};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::{BufReader, Read, Seek, Write};
use std::path::PathBuf;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;
use arrow::array::RecordBatch;
use arrow_schema::{ArrowError, SchemaRef};
use glob::{glob_with, MatchOptions};
use std::sync::atomic::AtomicU64;
use arrow::ipc::reader::StreamReader;
use arrow::ipc::writer::{IpcWriteOptions, StreamWriter};
use datafusion::physical_plan::SendableRecordBatchStream;
use datafusion::physical_plan::RecordBatchStream;
use datafusion::error::DataFusionError;
use datafusion::prelude::{ParquetReadOptions, SessionConfig, SessionContext};
use indexmap::IndexMap;
use once_cell::sync::Lazy;
use serde_derive::{Deserialize, Serialize};
use serde_json::Value;
use crate::METRICS;
use crate::metrics::counters as metrics_hot;
use crate::buffer::BufferChunker;
use crate::helpers::configuration::Config;
use crate::helpers::Helpers;
use crate::helpers::offsets::{OffsetKey, Offsets, OffsetTypes};
use crate::helpers::timed_rwlock::TimedRwLock;
use crate::plugins::DataOutputPlugin;
use tokio::time::{sleep as tokio_sleep, Duration as TokioDuration};
use futures::stream::StreamExt as FuturesStreamExt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll as TaskPoll};
use dashmap::DashMap;
use std::sync::atomic::Ordering as AtomicOrdering;
use sha2::{Sha256, Digest};
use hex;
use std::os::fd::AsRawFd;
use crate::buffer::segment_file::{SegmentFile, SegmentFileMetadata};

type PartitionKey = (String, String, Option<i64>, String);
use tokio::sync::mpsc;

#[allow(dead_code)]
pub static TOTAL_ROWS: Lazy<TimedRwLock<AtomicU64>> =
    Lazy::new(|| TimedRwLock::new("record_batch_total".to_string(), AtomicU64::new(0)));

// Lock-free WAL index and counters
pub static WAL_BYTES_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
static CONSUMER_STARTED: Lazy<std::sync::atomic::AtomicBool> = Lazy::new(|| std::sync::atomic::AtomicBool::new(false));

// Per-partition notify for quick wakeups
static PARTITION_NOTIFIES: Lazy<DashMap<PartitionKey, Arc<tokio::sync::Notify>>> = Lazy::new(|| DashMap::new());

// Global in-memory segment accumulator (reduces tiny WAL files across threads)
static SEGMENT_LIVE: Lazy<Arc<std::sync::Mutex<GlobalSegment>>> = Lazy::new(|| Arc::new(std::sync::Mutex::new(GlobalSegment::new())));
// Segment snapshot representation
#[derive(Clone)]
struct SegmentPartitionMeta { bytes: u64, updated_at: SystemTime }

#[derive(Clone)]
struct SegmentSnapshot {
    id: String,
    durability: Durability,
    created_at: SystemTime,
    updated_at: SystemTime,
    total_bytes: u64,
    offsets: HashMap<OffsetKey, u64>,
    batches: HashMap<PartitionKey, Vec<RecordBatch>>, // present only if in_memory
    meta: HashMap<PartitionKey, SegmentPartitionMeta>,
}

impl SegmentSnapshot {
    fn new(id: String, offsets: HashMap<OffsetKey, u64>, batches: HashMap<PartitionKey, Vec<RecordBatch>>, meta: HashMap<PartitionKey, SegmentPartitionMeta>, total_bytes: u64) -> Self {
        let now = SystemTime::now();
        SegmentSnapshot { id, durability: Durability::Memory, created_at: now, updated_at: now, total_bytes, offsets, batches, meta }
    }
}

// Queue of snapshots produced by rotation in write()
static SEGMENT_SNAPSHOTS: Lazy<std::sync::Mutex<std::collections::VecDeque<Arc<std::sync::Mutex<SegmentSnapshot>>>>> = Lazy::new(|| std::sync::Mutex::new(std::collections::VecDeque::with_capacity(8)));

// lazy_static! {
//     pub static ref WAL_INDEX: TimedRwLock<WalIndex> = TimedRwLock::new("wal_index".to_string(), WalIndex::new());
// }

// Moved into WalS3Object as read_from_stream()

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
pub struct OffsetKeySerialize {
    pub(crate) source_namespace: String,
    pub(crate) source_partition: String,
    pub(crate) position: u64,
}

#[derive(Debug, Clone)]
pub struct IngestRecord {
    pub(crate) _namespace: String,
    pub(crate) _partition: String,
    pub(crate) _time: Option<i64>,
    pub(crate) record: Value
}

pub struct IngestBufferBatch {
    pub(crate) offsets: HashMap<OffsetKey, u64>,
    pub(crate) _namespace: String,
    pub(crate) _partition: String,
    pub(crate) _time: Option<i64>,
    pub(crate) _shard: String,
    pub(crate) schema: SchemaRef,
    pub(crate) record_batches: Option<Vec<RecordBatch>>,
}


// Single global segment that aggregates batches for all partitions
struct GlobalSegment {
    batches: HashMap<PartitionKey, Vec<RecordBatch>>, // per partition batches
    offsets: HashMap<OffsetKey, u64>,                 // deduped across all partitions
    bytes: u64,
    updated_at: SystemTime,
    flushed_at: SystemTime,
}

impl GlobalSegment {
    fn new() -> Self { let now = SystemTime::now(); GlobalSegment { batches: HashMap::with_capacity(64), offsets: HashMap::new(), bytes: 0, updated_at: now, flushed_at: now } }
    fn add(&mut self, key: PartitionKey, batches: Vec<RecordBatch>, offsets: &HashMap<OffsetKey, u64>) {
        let mut add_bytes: u64 = 0;
        for b in batches.iter() { add_bytes = add_bytes.saturating_add(b.get_array_memory_size() as u64); }
        self.batches.entry(key).or_insert_with(|| Vec::with_capacity(64)).extend(batches);
        for (k, v) in offsets.iter() { self.offsets.entry(k.clone()).and_modify(|p| *p = (*p).max(*v)).or_insert(*v); }
        self.bytes = self.bytes.saturating_add(add_bytes);
        self.updated_at = SystemTime::now();
    }
    fn take(&mut self) -> (HashMap<PartitionKey, Vec<RecordBatch>>, HashMap<OffsetKey, u64>, u64) {
        let batches = std::mem::take(&mut self.batches);
        let offsets = std::mem::take(&mut self.offsets);
        let bytes = std::mem::replace(&mut self.bytes, 0);
        self.updated_at = SystemTime::now();
        (batches, offsets, bytes)
    }
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

pub struct Buffers {
    buf: Vec<IngestBufferBatch>, // legacy; unused in segmented mode
}

impl Buffers {

    pub fn new() -> Self { Buffers { buf: Vec::with_capacity(32) } }


    pub fn write(&mut self, batches: Vec<IngestBufferBatch>) {
        for mut ingest_buffer_batch in batches.into_iter() {
            let namespace = ingest_buffer_batch._namespace.clone();
            let partition = ingest_buffer_batch._partition.clone();
            let time = ingest_buffer_batch._time.clone();
            // Ensure shard key reflects schema so schemas do not mix in one segment
            let shard = if ingest_buffer_batch._shard.is_empty() { schema_fingerprint(&ingest_buffer_batch.schema) } else { ingest_buffer_batch._shard.clone() };
            let key: PartitionKey = (namespace, partition, time, shard);

            let mut batches_vec = ingest_buffer_batch.record_batches.take().unwrap_or_default();
            if batches_vec.is_empty() { continue; }

            // Thresholds (apply in write): 4MB or 60s elapsed since last flush or update
            let byte_threshold = 4 * 1024 * 1024u64;
            let time_threshold = 60u64; // seconds default

            let mut seg = SEGMENT_LIVE.lock().unwrap();
            let age_secs = seg.flushed_at.elapsed().map(|d| d.as_secs()).unwrap_or(0);
            let last_update_elapsed = seg.updated_at.elapsed().map(|d| d.as_secs()).unwrap_or(0);
            let should_rotate = (seg.bytes >= byte_threshold || age_secs >= time_threshold || last_update_elapsed >= time_threshold) && seg.bytes > 0;
            if should_rotate {
                let (batches, offsets, total_bytes) = seg.take();
                seg.flushed_at = SystemTime::now();
                // Build per-partition meta
                let mut meta: HashMap<PartitionKey, SegmentPartitionMeta> = HashMap::new();
                for (k, v) in batches.iter() {
                    let bytes_estimate = v.iter().map(|b| b.get_array_memory_size() as u64).sum();
                    meta.insert(k.clone(), SegmentPartitionMeta { bytes: bytes_estimate, updated_at: SystemTime::now() });
                }
                let snapshot = SegmentSnapshot::new(Helpers::random_str(16), offsets, batches, meta, total_bytes);
                if Config::log_wal_enabled() || Config::debug_enabled() {
                    let part_count = snapshot.meta.len();
                    let reason = if seg.bytes >= byte_threshold { "size" } else { "time" };
                    println!("Segment rotated: id={} reason={} total_bytes={} partitions={}", snapshot.id, reason, snapshot.total_bytes, part_count);
                }
                if let Ok(mut q) = SEGMENT_SNAPSHOTS.lock() { q.push_back(Arc::new(std::sync::Mutex::new(snapshot))); }
            }
            seg.add(key, batches_vec.drain(..).collect(), &ingest_buffer_batch.offsets);
        }
    }

    pub async fn flush(&mut self, offsets_db: Arc<Offsets>, shared_output: Arc<Box<dyn DataOutputPlugin + Send + Sync>>) -> Result<(), ArrowError> {
        
        let mut bytes: u64 = 0;
        let mut rows: u64 = 0;

        let mut uploaded_bytes: u64 = 0;

        // Drain any pending rotated segments unconditionally so compactor can run during ingest
        loop {
            let next_snapshot_opt = { let mut q = SEGMENT_SNAPSHOTS.lock().unwrap(); q.pop_front() };
            if next_snapshot_opt.is_none() { break; }
            let snap_arc = next_snapshot_opt.unwrap();
            let (snapshot_id, snapshot_offsets, snapshot_batches) = {
                let s = snap_arc.lock().unwrap();
                // Only persist if in Memory durability
                if s.durability != Durability::Memory { continue; }
                (s.id.clone(), s.offsets.clone(), s.batches.clone())
            };
            // Persist to SegmentFile
            let seg_dir = PathBuf::from(format!("{}/segment_buffer/segs", Config::get_data_dir()));
            let seg_file = SegmentFile::new(&seg_dir, &snapshot_id).map_err(|e| ArrowError::from_external_error(Box::new(e)))?;
            let mut partitions_meta: HashMap<PartitionKey, (u64, SystemTime)> = HashMap::new();
            for (k, v) in snapshot_batches.iter() { let bytes_estimate = v.iter().map(|b| b.get_array_memory_size() as u64).sum(); partitions_meta.insert(k.clone(), (bytes_estimate, SystemTime::now())); }
            let (seg_bytes, seg_rows) = seg_file.write_snapshot(&snapshot_offsets, &snapshot_batches, &partitions_meta).map_err(|e| ArrowError::from_external_error(Box::new(e)))?;
            if Config::log_wal_enabled() || Config::debug_enabled() {
                println!("Segment persisted: file={} id={} bytes={} rows={} partitions={}", seg_file.path.to_string_lossy(), snapshot_id, seg_bytes, seg_rows, snapshot_batches.len());
            }
            uploaded_bytes += seg_bytes;
            rows += seg_rows;
            // Update snapshot state to on-disk
            {
                let mut s = snap_arc.lock().unwrap();
                s.durability = Durability::Disk;
            }
            // Publish to index
            for ((namespace, partition, time, shard), batches) in snapshot_batches.into_iter() {
                if batches.is_empty() { continue; }
                let mut per_partition_offsets: HashMap<OffsetKey, u64> = HashMap::new();
                for (ok, pos) in snapshot_offsets.iter() { if ok.namespace == namespace && ok.partition == partition { per_partition_offsets.insert(ok.clone(), *pos); } }
                let partition_key: PartitionKey = (namespace.clone(), partition.clone(), time, shard.clone());
                // No WAL_INDEX: compactor will inspect SEGMENT_SNAPSHOTS to decide work
            }
            for (offset, position) in snapshot_offsets.iter() {
                    let offset_key = OffsetKey { namespace: offset.namespace.clone(), partition: offset.partition.clone() };
                    offsets_db.insert(&offset_key, OffsetTypes::Position, *position);
                    offsets_db.insert(&offset_key, OffsetTypes::Closed, 1);
            }
        }

        metrics_hot::add_wal_write_bytes(bytes);
        metrics_hot::add_wal_write_rows(rows);

        self.buf.clear();

        WAL_BYTES_TOTAL.fetch_add(uploaded_bytes, std::sync::atomic::Ordering::Relaxed);

        Buffers::start_single_consumer(shared_output.clone(), offsets_db.clone());

        Ok(())
    }

    /// Ensure a single background compactor is running.
    ///
    /// The compactor watches `WAL_PARTITION_INDEX` and, when partitions exceed
    /// the configured byte/time thresholds (`buffer_threshold_bytes` / `buffer_threshold_seconds`),
    /// it reads the partition's WAL objects from S3, compacts them into the configured output
    /// (typically Athena S3), and reduces the tracked index bytes accordingly.
    ///
    /// Offsets are NOT committed here; they are committed in `Buffers::flush` immediately after
    /// each WAL object is successfully uploaded to S3, making compaction fully decoupled from ingest.
    pub fn start_single_consumer(shared_output: Arc<Box<dyn DataOutputPlugin + Send + Sync>>, offsets_db: Arc<Offsets>) {
        use std::sync::atomic::Ordering as AO;
        if CONSUMER_STARTED.compare_exchange(false, true, AO::Relaxed, AO::Relaxed).is_err() { return; }
            tokio::spawn(async move {
                loop {
                    match Buffers::compact_one_partition(false, shared_output.clone(), offsets_db.clone()).await {
                        Ok(did_work) => {
                            if !did_work { tokio_sleep(TokioDuration::from_millis(500)).await; }
                        }
                        Err(e) => {
                            println!("Compactor error: {}", e);
                            tokio_sleep(TokioDuration::from_millis(1000)).await;
                        }
                    }
                }
            });
    }

    // Removed compaction dispatch; replaced by single-thread consumer

    // Removed enqueue_compaction

    pub async fn compact_all_partitions(force: bool, offsets_db: Arc<Offsets>, shared_output: Arc<Box<dyn DataOutputPlugin + Send + Sync>>) {
        // Drain/persist in-memory snapshots first so compactor works only on disk
        let _ = flush_all_segments(offsets_db.clone()).await;

        // Parallel compaction of on-disk segment partitions
        use futures::stream::StreamExt;
            let concurrency = crate::metrics::counters::WAL_COMPACTION_CONCURRENCY_TARGET
            .load(std::sync::atomic::Ordering::Relaxed)
            .clamp(1, 64) as usize;

        loop {
            let candidates = Buffers::next_compaction_candidates(concurrency, force);
            if candidates.is_empty() { break; }

            let mut in_flight: futures::stream::FuturesUnordered<Pin<Box<dyn Future<Output = ()> + Send>>> = futures::stream::FuturesUnordered::new();
            for (path, meta, idx) in candidates.into_iter() {
                let out = shared_output.clone();
                let off = offsets_db.clone();
                in_flight.push(Box::pin(async move {
                    let _ = Buffers::compact_segment_partition(&path, &meta, &idx, out, off).await;
                }));
            }
            while let Some(_) = in_flight.next().await {}
        }

        // Sweep: remove any fully-tombstoned segments left behind
        Buffers::sweep_segment_cleanup();
    }

    async fn compact_one_partition(force: bool, shared_output: Arc<Box<dyn DataOutputPlugin + Send + Sync>>, offsets_db: Arc<Offsets>) -> io::Result<bool> {
        let seg_dir = PathBuf::from(format!("{}/segment_buffer/segs", Config::get_data_dir()));
        let mut best: Option<(PathBuf, SegmentFileMetadata, crate::buffer::segment_file::SegmentPartitionIndexEntry)> = None;
        // Scan .seg files and pick the first partition meeting thresholds
        if seg_dir.exists() {
            for entry in fs::read_dir(&seg_dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("seg") { continue; }
                // Read metadata once per file
                let seg = SegmentFile { path: path.clone() };
                let meta = match seg.read_metadata() { Ok(m) => m, Err(e) => { println!("Failed to read segment metadata {}: {}", path.to_string_lossy(), e); continue; } };
                let now_secs = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
                for idx in meta.index.iter() {
                    if Buffers::is_partition_tombstoned(&path, &idx.key) { continue; }
                    let should = Buffers::should_compact(idx.bytes, idx.updated_at_secs, now_secs, force);
                    if should {
                        best = Some((path.clone(), meta.clone(), idx.clone()));
                        break;
                    }
                }
                if best.is_some() { break; }
            }
        }
        if let Some((path, meta, idx)) = best {
            let (ns, part, time, shard) = (&idx.key.0, &idx.key.1, idx.key.2.unwrap_or(0), &idx.key.3);
            println!("Compactor: candidate ns={} part={} time={} shard={} bytes={} updated_at={}", ns, part, time, shard, idx.bytes, idx.updated_at_secs);
            Buffers::compact_segment_partition(&path, &meta, &idx, shared_output.clone(), offsets_db.clone()).await.ok();
            return Ok(true);
        }
        Ok(false)
    }

    fn should_compact(bytes: u64, updated_at_secs: u64, now_secs: u64, force: bool) -> bool {
        if force { return true; }
        let byte_threshold = Config::get_pipeline_buffer_threshold_bytes();
        let time_threshold = Config::get_pipeline_buffer_threshold_seconds() as u64;
        bytes >= byte_threshold || now_secs.saturating_sub(updated_at_secs) >= time_threshold
    }

    fn tombstone_dir() -> PathBuf { PathBuf::from(format!("{}/segment_buffer/done", Config::get_data_dir())) }

    fn partition_tombstone_path(seg_path: &PathBuf, key: &PartitionKey) -> PathBuf {
        let snapshot_id = seg_path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown");
        let (ns, part, time_opt, shard) = key;
        let time = time_opt.unwrap_or(0);
        let safe = |s: &str| s.replace('/', "_");
        let file = format!("{}.seg.{}.{}.{}.{}.tombstone", snapshot_id, safe(ns), safe(part), time, safe(shard));
        Buffers::tombstone_dir().join(file)
    }

    fn is_partition_tombstoned(seg_path: &PathBuf, key: &PartitionKey) -> bool { Buffers::partition_tombstone_path(seg_path, key).exists() }

    fn compute_compaction_id(seg_path: &PathBuf, idx: &crate::buffer::segment_file::SegmentPartitionIndexEntry) -> String {
        let mut hasher = Sha256::new();
        if let Some(name) = seg_path.file_name().and_then(|s| s.to_str()) { hasher.update(name.as_bytes()); }
        hasher.update(&idx.start.to_le_bytes());
        hasher.update(&idx.len.to_le_bytes());
        hasher.update(&idx.bytes.to_le_bytes());
        hasher.update(&idx.updated_at_secs.to_le_bytes());
        let digest = hasher.finalize();
        hex::encode(&digest[..8])
    }

    fn next_compaction_candidates(limit: usize, force: bool) -> Vec<(PathBuf, SegmentFileMetadata, crate::buffer::segment_file::SegmentPartitionIndexEntry)> {
        let mut out: Vec<(PathBuf, SegmentFileMetadata, crate::buffer::segment_file::SegmentPartitionIndexEntry)> = Vec::with_capacity(limit);
        let seg_dir = PathBuf::from(format!("{}/segment_buffer/segs", Config::get_data_dir()));
        if !seg_dir.exists() { return out; }
        for entry in fs::read_dir(&seg_dir).unwrap_or_else(|_| fs::read_dir("/").unwrap()) {
            if out.len() >= limit { break; }
            if let Ok(ent) = entry {
                let path = ent.path();
                if path.extension().and_then(|s| s.to_str()) != Some("seg") { continue; }
                let seg = SegmentFile { path: path.clone() };
                if let Ok(meta) = seg.read_metadata() {
                    let now_secs = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
                    for idx in meta.index.iter() {
                        if Buffers::is_partition_tombstoned(&path, &idx.key) { continue; }
                        if Buffers::should_compact(idx.bytes, idx.updated_at_secs, now_secs, force) {
                            out.push((path.clone(), meta.clone(), idx.clone()));
                            if out.len() >= limit { break; }
                        }
                    }
                }
            }
        }
        out
    }

    fn sweep_segment_cleanup() {
        let seg_dir = PathBuf::from(format!("{}/segment_buffer/segs", Config::get_data_dir()));
        if !seg_dir.exists() { return; }
        for entry in fs::read_dir(&seg_dir).unwrap_or_else(|_| fs::read_dir("/").unwrap()) {
            if let Ok(ent) = entry {
                let p = ent.path();
                if p.extension().and_then(|s| s.to_str()) != Some("seg") { continue; }
                let segf = SegmentFile { path: p.clone() };
                if let Ok(m) = segf.read_metadata() {
                    let mut remaining = 0usize;
                    for idx in m.index.iter() { if !Buffers::is_partition_tombstoned(&p, &idx.key) { remaining += 1; } }
                    if remaining == 0 {
                        match fs::remove_file(&p) {
                            Ok(_) => {
                                println!("Removed fully-compacted segment {}", p.to_string_lossy());
                                // remove all tombstones for this segment
                                for part in m.index.iter() {
                                    let tp = Buffers::partition_tombstone_path(&p, &part.key);
                                    if tp.exists() { if let Err(e) = fs::remove_file(&tp) { println!("Failed to remove tombstone {:?}: {}", tp, e); } }
                                }
                            },
                            Err(e) => println!("Failed to remove fully-compacted segment {}: {}", p.to_string_lossy(), e),
                        }
                    }
                }
            }
        }
    }

    pub fn segs_remaining() -> usize {
        let seg_dir = PathBuf::from(format!("{}/segment_buffer/segs", Config::get_data_dir()));
        if !seg_dir.exists() { return 0; }
        let mut count = 0usize;
        if let Ok(rd) = fs::read_dir(&seg_dir) {
            for e in rd.flatten() { if e.path().extension().and_then(|s| s.to_str()) == Some("seg") { count += 1; } }
        }
        count
    }

    async fn compact_segment_partition(
        seg_path: &PathBuf,
        meta: &SegmentFileMetadata,
        idx: &crate::buffer::segment_file::SegmentPartitionIndexEntry,
        shared_output: Arc<Box<dyn DataOutputPlugin + Send + Sync>>,
        offsets_db: Arc<Offsets>,
    ) -> io::Result<()> {
        let (namespace, partition, time, shard) = (idx.key.0.clone(), idx.key.1.clone(), idx.key.2, idx.key.3.clone());
        let mut out_key = BufferChunker::encode_chunk_name("output", Some(&namespace), Some(&partition), time, Some(&shard));
        let compaction_id = Buffers::compute_compaction_id(seg_path, idx);
        out_key = format!("{}-c={}", out_key, compaction_id);

        println!("Compactor: start ns={} part={} time={} shard={} seg={} bytes={} out_key={}",
            namespace, partition, time.unwrap_or(0), shard, seg_path.to_string_lossy(), idx.bytes, out_key);

        // Validate and clamp partition bounds to avoid corrupt reads
        let file_len = OpenOptions::new().read(true).open(&seg_path)?.metadata()?.len();
        if idx.start >= file_len {
            println!("Compactor: partition start beyond file end for {} start={} len={} file_len={}", seg_path.to_string_lossy(), idx.start, idx.len, file_len);
            return Ok(());
        }
        let safe_len = std::cmp::min(idx.len, file_len.saturating_sub(idx.start));
        if Config::debug_enabled() || Config::log_wal_enabled() {
            let end_hint = idx.start.saturating_add(safe_len);
            println!(
                "Compactor: bounds seg={} file_len={} start={} idx_len={} safe_len={} end_hint={}",
                seg_path.to_string_lossy(), file_len, idx.start, idx.len, safe_len, end_hint
            );
        }

        // Determine schema by opening and creating a StreamReader once (bounded to safe_len)
        let schema: SchemaRef = {
            let mut file = OpenOptions::new().read(true).open(&seg_path)?;
            file.seek(io::SeekFrom::Start(idx.start))?;
            let mut reader = io::BufReader::new(file);
            use std::io::Read as IoRead;
            let mut take = reader.take(safe_len);
            match StreamReader::try_new(&mut take, None) {
                Ok(sr) => sr.schema(),
                Err(e) => {
                    let es = e.to_string();
                    if es.contains("failed to fill whole buffer") || es.contains("UnexpectedEof") {
                        // Retry without length limiting in case the computed len is slightly short
                        let mut file2 = OpenOptions::new().read(true).open(&seg_path)?;
                        file2.seek(io::SeekFrom::Start(idx.start))?;
                        let reader2 = io::BufReader::new(file2);
                        let sr2 = StreamReader::try_new(reader2, None)
                            .map_err(|e2| io::Error::new(io::ErrorKind::Other, format!("arrow: {}", e2)))?;
                        sr2.schema()
                    } else {
                        return Err(io::Error::new(io::ErrorKind::Other, format!("arrow: {}", e)));
                    }
                }
            }
        };

        let (tx, rx) = mpsc::channel::<Result<RecordBatch, DataFusionError>>(8);
        let seg_path_clone = seg_path.clone();
        let start_pos = idx.start;
        let part_len = safe_len;
        tokio::spawn(async move {
            match OpenOptions::new().read(true).open(&seg_path_clone) {
                Ok(mut file) => {
                    if let Err(e) = file.seek(io::SeekFrom::Start(start_pos)) { let _ = tx.send(Err(DataFusionError::IoError(e))); return; }
                    let mut reader = io::BufReader::new(file);
                    // Limit reads to the partition len by wrapping the reader in Take
                    use std::io::Read as IoRead;
                    let mut take = reader.take(part_len);
                    match StreamReader::try_new(&mut take, None) {
                        Ok(sr) => {
                            let mut had_iter_error = false;
                            for item in sr {
                                match item {
                                    Ok(batch) => { if tx.send(Ok(batch)).await.is_err() { break; } },
                                    Err(e) => {
                                        let es = e.to_string();
                                        had_iter_error = true;
                                        if es.contains("failed to fill whole buffer") || es.contains("UnexpectedEof") {
                                            // Retry reading unbounded from start once
                                            match OpenOptions::new().read(true).open(&seg_path_clone) {
                                                Ok(mut f2) => {
                                                    if let Err(e) = f2.seek(io::SeekFrom::Start(start_pos)) { let _ = tx.send(Err(DataFusionError::IoError(e))); break; }
                                                    let reader2 = io::BufReader::new(f2);
                                                    match StreamReader::try_new(reader2, None) {
                                                        Ok(sr2) => {
                                                            for item2 in sr2 {
                                                                match item2 {
                                                                    Ok(batch) => { if tx.send(Ok(batch)).await.is_err() { break; } },
                                                                    Err(e2) => { let _ = tx.send(Err(DataFusionError::ArrowError(e2, None))).await; break; }
                                                                }
                                                            }
                                                        }
                                                        Err(e2) => { let _ = tx.send(Err(DataFusionError::ArrowError(e2, None))).await; }
                                                    }
                                                }
                                                Err(eopen) => { let _ = tx.send(Err(DataFusionError::IoError(eopen))); }
                                            }
                                        } else {
                                            let _ = tx.send(Err(DataFusionError::ArrowError(e, None))).await;
                                        }
                                        break;
                                    }
                                }
                            }
                            if had_iter_error { /* already handled fallback or sent error */ }
                        }
                        Err(e) => {
                            // Fallback: if the limited reader fails due to truncated buffer, retry without the limit
                            let es = e.to_string();
                            if es.contains("failed to fill whole buffer") || es.contains("UnexpectedEof") {
                                match OpenOptions::new().read(true).open(&seg_path_clone) {
                                    Ok(mut f2) => {
                                        if let Err(e) = f2.seek(io::SeekFrom::Start(start_pos)) { let _ = tx.send(Err(DataFusionError::IoError(e))); return; }
                                        let reader2 = io::BufReader::new(f2);
                                        match StreamReader::try_new(reader2, None) {
                                            Ok(sr2) => {
                                                for item in sr2 {
                                                    match item {
                                                        Ok(batch) => { if tx.send(Ok(batch)).await.is_err() { break; } },
                                                        Err(e) => { let _ = tx.send(Err(DataFusionError::ArrowError(e, None))).await; break; }
                                                    }
                                                }
                                            }
                                            Err(e2) => { let _ = tx.send(Err(DataFusionError::ArrowError(e2, None))).await; }
                                        }
                                    }
                                    Err(eopen) => { let _ = tx.send(Err(DataFusionError::IoError(eopen))); }
                                }
                            } else {
                                let _ = tx.send(Err(DataFusionError::ArrowError(e, None))).await;
                            }
                        }
                    }
                }
                Err(e) => { let _ = tx.send(Err(DataFusionError::IoError(e))); }
            }
            drop(tx);
        });

        struct SegRecordBatchStream { schema: SchemaRef, rx: mpsc::Receiver<Result<RecordBatch, DataFusionError>> }
        impl futures::Stream for SegRecordBatchStream {
            type Item = Result<RecordBatch, DataFusionError>;
            fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> TaskPoll<Option<Self::Item>> {
                // Safety: we only move rx
                let inner = unsafe { self.get_unchecked_mut() };
                match inner.rx.poll_recv(cx) {
                    TaskPoll::Ready(Some(item)) => TaskPoll::Ready(Some(item)),
                    TaskPoll::Ready(None) => TaskPoll::Ready(None),
                    TaskPoll::Pending => TaskPoll::Pending,
                }
            }
        }
        impl RecordBatchStream for SegRecordBatchStream { fn schema(&self) -> SchemaRef { self.schema.clone() } }

        let batch_stream: SendableRecordBatchStream = Box::pin(SegRecordBatchStream { schema: schema.clone(), rx });
        if let Err(e) = shared_output.sync(batch_stream, out_key.clone()).await {
            let err_str = e.to_string();
            println!("Compactor upload failed for {}: {}", out_key, err_str);
            // Diagnostics and quarantine for truncated streams to ensure forward progress
            if err_str.contains("failed to fill whole buffer") || err_str.contains("UnexpectedEof") {
                let file_len2 = OpenOptions::new().read(true).open(&seg_path)?.metadata()?.len();
                let safe_len2 = std::cmp::min(idx.len, file_len2.saturating_sub(idx.start));
                let diag = format!(
                    "Compactor diagnostic: seg_path={} file_len={} start={} idx_len={} safe_len={} out_key={} error={}",
                    seg_path.to_string_lossy(), file_len2, idx.start, idx.len, safe_len2, out_key, err_str
                );
                println!("Compactor: truncated-stream quarantine -> {}", diag);
                let tdir = Buffers::tombstone_dir(); let _ = fs::create_dir_all(&tdir);
                let tpath = Buffers::partition_tombstone_path(seg_path, &idx.key);
                // Write tombstone with diagnostic content to skip future attempts on this partition
                if let Err(werr) = fs::write(&tpath, diag.as_bytes()) { println!("Failed to write tombstone {:?}: {}", tpath, werr); }
            }
            return Ok(());
        }

        // Do not commit offsets here; they are committed upon persisting .seg in flush()

        // Write tombstone to avoid reprocessing this partition from this segment
        let tdir = Buffers::tombstone_dir(); let _ = fs::create_dir_all(&tdir);
        let tpath = Buffers::partition_tombstone_path(seg_path, &idx.key);
        if let Err(e) = fs::write(&tpath, b"") { println!("Failed to write tombstone {:?}: {}", tpath, e); }
        println!("Compactor: finished out_key={} tombstone={}", out_key, tpath.to_string_lossy());

        // If all partitions in this segment are tombstoned, delete the .seg file
        let segf = SegmentFile { path: seg_path.clone() };
        match segf.read_metadata() {
            Ok(m) => {
                let mut remaining = 0usize;
                for p in m.index.iter() { if !Buffers::is_partition_tombstoned(seg_path, &p.key) { remaining += 1; } }
                if remaining == 0 {
                    // remove segment file
                    if let Err(e) = fs::remove_file(seg_path) {
                        println!("Failed to remove fully-compacted segment {}: {}", seg_path.to_string_lossy(), e);
                    } else {
                        // println!("Removed fully-compacted segment {}", seg_path.to_string_lossy());
                        // remove all tombstones for this segment
                        for part in m.index.iter() {
                            let tp = Buffers::partition_tombstone_path(seg_path, &part.key);
                            if tp.exists() { if let Err(e) = fs::remove_file(&tp) { println!("Failed to remove tombstone {:?}: {}", tp, e); } }
                        }
                    }
                }
            }
            Err(e) => {
                if e.kind() != io::ErrorKind::NotFound {
                    println!("Failed to re-read segment metadata for cleanup {}: {}", seg_path.to_string_lossy(), e);
                }
            }
        }
        Ok(())
    }

pub fn wal_recover_disk(offsets_db: Arc<Offsets>) -> io::Result<()> {
        let started = std::time::Instant::now();
        let mut count = 0u64;
        let mut bytes = 0u64;

        println!("Indexing WAL (.seg)");

        // Scan the on-disk segment directory for .seg files
        let seg_dir = PathBuf::from(format!("{}/segment_buffer/segs", Config::get_data_dir()));
        let mut seg_files: Vec<PathBuf> = Vec::new();
        if seg_dir.exists() {
            for entry in fs::read_dir(&seg_dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("seg") { seg_files.push(path); }
            }
        }

        let seg_files_count = seg_files.len();
        let mut namespaces: HashSet<String> = HashSet::new();
        let mut namespace_partition_files: HashMap<(String, String, Option<i64>, String), u64> = HashMap::new();
        let mut namespace_partition_bytes: HashMap<(String, String, Option<i64>, String), u64> = HashMap::new();

        let mut committed_offsets: u64 = 0;

        for file_path in seg_files {
            let seg = SegmentFile { path: file_path.clone() };
            match seg.read_metadata() {
                Ok(meta) => {
                    // Accumulate bytes and partitions from index
                    for idx in meta.index.iter() {
                        let key = idx.key.clone();
                        namespaces.insert(key.0.clone());
                        *namespace_partition_files.entry(key.clone()).or_insert(0) += 1;
                        *namespace_partition_bytes.entry(key.clone()).or_insert(0) = (*namespace_partition_bytes.get(&key).unwrap_or(&0)).saturating_add(idx.bytes);
                    }
                    bytes = bytes.saturating_add(meta.total_bytes);
                    count = count.saturating_add(1);
                    // Commit offsets from this segment
                    for (ok, pos) in meta.offsets.iter() {
                        let offset_key = OffsetKey { namespace: ok.namespace.clone(), partition: ok.partition.clone() };
                        offsets_db.insert(&offset_key, OffsetTypes::Position, *pos);
                        offsets_db.insert(&offset_key, OffsetTypes::Closed, 1);
                        committed_offsets = committed_offsets.saturating_add(1);
                    }
                }
                Err(e) => { println!("Failed to read segment metadata {}: {}", file_path.to_string_lossy(), e); }
            }
        }

        let elapsed = started.elapsed().as_secs_f64();
        println!("Indexed {} of {} Segment files for {} namespaces", count, seg_files_count, namespaces.len());
        if elapsed > 0.0 { let rate = (count as f64 / elapsed) as u64; println!("WAL indexing took {:.2}s ~ {} files/s, {} total bytes", elapsed, rate, Helpers::human_readable_size(bytes as u64)); }
        println!("Committed {} offsets from .seg files", committed_offsets);

        let mut wal_index_metrics: WalIndexMetrics = WalIndexMetrics { metrics: Vec::new() };
        for ((ns, _part, _time, _shard), file_count) in namespace_partition_files.iter() {
            let bytes_sum: u64 = namespace_partition_bytes.iter().filter(|(k, _)| &k.0 == ns).map(|(_, v)| *v).sum();
            wal_index_metrics.metrics.push(WalIndexMetric { namespace: ns.clone(), partitions: 0, files: *file_count, bytes: bytes_sum });
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

    /// Blocking S3 WAL index recovery used for end-of-ingest compaction
    pub fn wal_recover_s3(_offsets_db: Arc<Offsets>) -> io::Result<()> { Ok(()) }

    fn list_wal_files() -> io::Result<Vec<PathBuf>> { Ok(Vec::new()) }
}

#[derive(Debug, Clone, Serialize)]
struct WalIndexMetric {
    namespace: String,
    partitions: u64,
    files: u64,
    bytes: u64
}

#[derive(Debug, Clone, Serialize)]
pub struct WalIndexMetrics {
    metrics: Vec<WalIndexMetric>
}

impl WalIndexMetrics {
    pub fn new() -> Self {
        WalIndexMetrics {
            metrics: Vec::new()
        }
    }
}

#[derive(Default, Clone)]
pub struct WalPartitionIndex { /* deprecated */ }

impl WalPartitionIndex {
    fn new() -> Self { WalPartitionIndex { } }
}

pub fn wal_recover(_offsets_db: Arc<Offsets>) -> io::Result<()> {
    // Delegate to disk-based recovery exclusively
    // @todo - implement S3 WAL recovery if we ever re-enable S3 WAL storage
    Buffers::wal_recover_disk(_offsets_db)
}

/// Drain and compact all WAL partitions to the configured output plugin.
/// Consumes partition queues by repeatedly compacting until empty.
pub async fn drain_all_partitions(shared_output: Arc<Box<dyn DataOutputPlugin + Send + Sync>>, offsets: Arc<Offsets>) {
    // Flush any remaining in-memory segments to WAL before compaction
    let _ = flush_all_segments(offsets.clone()).await;

    // Collect partition arcs
    let mut parts: Vec<Arc<tokio::sync::Mutex<WalPartition>>> = Vec::new();

    // Bounded parallel compaction across partitions (single-threaded per partition via the mutex)
    let concurrency = crate::metrics::counters::WAL_COMPACTION_CONCURRENCY_TARGET
        .load(std::sync::atomic::Ordering::Relaxed)
        .clamp(1, 64);

    let mut in_flight: futures::stream::FuturesUnordered<Pin<Box<dyn Future<Output = ()> + Send>>> = futures::stream::FuturesUnordered::new();
    let mut iter = parts.into_iter();

    for _ in 0..concurrency {
        if let Some(part_arc) = iter.next() {
            let offsets_db = offsets.clone();
            let out = shared_output.clone();
            in_flight.push(Box::pin(async move {
                loop {
                    // Lock this partition and compact until empty
                    if let Ok(mut p) = part_arc.try_lock() {
                        if p.len() == 0 { break; }
                        let _ = p.compact_batches_to_parquet(offsets_db.clone(), out.clone()).await;
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
                        if p.len() == 0 { break; }
                        let _ = p.compact_batches_to_parquet(offsets_db.clone(), out.clone()).await;
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
    let mut bytes: u64 = 0;
    let mut rows: u64 = 0;
    let mut uploaded_bytes: u64 = 0;

    // Drain any rotated snapshots first
    loop {
            let next_snapshot_opt = { let mut q = SEGMENT_SNAPSHOTS.lock().unwrap(); q.pop_front() };
            if next_snapshot_opt.is_none() { break; }
            let snap_arc = next_snapshot_opt.unwrap();
            let (snapshot_id, snapshot_offsets, snapshot_batches) = {
                let s = snap_arc.lock().unwrap();
                if s.durability != Durability::Memory { continue; }
                (s.id.clone(), s.offsets.clone(), s.batches.clone())
            };
            let seg_dir = PathBuf::from(format!("{}/segment_buffer/segs", Config::get_data_dir()));
            let seg_file = SegmentFile::new(&seg_dir, &snapshot_id).map_err(|e| ArrowError::from_external_error(Box::new(e)))?;
            let mut partitions_meta: HashMap<PartitionKey, (u64, SystemTime)> = HashMap::new();
            for (k, v) in snapshot_batches.iter() { let bytes_estimate = v.iter().map(|b| b.get_array_memory_size() as u64).sum(); partitions_meta.insert(k.clone(), (bytes_estimate, SystemTime::now())); }
            let (seg_bytes, seg_rows) = seg_file.write_snapshot(&snapshot_offsets, &snapshot_batches, &partitions_meta).map_err(|e| ArrowError::from_external_error(Box::new(e)))?;
            uploaded_bytes += seg_bytes;
            rows += seg_rows;
            {
                let mut s = snap_arc.lock().unwrap();
                s.durability = Durability::Disk;
            }
            for ((namespace, partition, time, shard), batches) in snapshot_batches.into_iter() {
            if batches.is_empty() { continue; }
            let mut per_partition_offsets: HashMap<OffsetKey, u64> = HashMap::new();
                for (ok, pos) in snapshot_offsets.iter() { if ok.namespace == namespace && ok.partition == partition { per_partition_offsets.insert(ok.clone(), *pos); } }
            let partition_key: PartitionKey = (namespace.clone(), partition.clone(), time, shard.clone());
            // compactor will inspect SEGMENT_SNAPSHOTS instead
        }
        for (offset, position) in snapshot_offsets.iter() { let offset_key = OffsetKey { namespace: offset.namespace.clone(), partition: offset.partition.clone() }; offsets_db.insert(&offset_key, OffsetTypes::Position, *position); }
    }

    // Then flush live segment once (force), if it has data
    let (to_flush_batches, to_flush_offsets) = {
        let mut guard = SEGMENT_LIVE.lock().unwrap();
        if guard.batches.is_empty() { (HashMap::new(), HashMap::new()) } else { guard.flushed_at = SystemTime::now(); let (b, o, _bytes) = guard.take(); (b, o) }
    };

    if !to_flush_batches.is_empty() {
        // Write the remaining live segment to a new .seg file so shutdown can compact it
        let snapshot_id = Helpers::random_str(16);
        let seg_dir = PathBuf::from(format!("{}/segment_buffer/segs", Config::get_data_dir()));
        let seg_file = SegmentFile::new(&seg_dir, &snapshot_id).map_err(|e| ArrowError::from_external_error(Box::new(e)))?;
        // Build per-partition meta
        let mut partitions_meta: HashMap<PartitionKey, (u64, SystemTime)> = HashMap::new();
        for (k, v) in to_flush_batches.iter() {
            let bytes_estimate = v.iter().map(|b| b.get_array_memory_size() as u64).sum();
            partitions_meta.insert(k.clone(), (bytes_estimate, SystemTime::now()));
        }
        let (seg_bytes, seg_rows) = seg_file
            .write_snapshot(&to_flush_offsets, &to_flush_batches, &partitions_meta)
            .map_err(|e| ArrowError::from_external_error(Box::new(e)))?;
        println!("Segment persisted (live): file={} id={} bytes={} rows={} partitions={}", seg_file.path.to_string_lossy(), snapshot_id, seg_bytes, seg_rows, to_flush_batches.len());
        uploaded_bytes += seg_bytes;
        rows += seg_rows;
        // Persist offsets Position/Closed now that live segment is durable
        for (offset, position) in to_flush_offsets.iter() {
            let offset_key = OffsetKey { namespace: offset.namespace.clone(), partition: offset.partition.clone() };
            offsets_db.insert(&offset_key, OffsetTypes::Position, *position);
            offsets_db.insert(&offset_key, OffsetTypes::Closed, 1);
        }
    }

    metrics_hot::add_wal_write_bytes(bytes);
    metrics_hot::add_wal_write_rows(rows);
    WAL_BYTES_TOTAL.fetch_add(uploaded_bytes, std::sync::atomic::Ordering::Relaxed);

    Ok(())
}

#[derive(Clone)]
enum WalEntry {
    Segment { path: PathBuf, key: PartitionKey, offsets: HashMap<OffsetKey, u64>, bytes: u64, updated_at: SystemTime },
}

impl WalEntry {
    fn bytes(&self) -> u64 { match self { WalEntry::Segment { bytes, .. } => *bytes } }
    fn updated_at(&self) -> SystemTime { match self { WalEntry::Segment { updated_at, .. } => *updated_at } }
    fn offsets(&self) -> &HashMap<OffsetKey, u64> { match self { WalEntry::Segment { offsets, .. } => offsets } }
}

fn load_partition_segment_counter(namespace: &str, partition: &str, time: Option<i64>, shard: &str) -> Option<u64> {
        let dir = WalFile::get_wal_partition_dir(namespace, partition, time, shard);
        let base = BufferChunker::encode_chunk_name("ingest", Some(namespace), Some(partition), time, Some(shard));
        let path = PathBuf::from(format!("{}/{}-segment.counter", dir, base));
        if let Ok(s) = fs::read_to_string(path) { return s.trim().parse::<u64>().ok(); }
    None
}

fn persist_partition_segment_counter(namespace: &str, partition: &str, time: Option<i64>, shard: &str, next_segment_id: u64) {
        let dir = WalFile::get_wal_partition_dir(namespace, partition, time, shard);
        let base = BufferChunker::encode_chunk_name("ingest", Some(namespace), Some(partition), time, Some(shard));
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
    pub(crate) partition: String,
    pub(crate) time: Option<i64>,
    pub(crate) shard: String,
}

impl WalPartition {
    pub fn len(&self) -> usize { self.queue.len() }
    pub fn clear_queue(&mut self) { self.queue.clear(); self.bytes = 0; }
    fn schemas_equivalent(a: &SchemaRef, b: &SchemaRef) -> bool {
        let sa = a.as_ref();
        let sb = b.as_ref();
        if sa.fields().len() != sb.fields().len() { return false; }
        for (fa, fb) in sa.fields().iter().zip(sb.fields().iter()) {
            if fa.name() != fb.name() { return false; }
            if fa.data_type() != fb.data_type() { return false; }
            if fa.is_nullable() != fb.is_nullable() { return false; }
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
            // println!("Compacting WAL: {} Bytes: {}, Segment Files: {}", self.namespace, self.bytes, self.files.len());
            let elapsed = SystemTime::now().duration_since(self.updated_at).unwrap().as_secs();
            println!(
                "Compacting WAL partition Namespace: {}, Partition: {}, Time: {}, of Bytes: {}, Elapsed Secs: {}, Queue Len: {}",
                self.namespace,
                self.partition,
                self.time.unwrap_or(0),
                Helpers::human_readable_size(self.bytes),
                elapsed,
                self.queue.len()
            );
            return true
        }

        false
    }

    pub(crate) async fn compact_batches_to_parquet(&mut self, _offsets_db: Arc<Offsets>, shared_output: Arc<Box<dyn DataOutputPlugin + Send + Sync>>) -> u64 {
        let data_dir = Config::get_data_dir();
        let mut output_file_name = BufferChunker::encode_chunk_name(
            "output",
            Some(&self.namespace),
            Some(&self.partition),
            self.time,
            Some(&self.shard)
        );

        // NOTE: offsets are committed AFTER successful upload now (moved below)

        // println!("Compacting WAL partition to Parquet, Namespace: {} Partition: {} {}", self.namespace, self.partition, self.time.unwrap_or(0));

        let mut wal_compacted_bytes_total = 0;
        let mut wal_compacted_files_total = 0;
       
        // Deterministic compaction id
        let mut hasher = Sha256::new();
        for e in self.queue.iter() {
            if let WalEntry::Segment { path, bytes, updated_at, .. } = e { hasher.update(path.as_os_str().as_encoded_bytes()); hasher.update(&bytes.to_le_bytes()); let ts = updated_at.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_nanos(); hasher.update(&ts.to_le_bytes()); }
        }
        let compaction_id = {
            let digest = hasher.finalize();
            hex::encode(&digest[..8])
        };
        output_file_name = format!("{}-c={}", output_file_name, compaction_id);

        // Choose a slice from the front of the queue up to byte threshold
        let mut segment_files: Vec<WalEntry> = Vec::new();
        if self.queue.is_empty() {
                println!("No WAL files to compact for partition: {} {}", self.namespace, self.partition);
                return wal_compacted_bytes_total;
            }
        let mut acc_bytes: u64 = 0;
        for e in self.queue.iter() {
            if acc_bytes >= Config::get_pipeline_buffer_threshold_bytes() { break; }
            acc_bytes = acc_bytes.saturating_add(e.bytes());
            segment_files.push(e.clone());
        }
        if segment_files.is_empty() { return 0; }

        let schema: SchemaRef = {
            let mut schema_opt: Option<SchemaRef> = None;
            // For Segment entries, we will read schema later from SegmentFile; fallback not needed here
            // Keep returning an error if none found
            match schema_opt {
                Some(s) => s,
                None => { println!("Failed to read schema from local WALs for namespace: {}", self.namespace); return wal_compacted_bytes_total; }
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
            println!(
                "Compactor: start ns={} part={} shard={} time={} files={} out_key={}",
                self.namespace, self.partition, self.shard, self.time.unwrap_or(0), files.len(), output_file_name
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
                                if let Err(e) = reader.read_exact(&mut offset_size) { println!("ERROR: Failed to read segment header for {}: {}", path.to_string_lossy(), e); continue; }
                            let skip = u64::from_le_bytes(offset_size);
                                if let Err(e) = reader.seek(io::SeekFrom::Current(skip as i64)) { println!("ERROR: Failed to seek segment stream {}: {}", path.to_string_lossy(), e); continue; }
                                match StreamReader::try_new(reader, None) {
                                    Ok(sr) => {
                                        for item in sr { match item { Ok(batch) => {
                                            if !WalPartition::schemas_equivalent(&batch.schema(), &schema_clone) { println!("ERROR: Skipping WAL batch due to schema mismatch for ns={} part={} time={}", namespace, partition, time_val.unwrap_or(0)); batch_mismatch_counter.fetch_add(1, AtomicOrdering::Relaxed); continue; }
                                            row_counter_task.fetch_add(batch.num_rows() as u64, AtomicOrdering::Relaxed);
                                            batch_ok_counter.fetch_add(1, AtomicOrdering::Relaxed);
                                            if tx.send(Ok(batch)).await.is_err() { break; }
                                        }, Err(e) => { batch_error_counter.fetch_add(1, AtomicOrdering::Relaxed); let _ = tx.send(Err(DataFusionError::ArrowError(e, None))).await; break; } } }
                                    }
                                    Err(e) => { println!("Failed to init Arrow stream for local segment {}: {}", path.to_string_lossy(), e); }
                                }
                            }
                            Err(e) => { println!("Failed to open segment file {}: {}", path.to_string_lossy(), e); }
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
            fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> TaskPoll<Option<Self::Item>> {
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
            fn schema(&self) -> SchemaRef { self.schema.clone() }
        }

        let batch_stream: SendableRecordBatchStream = Box::pin(WalRecordBatchStream { schema: schema.clone(), rx });


        // (No re-upload here; WALs were uploaded earlier in flush prior to offset commit.)

        match shared_output.sync(batch_stream, output_file_name.clone()).await {
            Ok(()) => {
                metrics_hot::add_wal_compacted_bytes(wal_compacted_bytes_total);
                let wal_compacted_rows_total = row_counter.load(AtomicOrdering::Relaxed);
                metrics_hot::add_wal_compacted_rows(wal_compacted_rows_total);
                metrics_hot::add_wal_compacted_files(wal_compacted_files_total);

                // On successful compaction, mark offsets as Closed
                for wal_entry in segment_files.iter() {
                    let WalEntry::Segment { offsets, .. } = wal_entry;
                    for (offset, position) in offsets.iter() {
                        let offset_key = OffsetKey { namespace: offset.namespace.clone(), partition: offset.partition.clone() };
                        _offsets_db.insert(&offset_key, OffsetTypes::Closed, *position);
                    }
                }

                    for wal_entry in segment_files.iter() {
                    let WalEntry::Segment { path, .. } = wal_entry;
                        let tombstone_path = format!("{}/ingest_buffer/done/{}.tombstone", data_dir, Helpers::random_str(32));
                    if let Err(e) = fs::rename(&path, tombstone_path) { println!("Failed to tombstone segment file: {}, Error: {}", path.to_string_lossy(), e); }
                    }
                    self.prune_tombstone_wals();
                // Remove exactly the entries we compacted from the front of the queue
                for _ in 0..segment_files.len() {
                    if let Some(front) = self.queue.pop_front() {
                        self.bytes = self.bytes.saturating_sub(front.bytes());
                    }
                }
            },
            Err(e) => {
                let output_plugin_name = Config::get_pipeline_output_plugin_name();
                println!("Failed to sync WAL partition to output plugin: {}, Namespace {}, Partition {}, Error {}", output_plugin_name, self.namespace, self.partition, e);
            }
        }

        wal_compacted_bytes_total
    }

    #[allow(dead_code)]
    async fn apply_sql_on_ipc_stream(temp_parquet_path: &str, _sql: &str, _schema_ref: SchemaRef) -> Result<Vec<RecordBatch>, ArrowError> {

        let mut session_config = SessionConfig::new();
        session_config = session_config.set("datafusion.catalog.information_schema", "true".into());
        session_config = session_config.set("datafusion.catalog.default_catalog", "skippr".into());
        session_config = session_config.set("datafusion.execution.collect_statistics", "true".into());

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
            .read_parquet(
                temp_parquet_path,
                ParquetReadOptions {
                    // schema: Some(schema_ref.as_ref()),
                    schema: None,
                    file_extension: "parquet",
                    table_partition_cols: vec![],
                    parquet_pruning: None,
                    skip_metadata: Some(true),
                    file_sort_order: vec![],
                },
            )
            .await.unwrap();
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
    async fn apply_sql_on_ipc_record_batches(record_batches: Vec<RecordBatch>, _sql: &str, _schema_ref: SchemaRef) -> Result<Vec<RecordBatch>, ArrowError> {

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
struct CompactionTask {
    key: (String, String, Option<i64>, String),
    force: bool,
}

// Public drain used at end-of-ingest to compact any remaining WALs regardless of thresholds
pub async fn force_drain_all(_offsets_db: Arc<Offsets>, _shared_output: Arc<Box<dyn DataOutputPlugin + Send + Sync>>) { }

#[derive(Clone)]
pub struct WalFile {
    pub(crate) path: PathBuf,
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
    pub fn new(namespace: &str, partition: &str, time: Option<i64>, shard: &str, offsets: HashMap<OffsetKey, u64>,) -> io::Result<Self> {

        let path_str = Self::generate_temp_wal_file_name(namespace, partition, time, shard);
        let path= PathBuf::from(&path_str);

        // Ensure file exists and then allow the fp to drop out of scope to limit open file handles
        let fd = OpenOptions::new().append(true).read(true).create(true).open(&path)?;
        fd.sync_all()?;
        unsafe { // belt and braces
            libc::fsync(fd.as_raw_fd());
        };

        Ok(WalFile {
            path,
            bytes: 0,
            namespace: namespace.to_string(),
            partition: partition.to_string(),
            time,
            shard: shard.to_string(),
            file: Arc::new(TimedRwLock::new("wal_file".to_string(), None)),
            updated_at: SystemTime::now(),
            offsets
        })
    }

    fn get_or_open_file(&self) -> io::Result<File> {
        // let mut file_lock = self.file.write();
        OpenOptions::new().read(true).create(true).append(true).open(&self.path)
        // file.try_clone()
        // if file.is_none() {
        //     *file_lock = Some(OpenOptions::new().write(true).read(true).create(true).open(&self.path)?);
        // }
        // // Clone the file handle via Arc. File itself does not implement Clone.
        // file_lock.as_ref().unwrap().try_clone()
    }

    fn from_path(path: &PathBuf) -> io::Result<Self> {

        let mut file = OpenOptions::new()
            .read(true)
            .open(&path)?;

        let offsets = Self::offset_from_file(&mut file)?;

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
            bytes,
            namespace,
            partition,
            time,
            shard,
            file: Arc::new(TimedRwLock::new("wal_file".to_string(), None)),
            updated_at,
            offsets
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

        reader.seek(io::SeekFrom::Current(i64::from_le_bytes(offset_size))).unwrap();

       Ok(usize::from_le_bytes(offset_size))

    }

    pub fn read_schema_from_stream(&mut self) -> Result<SchemaRef, ArrowError> {

        let file= self.get_or_open_file()?;
        let mut reader = io::BufReader::new(&file);

        Self::seek_offset(&mut reader)?;

        let stream_reader = StreamReader::try_new(reader, None)?;

        let schema = stream_reader.schema();

        Ok(schema)
    }

    pub fn read_from_stream(&mut self) -> Result<Vec<RecordBatch>, ArrowError> {

        let file= self.get_or_open_file()?;
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
    pub fn write_to_stream(&mut self, record_batches: &[RecordBatch]) -> Result<(u64, u64), ArrowError> {
        // let writer = self.file.as_mut().ok_or(ArrowError::IoError("Can't write to WAL file".to_string(), io::Error::new(io::ErrorKind::NotFound, "File not found")))?;

        let file = self.get_or_open_file().expect("Failed to clone WAL file for write");
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

        let mut stream_writer = StreamWriter::try_new_with_options(writer, &record_batches[0].schema(), options)?;
        for batch in record_batches {
            _size += batch.get_array_memory_size(); // @todo - account for compression ratio, observed ~50% reduction
            row_count += batch.num_rows();
            stream_writer.write(batch).expect("Failed to write record batch to stream writer");
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

        let file = self.get_or_open_file().expect("Failed to get file for metadata read");
        self.bytes += file.metadata().unwrap().len();

        Ok((self.bytes, row_count as u64))
    }

    fn get_wal_partition_dir(_namespace: &str, _partition: &str, _time: Option<i64>, shard: &str) -> String {
        let data_dir = Config::get_data_dir();

        // let metrics_guard = METRICS.read();
        // let run_id = metrics_guard.run_id.clone();
        let output_dir = &format!("{}/ingest_buffer", data_dir);

        let shard = match shard {
            "" => "none",
            _ => shard
        };

        let wal_partition_dir = format!("{}/{}", output_dir, shard);

        fs::create_dir_all(&wal_partition_dir).expect("Failed to create WAL partition directories");

        wal_partition_dir


    }

    fn generate_temp_wal_file_name(namespace: &str, partition: &str, time: Option<i64>, shard: &str) -> String {

        let wal_file_name = BufferChunker::encode_chunk_name(
            "ingest",
            Some(namespace),
            Some(partition),
            time,
            Some(shard),
        );

        let wal_partition_dir = WalFile::get_wal_partition_dir(namespace, partition, time, shard);

        let wal_file_name = format!("{}/{}&id={}", wal_partition_dir, wal_file_name, Helpers::random_str(32));

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
        let fd = self.get_or_open_file().unwrap().as_raw_fd();
        unsafe {
            libc::fsync(fd);
        };

        Ok(())
    }

    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.get_or_open_file().unwrap().write_all(buf)
    }
}

pub fn _get_partition_dir(_namespace: &str, _partition: &str, _time: Option<i64>, shard: &str) -> String {
    let data_dir = Config::get_data_dir();

    // let metrics_guard = METRICS.read();
    // let run_id = metrics_guard.run_id.clone();
    let output_dir = &format!("{}/ingest_buffer", data_dir);

    let shard = match shard {
        "" => "none",
        _ => shard
    };

    let wal_partition_dir = format!("{}/{}", output_dir, shard);

    fs::create_dir_all(&wal_partition_dir).expect("Failed to create WAL partition directories");

    wal_partition_dir
}

