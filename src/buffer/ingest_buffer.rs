use std::fs::{File, OpenOptions};
use std::{fs, io};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::{BufReader, Read, Seek, Write};
use std::path::PathBuf;
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

type PartitionKey = (String, String, Option<i64>, String);
use tokio::sync::mpsc;

#[allow(dead_code)]
pub static TOTAL_ROWS: Lazy<TimedRwLock<AtomicU64>> =
    Lazy::new(|| TimedRwLock::new("record_batch_total".to_string(), AtomicU64::new(0)));

// Lock-free WAL index and counters
pub(crate) static WAL_INDEX: Lazy<DashMap<PartitionKey, Arc<tokio::sync::Mutex<WalPartition>>>> = Lazy::new(|| DashMap::new());
pub static WAL_BYTES_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
static CONSUMER_STARTED: Lazy<std::sync::atomic::AtomicBool> = Lazy::new(|| std::sync::atomic::AtomicBool::new(false));

// Per-partition notify for quick wakeups
static PARTITION_NOTIFIES: Lazy<DashMap<PartitionKey, Arc<tokio::sync::Notify>>> = Lazy::new(|| DashMap::new());

// Global in-memory segment accumulator (reduces tiny WAL files across threads)
static GLOBAL_SEGMENT: Lazy<Arc<tokio::sync::Mutex<GlobalSegment>>> = Lazy::new(|| Arc::new(tokio::sync::Mutex::new(GlobalSegment::new())));

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
}

impl GlobalSegment {
    fn new() -> Self { GlobalSegment { batches: HashMap::with_capacity(64), offsets: HashMap::new(), bytes: 0, updated_at: SystemTime::now() } }
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

            if let Ok(mut seg) = GLOBAL_SEGMENT.try_lock() { seg.add(key, batches_vec.drain(..).collect(), &ingest_buffer_batch.offsets); }
        }
    }

    pub async fn flush(&mut self, offsets_db: Arc<Offsets>, shared_output: Arc<Box<dyn DataOutputPlugin + Send + Sync>>) -> Result<(), ArrowError> {
        
        let mut bytes: u64 = 0;
        let mut rows: u64 = 0;

        let mut uploaded_bytes: u64 = 0;

        // 4MB default
        let byte_threshold = 4 * 1024 * 1024;
        // 60 seconds default
        let time_threshold = 60;

        // Decide whether to flush global segment
        let (to_flush_batches, to_flush_offsets) = {
            let mut guard = match GLOBAL_SEGMENT.try_lock() { Ok(g) => g, Err(_) => return Ok(()) };
            let age_secs = guard.updated_at.elapsed().map(|d| d.as_secs()).unwrap_or(0);
            let should_flush = guard.bytes >= byte_threshold || age_secs >= time_threshold as u64;
            if !should_flush || guard.batches.is_empty() { return Ok(()); }
            let (batches, offsets, _bytes) = guard.take();
            (batches, offsets)
        };

        // For each partition, write a single WAL file streaming all batches
        for ((namespace, partition, time, shard), batches) in to_flush_batches.into_iter() {
            if batches.is_empty() { continue; }
            // Filter offsets by partition
            let mut per_partition_offsets: HashMap<OffsetKey, u64> = HashMap::new();
            for (ok, pos) in to_flush_offsets.iter() { if ok.namespace == namespace && ok.partition == partition { per_partition_offsets.insert(ok.clone(), *pos); } }

            let mut wal_file = WalFile::new(&namespace, &partition, time, &shard, HashMap::new())?;
            let stat = wal_file.write_to_stream(&batches)?;
            wal_file.flush()?;
            wal_file.finish()?;
            uploaded_bytes += wal_file.bytes;
            bytes += stat.0;
            rows += stat.1;

            // Publish to in-memory WAL index
            let partition_key: PartitionKey = (namespace.clone(), partition.clone(), time, shard.clone());
            match WAL_INDEX.entry(partition_key.clone()) {
                dashmap::mapref::entry::Entry::Occupied(mut occ) => {
                    if let Ok(mut part) = occ.get_mut().try_lock() {
                        part.bytes = part.bytes.saturating_add(wal_file.bytes);
                        part.updated_at = SystemTime::now();
                        part.queue.push_back(WalEntry::Disk { wal: wal_file, offsets: per_partition_offsets.clone() });
                        if let Some(n) = PARTITION_NOTIFIES.get(&partition_key) { n.notify_waiters(); }
                    }
                }
                dashmap::mapref::entry::Entry::Vacant(vac) => {
                    let mut part = WalPartition {
                        queue: std::collections::VecDeque::new(),
                        bytes: wal_file.bytes,
                        updated_at: SystemTime::now(),
                        namespace: namespace.clone(),
                        partition: partition.clone(),
                        time,
                        shard: shard.clone(),
                    };
                    part.queue.push_back(WalEntry::Disk { wal: wal_file, offsets: per_partition_offsets.clone() });
                    let arc = Arc::new(tokio::sync::Mutex::new(part));
                    vac.insert(arc);
                    PARTITION_NOTIFIES.insert(partition_key.clone(), Arc::new(tokio::sync::Notify::new()));
                    if let Some(n) = PARTITION_NOTIFIES.get(&partition_key) { n.notify_waiters(); }
                }
            }
        }

        // Persist all offsets Position atomically for the flushed segment
        for (offset, position) in to_flush_offsets.iter() {
            let offset_key = OffsetKey { namespace: offset.namespace.clone(), partition: offset.partition.clone() };
            offsets_db.insert(&offset_key, OffsetTypes::Position, *position);
        }

        metrics_hot::add_wal_write_bytes(bytes);
        metrics_hot::add_wal_write_rows(rows);

        self.buf.clear();

        WAL_BYTES_TOTAL.fetch_add(uploaded_bytes, std::sync::atomic::Ordering::Relaxed);

        Buffers::start_single_consumer(shared_output.clone());

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
    fn start_single_consumer(shared_output: Arc<Box<dyn DataOutputPlugin + Send + Sync>>) {
        use std::sync::atomic::Ordering as AO;
        if CONSUMER_STARTED.compare_exchange(false, true, AO::Relaxed, AO::Relaxed).is_err() { return; }
            tokio::spawn(async move {
            loop {
                // Single-threaded: process at most one partition per tick
                let mut worked = false;
                    for item in WAL_INDEX.iter() {
                    if let Some(entry) = WAL_INDEX.get(item.key()) {
                        if let Ok(mut part) = entry.try_lock() {
                            if part.check_wal_rotate(false) {
                                // Peek entries until threshold; stream and then pop
                                let _ = part.compact_batches_to_parquet(Arc::new(Offsets::init().unwrap()), shared_output.clone()).await;
                                part.clear_queue();
                                worked = true;
                                break;
                            }
                        }
                    }
                }
                if !worked {
                    if let Some(n) = PARTITION_NOTIFIES.iter().next().map(|e| e.value().clone()) { n.notified().await; } else { tokio_sleep(TokioDuration::from_millis(200)).await; }
                }
            }
        });
    }

    // Removed compaction dispatch; replaced by single-thread consumer

    // Removed enqueue_compaction

    pub async fn compact_all_partitions(_force: bool, offsets_db: Arc<Offsets>, shared_output: Arc<Box<dyn DataOutputPlugin + Send + Sync>>) {

        loop {
            // Step 1: rebuild the index (disk)
            wal_recover(offsets_db.clone()).expect("Failed to recover WAL index");
            let partitions_to_compact: Vec<WalPartition> = {
                let mut parts = Vec::new();
                for item in WAL_INDEX.iter() {
                    let key = item.key().clone();
                    let part_arc = item.value().clone();
                    // Lock briefly to decide and pop one closed segment
                    let g = part_arc.lock().await;
                    parts.push(WalPartition {
                        queue: g.queue.clone(),
                        bytes: g.bytes,
                        updated_at: g.updated_at,
                        namespace: g.namespace.clone(),
                        partition: g.partition.clone(),
                        time: g.time,
                        shard: g.shard.clone(),
                    });
                }
                parts
            };

            if partitions_to_compact.is_empty() {
                break;
            }

            // Step 2: Parallel compaction with bounded concurrency
            let concurrency = crate::metrics::counters::WAL_COMPACTION_CONCURRENCY_TARGET
                .load(std::sync::atomic::Ordering::Relaxed)
                .clamp(1, 64);

            let mut in_flight: futures::stream::FuturesUnordered<Pin<Box<dyn Future<Output = u64> + Send>>> = futures::stream::FuturesUnordered::new();
            let mut iter = partitions_to_compact.into_iter();

            for _ in 0..concurrency {
                if let Some(mut p) = iter.next() {
                    // Ensure we rotate the open files into a closed segment before compaction
                    // queue model: no rotation needed
                    let offsets_db_clone = offsets_db.clone();
                    let shared_output2 = shared_output.clone();
                    in_flight.push(Box::pin(async move {
                        let bytes = p.compact_batches_to_parquet(offsets_db_clone, shared_output2).await;
                        bytes
                    }));
                }
            }

            let mut total_compacted_bytes: u64 = 0;
            while let Some(bytes_done) = in_flight.next().await {
                total_compacted_bytes += bytes_done as u64;
                if let Some(mut p) = iter.next() {
                    // queue model: no rotation needed
                    let offsets_db_clone = offsets_db.clone();
                    let shared_output2 = shared_output.clone();
                    in_flight.push(Box::pin(async move {
                        let bytes = p.compact_batches_to_parquet(offsets_db_clone, shared_output2).await;
                        bytes
                    }));
                }
            }

            // Step 3: Reduce WAL bytes counter
            WAL_BYTES_TOTAL.fetch_update(std::sync::atomic::Ordering::Relaxed, std::sync::atomic::Ordering::Relaxed, |v| Some(v.saturating_sub(total_compacted_bytes))).ok();
            // Loop again; will break when no segments remain
        }
    }

    pub fn wal_recover_disk(offsets_db: Arc<Offsets>) -> io::Result<()> {
        let started = std::time::Instant::now();
        let mut count = 0;
        let mut bytes = 0;

        println!("Indexing WAL");

        let wal_files = Self::list_wal_files()?;

        let wal_files_count = wal_files.len();

        if wal_files_count == 0 {
            println!("Indexed {} of {} WAL files", count, wal_files_count);
            return Ok(());
        }

        let mut namespaces = HashSet::new();
        let mut namespace_partitions: HashMap<String, HashSet<(String, String, Option<i64>, String)>> = HashMap::new();
        let mut namespace_partition_files: HashMap<(String, String, Option<i64>, String), u64> = HashMap::new();
        let mut namespace_partition_bytes: HashMap<(String, String, Option<i64>, String), u64> = HashMap::new();

        for file_path in wal_files {

            let wal_file = match WalFile::from_path(&file_path) {
                Ok(wal_file) => wal_file,
                Err(e) => {
                    println!("Failed to index WAL file: {} of bytes: {}, Error {}.", file_path.to_str().unwrap(), file_path.metadata().unwrap().len(), e);
                    continue;
                }
            };

            let partition_key = (wal_file.namespace.clone(), wal_file.partition.clone(), wal_file.time.clone(), wal_file.shard.clone());

            namespaces.insert(wal_file.namespace.clone());
            namespace_partitions.entry(wal_file.namespace.clone()).or_insert_with(|| HashSet::new()).insert(partition_key.clone());
            let val = namespace_partition_files.entry(partition_key.clone()).or_insert(0);
            *val += 1;

            namespace_partition_bytes.entry(partition_key.clone()).or_insert(0);
            let val = namespace_partition_bytes.entry(partition_key.clone()).or_insert(0);
            *val += wal_file.bytes;

            count += 1;
            bytes += wal_file.bytes;

            use dashmap::mapref::entry::Entry;
            match WAL_INDEX.entry(partition_key.clone()) {
                Entry::Occupied(mut occ) => {
                    if let Ok(mut part) = occ.get_mut().try_lock() {
                        if part.updated_at < wal_file.updated_at { part.updated_at = wal_file.updated_at; }
                        part.bytes += wal_file.bytes;
                        part.queue.push_back(WalEntry::from_disk(wal_file.clone()));
                    }
                }
                Entry::Vacant(vac) => {
                    let mut part = WalPartition {
                        queue: std::collections::VecDeque::new(),
                        bytes: 0,
                        updated_at: SystemTime::UNIX_EPOCH,
                        namespace: partition_key.0.clone(),
                        partition: partition_key.1.clone(),
                        time: partition_key.2,
                        shard: partition_key.3.clone(),
                    };
                    if part.updated_at < wal_file.updated_at { part.updated_at = wal_file.updated_at; }
                    part.bytes += wal_file.bytes;
                    part.queue.push_back(WalEntry::from_disk(wal_file.clone()));
                    vac.insert(Arc::new(tokio::sync::Mutex::new(part)));
                }
            }

            WAL_BYTES_TOTAL.fetch_add(wal_file.bytes, AtomicOrdering::Relaxed);

            if count % 1000 == 0 { println!("Indexed {} of {} WAL files for {} namespaces in {} partitions", count, wal_files_count, namespaces.len(), WAL_INDEX.len()); }
        }

        if Config::debug_enabled() || Config::log_wal_enabled() { println!("Indexed {} of {} WAL files for {} namespaces in {} partitions", count, wal_files_count, namespaces.len(), WAL_INDEX.len()); }
        let elapsed = started.elapsed().as_secs_f64();
        if elapsed > 0.0 {
            let rate = (count as f64 / elapsed) as u64;
            let real_rate = rate.min(wal_files_count as u64);
            println!("WAL indexing took {:.2}s ~ {} files/s, {} total bytes", elapsed, real_rate, Helpers::human_readable_size(bytes as u64));
        }

        let mut wal_index_metrics: WalIndexMetrics = WalIndexMetrics { metrics: Vec::new() };

        for (namespace, partition_key) in namespace_partitions {
            let human_bytes = Helpers::human_readable_size(namespace_partition_bytes.iter().filter(|(k, _v)| k.0 == namespace).map(|(_k, v)| v).sum::<u64>());
            if Config::debug_enabled() || Config::log_wal_enabled() { println!("Namespace {} contains {} partitions and {} files of {}", namespace, partition_key.len(), namespace_partition_files.iter().filter(|(k, _v)| k.0 == namespace).map(|(_k, v)| v).sum::<u64>(), human_bytes); }

            wal_index_metrics.metrics.push(WalIndexMetric {
                namespace: namespace.clone(),
                partitions: partition_key.len() as u64,
                files: namespace_partition_files.iter().filter(|(k, _v)| k.0 == namespace).map(|(_k, v)| v).sum(),
                bytes: namespace_partition_bytes.iter().filter(|(k, _v)| k.0 == namespace).map(|(_k, v)| v).sum(),
            });
        }

        {
            let mut metrics = METRICS.write();
            metrics.wal_index_namespaces_total = namespaces.len() as u64;
            metrics.wal_index_partitions_total = WAL_INDEX.len() as u64;
            metrics.wal_index_files_total = count as u64;
            metrics.wal_index_bytes_total = WAL_BYTES_TOTAL.load(AtomicOrdering::Relaxed) as u64;
            metrics.wal_index_metrics = wal_index_metrics;
        }

        println!("Syncing offsets form WAL to DB");

        for item in WAL_INDEX.iter() {
            let wal_partition_arc = item.value();
            if let Ok(mut wal_partition) = wal_partition_arc.try_lock() {
            // ensure offsets Position persisted; Closed will be set on successful compaction
            for wal_file in wal_partition.queue.iter() {
                wal_file.offsets().iter().for_each(|(offset, position)| {
                    let offset_key = OffsetKey { namespace: offset.namespace.clone(), partition: offset.partition.clone() };
                    offsets_db.insert(&offset_key, OffsetTypes::Position, *position);
                });
            }
            }
        }

        offsets_db.flush();

        println!("Indexed {} of {} WAL files", count, wal_files_count);

        Ok(())
    }

    /// Blocking S3 WAL index recovery used for end-of-ingest compaction
    pub fn wal_recover_s3(offsets_db: Arc<Offsets>) -> io::Result<()> {
        // Reverted: Delegate to disk-based recovery exclusively
        Self::wal_recover_disk(offsets_db)
    }

    fn list_wal_files() -> io::Result<Vec<PathBuf>> {
        let data_dir = Config::get_data_dir();
        let wal_dir = PathBuf::from(format!("{}/ingest_buffer", data_dir));
        let mut wal_files = Vec::new();

        // remove empty dirs
        for entry in glob_with(&format!("{}/**/*", wal_dir.to_str().unwrap()), MatchOptions {
            case_sensitive: false,
            require_literal_separator: false,
            require_literal_leading_dot: false,
        }).expect("Failed to clean WAL dir") {

            let path = entry.expect("Failed to read WAL file");

            // remove empty files
            if path.is_file() && path.metadata().unwrap().len() == 0 {
                if Config::debug_enabled() { println!("Removing empty WAL file: {}", path.to_str().unwrap()); }
                fs::remove_file(&path).unwrap();
            }

            // remove .tmp files
            if path.is_file() && path.extension().and_then(OsStr::to_str) == Some("tmp") {
                if Config::debug_enabled() { println!("Removing temp WAL file: {}", path.to_str().unwrap()); }
                fs::remove_file(&path).unwrap();
            }

            // remove empty dirs
            // if path.is_dir() {
            //     if fs::read_dir(&path).unwrap().count() == 0 {
            //         println!("Removing empty WAL dir: {}", path.to_str().unwrap());
            //         fs::remove_dir_all(&path).unwrap();
            //     }
            // }
        }

        let options = MatchOptions {
            case_sensitive: false,
            require_literal_separator: false,
            require_literal_leading_dot: false,
        };

        for entry in glob_with(&format!("{}/**/*.wal", wal_dir.to_str().unwrap()), options).expect("Failed to read WAL files") {

            let path = entry.expect("Failed to read WAL file");
            wal_files.push(path);
        }
        Ok(wal_files)
    }
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
    for item in WAL_INDEX.iter() {
        if let Some(entry) = WAL_INDEX.get(item.key()) {
            if let Ok(mut part) = entry.try_lock() {
                while part.len() > 0 {
                    let _ = part.compact_batches_to_parquet(offsets.clone(), shared_output.clone()).await;
                }
            }
        }
    }
}

/// Force-flush all segments to WAL files regardless of thresholds.
pub async fn flush_all_segments(offsets_db: Arc<Offsets>) -> Result<(), ArrowError> {
    let mut bytes: u64 = 0;
    let mut rows: u64 = 0;
    let mut uploaded_bytes: u64 = 0;

    let (to_flush_batches, to_flush_offsets) = {
        let mut guard = GLOBAL_SEGMENT.lock().await;
        if guard.batches.is_empty() { return Ok(()); }
        let (batches, offsets, _bytes) = guard.take();
        (batches, offsets)
    };

    for ((namespace, partition, time, shard), batches) in to_flush_batches.into_iter() {
        if batches.is_empty() { continue; }
        let mut per_partition_offsets: HashMap<OffsetKey, u64> = HashMap::new();
        for (ok, pos) in to_flush_offsets.iter() { if ok.namespace == namespace && ok.partition == partition { per_partition_offsets.insert(ok.clone(), *pos); } }
        let mut wal_file = WalFile::new(&namespace, &partition, time, &shard, HashMap::new())?;
        let stat = wal_file.write_to_stream(&batches)?;
        wal_file.flush()?;
        wal_file.finish()?;
        uploaded_bytes += wal_file.bytes;
        bytes += stat.0;
        rows += stat.1;

        let partition_key: PartitionKey = (namespace.clone(), partition.clone(), time, shard.clone());
        match WAL_INDEX.entry(partition_key.clone()) {
            dashmap::mapref::entry::Entry::Occupied(mut occ) => {
                if let Ok(mut part) = occ.get_mut().try_lock() {
                    part.bytes = part.bytes.saturating_add(wal_file.bytes);
                    part.updated_at = SystemTime::now();
                    part.queue.push_back(WalEntry::Disk { wal: wal_file, offsets: per_partition_offsets.clone() });
                    if let Some(n) = PARTITION_NOTIFIES.get(&partition_key) { n.notify_waiters(); }
                }
            }
            dashmap::mapref::entry::Entry::Vacant(vac) => {
                let mut part = WalPartition {
                    queue: std::collections::VecDeque::new(),
                    bytes: wal_file.bytes,
                    updated_at: SystemTime::now(),
                    namespace: namespace.clone(),
                    partition: partition.clone(),
                    time,
                    shard: shard.clone(),
                };
                part.queue.push_back(WalEntry::Disk { wal: wal_file, offsets: per_partition_offsets.clone() });
                let arc = Arc::new(tokio::sync::Mutex::new(part));
                vac.insert(arc);
                PARTITION_NOTIFIES.insert(partition_key.clone(), Arc::new(tokio::sync::Notify::new()));
                if let Some(n) = PARTITION_NOTIFIES.get(&partition_key) { n.notify_waiters(); }
            }
        }
    }

    // Persist offsets Position atomically across the flushed segment
    for (offset, position) in to_flush_offsets.iter() {
        let offset_key = OffsetKey { namespace: offset.namespace.clone(), partition: offset.partition.clone() };
        offsets_db.insert(&offset_key, OffsetTypes::Position, *position);
    }

    metrics_hot::add_wal_write_bytes(bytes);
    metrics_hot::add_wal_write_rows(rows);
    WAL_BYTES_TOTAL.fetch_add(uploaded_bytes, std::sync::atomic::Ordering::Relaxed);

    Ok(())
}

#[derive(Clone)]
enum WalEntry {
    Disk { wal: WalFile, offsets: HashMap<OffsetKey, u64> },
}

impl WalEntry {
    fn bytes(&self) -> u64 { match self { WalEntry::Disk { wal, .. } => wal.bytes } }
    fn updated_at(&self) -> SystemTime { match self { WalEntry::Disk { wal, .. } => wal.updated_at } }
    fn offsets(&self) -> &HashMap<OffsetKey, u64> { match self { WalEntry::Disk { offsets, .. } => offsets } }
    fn from_disk(w: WalFile) -> Self { WalEntry::Disk { wal: w, offsets: HashMap::new() } }
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
            if let WalEntry::Disk { wal, .. } = e { hasher.update(wal.path.as_os_str().as_encoded_bytes()); hasher.update(&wal.bytes.to_le_bytes()); let ts = wal.updated_at.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_nanos(); hasher.update(&ts.to_le_bytes()); }
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
            for wf in segment_files.iter() { let WalEntry::Disk { wal, .. } = wf; if let Ok(s) = wal.clone().read_schema_from_stream() { schema_opt = Some(s); break; } }
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
                    WalEntry::Disk { wal: d, .. } => {
                        match OpenOptions::new().read(true).open(&d.path) {
                            Ok(file) => {
                                let mut reader = io::BufReader::new(file);
                            let mut offset_size = [0u8; 8];
                                if let Err(e) = reader.read_exact(&mut offset_size) { println!("ERROR: Failed to read WAL header for {}: {}", d.path.to_string_lossy(), e); continue; }
                            let skip = u64::from_le_bytes(offset_size);
                                if let Err(e) = reader.seek(io::SeekFrom::Current(skip as i64)) { println!("ERROR: Failed to seek WAL stream {}: {}", d.path.to_string_lossy(), e); continue; }
                                match StreamReader::try_new(reader, None) {
                                    Ok(sr) => {
                                        for item in sr { match item { Ok(batch) => {
                                            if !WalPartition::schemas_equivalent(&batch.schema(), &schema_clone) { println!("ERROR: Skipping WAL batch due to schema mismatch for ns={} part={} time={}", namespace, partition, time_val.unwrap_or(0)); batch_mismatch_counter.fetch_add(1, AtomicOrdering::Relaxed); continue; }
                                            row_counter_task.fetch_add(batch.num_rows() as u64, AtomicOrdering::Relaxed);
                                            batch_ok_counter.fetch_add(1, AtomicOrdering::Relaxed);
                                            if tx.send(Ok(batch)).await.is_err() { break; }
                                        }, Err(e) => { batch_error_counter.fetch_add(1, AtomicOrdering::Relaxed); let _ = tx.send(Err(DataFusionError::ArrowError(e, None))).await; break; } } }
                                    }
                                    Err(e) => { println!("Failed to init Arrow stream for local WAL {}: {}", d.path.to_string_lossy(), e); }
                                }
                            }
                            Err(e) => { println!("Failed to open WAL file {}: {}", d.path.to_string_lossy(), e); }
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
                    let WalEntry::Disk { offsets, .. } = wal_entry;
                    for (offset, position) in offsets.iter() {
                        let offset_key = OffsetKey { namespace: offset.namespace.clone(), partition: offset.partition.clone() };
                        _offsets_db.insert(&offset_key, OffsetTypes::Closed, *position);
                    }
                }

                        for wal_entry in segment_files.iter() {
                    let WalEntry::Disk { wal, .. } = wal_entry;
                        let tombstone_path = format!("{}/ingest_buffer/done/{}.tombstone", data_dir, Helpers::random_str(32));
                    if let Err(e) = fs::rename(&wal.path, tombstone_path) { println!("Failed to tombstone WAL file: {}, Error: {}", wal.path.to_string_lossy(), e); }
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

