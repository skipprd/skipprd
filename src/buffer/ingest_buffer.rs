use std::fs::{File, OpenOptions};
use std::{fs, io,};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::{BufReader, Read, Seek, Write};
use std::path::PathBuf;
use std::sync::{Arc};
use std::time::{SystemTime};
use arrow::array::{RecordBatch};
use arrow::json::ReaderBuilder;
use arrow_schema::{ArrowError, SchemaRef};
use glob::{glob_with, MatchOptions};
use std::sync::atomic::{AtomicU64};
use arrow::ipc::{CompressionType};
use arrow::ipc::reader::StreamReader;
use arrow::ipc::writer::{IpcWriteOptions, StreamWriter};
use bincode;
use datafusion::physical_plan::memory::MemoryStream;
use datafusion::physical_plan::SendableRecordBatchStream;
use datafusion::prelude::{ParquetReadOptions, SessionConfig, SessionContext};
use indexmap::IndexMap;
use once_cell::sync::Lazy;
use serde_derive::{Deserialize, Serialize};
use serde_json::{Value};
use crate::{METRICS};
use crate::metrics::counters as metrics_hot;
use crate::buffer::BufferChunker;
use crate::helpers::configuration::Config;
use crate::helpers::Helpers;
use crate::helpers::offsets::{OffsetKey, Offsets, OffsetTypes};
use crate::helpers::timed_rwlock::TimedRwLock;
use crate::ingest_work::{Deadletter, Ingest};
use crate::plugins::DataOutputPlugin;
use std::os::fd::AsRawFd;
use aws_sdk_s3::primitives::ByteStream as S3ByteStream;
use aws_sdk_s3::Client as S3Client;
use once_cell::sync::OnceCell;
use std::sync::atomic::AtomicBool;
use tokio::time::{sleep as tokio_sleep, Duration as TokioDuration};
use futures::stream::StreamExt as FuturesStreamExt;
use std::future::Future;
use std::pin::Pin;
use crate::ARROW_SCHEMA;
use dashmap::DashMap;
use std::sync::atomic::Ordering as AtomicOrdering;
use crate::buffer::wal_accumulator::PartitionKey;
use rand::{thread_rng, Rng};
use aws_sdk_s3::error::SdkError as S3SdkError;
use aws_sdk_s3::operation::get_object::{GetObjectError, GetObjectOutput};

#[allow(dead_code)]
pub static TOTAL_ROWS: Lazy<TimedRwLock<AtomicU64>> =
    Lazy::new(|| TimedRwLock::new("record_batch_total".to_string(), AtomicU64::new(0)));

// Lock-free WAL index and counters
pub static WAL_INDEX: Lazy<DashMap<PartitionKey, WalPartition>> = Lazy::new(|| DashMap::new());
pub static WAL_BYTES_TOTAL: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));
static COMPACTION_LOCK: OnceCell<std::sync::Mutex<()>> = OnceCell::new();
static COMPACTION_ASYNC_LOCK: Lazy<tokio::sync::Mutex<()>> = Lazy::new(|| tokio::sync::Mutex::new(()));
static FORCE_COMPACT_ONCE: Lazy<AtomicBool> = Lazy::new(|| AtomicBool::new(false));

// lazy_static! {
//     pub static ref WAL_INDEX: TimedRwLock<WalIndex> = TimedRwLock::new("wal_index".to_string(), WalIndex::new());
// }

/// Download an S3 object with exponential backoff and jitter.
/// - Retries transient failures up to a small cap
/// - Does not retry on NoSuchKey
async fn s3_get_object_with_backoff(
    client: &S3Client,
    bucket: &str,
    key: &str,
) -> Result<GetObjectOutput, GetObjectError> {
    let mut attempt: u32 = 0;
    let max_attempts: u32 = 6;
    loop {
        match client.get_object().bucket(bucket).key(key).send().await {
            Ok(resp) => return Ok(resp),
            Err(e) => {
                // Do not retry if the object truly doesn't exist
                let no_such_key = matches!(&e, S3SdkError::ServiceError(se) if se.err().is_no_such_key());
                if no_such_key { return Err(e.into_service_error()); }

                attempt += 1;
                if attempt >= max_attempts {
                    return Err(e.into_service_error());
                }
                // 200ms * 2^attempt with up to 100ms jitter, capped
                let base = 200u64.saturating_mul(1u64 << attempt.min(10));
                let jitter: u64 = thread_rng().gen_range(0..100);
                let sleep_ms = (base + jitter).min(5_000);
                println!(
                    "Retrying S3 get_object s3://{}/{} in {}ms (attempt {} of {})",
                    bucket, key, sleep_ms, attempt, max_attempts
                );
                tokio_sleep(TokioDuration::from_millis(sleep_ms)).await;
            }
        }
    }
}

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
    pub(crate) records: Vec<IngestRecord>,
    pub(crate) schema: SchemaRef,
    pub(crate) record_batches: Option<Vec<RecordBatch>>,
}

pub struct Buffers {
    buf: IndexMap<(String, String, Option<i64>, String), IngestBufferBatch>,
}

impl Buffers {

    pub fn new() -> Self {
        Buffers {
            buf: IndexMap::with_capacity(32), // Pre-allocate with a reasonable size
        }
    }


    pub fn write(&mut self, ingest_buffer_batch: HashMap<(String, String, Option<i64>, String), IngestBufferBatch>) {
        // Reserve capacity to avoid reallocations
        self.buf.reserve(ingest_buffer_batch.len());
        for batch in ingest_buffer_batch {
            self.buf.insert(batch.0, batch.1);
        }
    }

    pub async fn flush(&mut self, offsets_db: Arc<Offsets>, shared_output: Arc<Box<dyn DataOutputPlugin + Send + Sync>>) -> Result<(), ArrowError> {
    // pub async fn flush(&mut self, offsets_db: Arc<Offsets>, shared_output: Arc<TimedRwLock<Box<dyn DataOutputPlugin + Send + Sync>>>) -> Result<(), ArrowError> {
        
        let mut bytes: u64 = 0;
        let mut rows: u64 = 0;

        let mut force_compact = false;
        {
            // Check disk bytes via atomic counter vs disk threshold (not per-partition file size)
            let max_bytes = Config::get_pipeline_buffer_disk_threshold_bytes();
            let bytes = WAL_BYTES_TOTAL.load(std::sync::atomic::Ordering::Relaxed);
            if bytes > (max_bytes - (max_bytes as f64 * 0.1) as u64) {
                println!("Disk bytes {} of {} bytes, compacting all partitions", bytes, max_bytes);
                force_compact = true;
            }
        }

        // println!("Flushing {} WAL files", self.buf.len());

        let mut partitions: HashMap<(String, String, Option<i64>, String), Vec<WalFile>> = HashMap::with_capacity(self.buf.len());
        let wal_storage = Config::get_wal_storage();
        let wal_bucket = Config::get_wal_s3_bucket();
        let wal_prefix = Config::get_wal_s3_prefix().trim_matches('/').to_string();
        let data_dir_base = Config::get_data_dir();
        let wal_base = format!("{}/ingest_buffer", data_dir_base);
        let aws_conf_opt = if wal_storage.eq_ignore_ascii_case("s3") { Some(aws_config::defaults(aws_config::BehaviorVersion::latest()).load().await) } else { None };
        let s3_opt = aws_conf_opt.as_ref().map(|c| S3Client::new(c));
        let mut uploaded_bytes: u64 = 0;

        for ((namespace, partition, time, shard), ingest_buffer_batch) in self.buf.iter_mut() {
            if Config::log_wal_enabled() {
                println!("Buffers::flush: preparing partition ns={} part={} time={} shard={} records={} batches_prebuilt={}", namespace, partition, time.unwrap_or(0), shard, ingest_buffer_batch.records.len(), ingest_buffer_batch.record_batches.as_ref().map(|b| b.len()).unwrap_or(0));
            }

            // println!("Writing {} rows to WAL {} {} {} {}", ingest_buffer_batch.records.len(), namespace, partition, time.unwrap_or(0), shard);

            let partition_entry = partitions.entry((namespace.clone(), partition.clone(), time.clone(), shard.clone())).or_insert_with(|| Vec::with_capacity(1)); // Usually just one file per entry
            
            let arrow_schema = ingest_buffer_batch.schema.clone();

            let mut record_batches = if let Some(b) = ingest_buffer_batch.record_batches.clone() { b } else {
                let arrow_schema = ingest_buffer_batch.schema.clone();
                let mut decoder = ReaderBuilder::new(arrow_schema).build_decoder().unwrap();
                let mut record_batches = Vec::with_capacity(
                    (ingest_buffer_batch.records.len() / 1000).max(1)
                );
                let json_values = ingest_buffer_batch.records.iter().map(|record| &record.record).collect::<Vec<&Value>>();
                if Config::log_wal_enabled() { println!("Buffers::flush: serializing {} json values into Arrow batch", json_values.len()); }
                decoder.serialize(&json_values).unwrap();
                match decoder.flush() {
                    Ok(Some(batch)) => { record_batches.push(batch); },
                    _ => {}
                }
                record_batches
            };
            
            if record_batches.is_empty() {
                if Config::log_wal_enabled() { println!("Buffers::flush: no record batches produced for ns={} time={} shard={}, skipping write", namespace, time.unwrap_or(0), shard); }
                // Ensure we still update bytes counters, but nothing to write
                continue;
            }

            let mut wal_file = WalFile::new(
                namespace,
                partition,
                *time,
                shard,
                ingest_buffer_batch.offsets.clone(),
            )?;


            // println!("WAL File {} offset: {:?}", wal_file.path.to_str().unwrap(), ingest_buffer_batch.offset);
            
            let stat = wal_file.write_to_stream(&record_batches)?;
            if Config::log_wal_enabled() { println!("Buffers::flush: wrote WAL bytes={} rows={} to {}", stat.0, stat.1, wal_file.path.to_string_lossy()); }

            bytes += stat.0;
            rows += stat.1;

            // println!("Wrote records to WAL file: {}", wal_file.path.to_str().unwrap());

            wal_file.flush()?;

            wal_file.finish()?;

            // Upload WAL to S3 (or keep on disk) and commit offsets upon durable write
            let mut should_index = true;
            if wal_storage.eq_ignore_ascii_case("s3") {
                let rel = wal_file.path.to_string_lossy().replace(&wal_base, "").trim_start_matches('/').to_string();
                let key = if wal_prefix.is_empty() { rel.clone() } else { format!("{}/{}", wal_prefix, rel) };
                if let Some(s3) = &s3_opt {
                    match S3ByteStream::from_path(&wal_file.path).await {
                        Ok(body) => {
                            match s3.put_object().bucket(&wal_bucket).key(&key).body(body).send().await {
                                Ok(_) => {
                                    if Config::debug_enabled() || Config::log_wal_enabled() {
                                        println!("Uploaded WAL to s3://{}/{} (ns={},part={},time={},shard={})", wal_bucket, key, namespace, partition, time.unwrap_or(0), shard);
                                    }
                                    // commit offsets now
                                    wal_file.offsets.iter().for_each(|(offset, position)| {
                                        let offset_key = OffsetKey { namespace: offset.namespace.clone(), partition: offset.partition.clone() };
                                        offsets_db.insert(&offset_key, OffsetTypes::Position, *position);
                                        offsets_db.insert(&offset_key, OffsetTypes::Closed, 1);
                                    });
                                    uploaded_bytes += wal_file.bytes;
                                }
                                Err(e) => { println!("Failed to upload WAL {}: {}", key, e); should_index = false; }
                            }
                        }
                        Err(e) => { println!("Failed to stream WAL for upload {}: {}", wal_file.path.to_string_lossy(), e); should_index = false; }
                    }
                } else { should_index = false; }
            } else {
                // disk storage: commit offsets after local fsync+rename
                wal_file.offsets.iter().for_each(|(offset, position)| {
                    let offset_key = OffsetKey { namespace: offset.namespace.clone(), partition: offset.partition.clone() };
                    offsets_db.insert(&offset_key, OffsetTypes::Position, *position);
                    offsets_db.insert(&offset_key, OffsetTypes::Closed, 1);
                });
                uploaded_bytes += wal_file.bytes;
            }

            if should_index { partition_entry.push(wal_file); }

        }

        metrics_hot::add_wal_write_bytes(bytes);
        metrics_hot::add_wal_write_rows(rows);

        // Feedback actual bytes-per-row to the accumulator EWMA per drained partition
        if rows > 0 && bytes > 0 {
            for ((namespace, partition, time, shard), wal_files) in partitions.iter() {
                let part_key = (namespace.clone(), partition.clone(), time.clone(), shard.clone());
                // Sum actual bytes/rows for this partition in this flush cycle
                let part_bytes: u64 = wal_files.iter().map(|wf| wf.bytes).sum();
                // Use rows proportionally by file sizes; fallback to global rows if needed
                let part_rows: u64 = rows; // best-effort, batches were built from same records set
                crate::buffer::wal_accumulator::wal_feedback_bytes_per_row(&part_key, part_bytes, part_rows);
            }
        }

        // offsets_db.flush();

        // println!("Ingested {} rows of {} bytes to WAL", stats.1, stats.0);

        self.buf.clear();

        WAL_BYTES_TOTAL.fetch_add(uploaded_bytes, std::sync::atomic::Ordering::Relaxed);
        for ((namespace, partition, time, shard), wal_files) in partitions.into_iter() {
            if Config::debug_enabled() {
                println!(
                    "WAL flush partition ns={} part={} time={} shard={} files={}",
                    namespace,
                    partition,
                    time.unwrap_or(0),
                    shard,
                    wal_files.len()
                );
            }
            use dashmap::mapref::entry::Entry;
            match WAL_INDEX.entry((namespace.clone(), partition.clone(), time.clone(), shard.clone())) {
                Entry::Occupied(mut occ) => {
                    let wal_partition = occ.get_mut();
                    wal_files.iter().for_each(|wf| wal_partition.bytes += wf.bytes);
                    for wf in wal_files.into_iter() {
                        if wf.updated_at > wal_partition.updated_at { wal_partition.updated_at = wf.updated_at; }
                        wal_partition.files.push(wf);
                    }
                }
                Entry::Vacant(vac) => {
                    let mut part = WalPartition {
                        files: Vec::new(),
                        namespace: namespace.clone(),
                        partition: partition.clone(),
                        time: time.clone(),
                        shard: shard.clone(),
                        updated_at: SystemTime::now(),
                        bytes: 0,
                    };
                    for wf in wal_files.into_iter() {
                        if wf.updated_at > part.updated_at { part.updated_at = wf.updated_at; }
                        part.bytes += wf.bytes;
                        part.files.push(wf);
                    }
                    vac.insert(part);
                }
            }
        }

        // Start background compactor once
        Buffers::ensure_compactor_running(offsets_db.clone(), shared_output.clone());

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
    fn ensure_compactor_running(offsets_db: Arc<Offsets>, shared_output: Arc<Box<dyn DataOutputPlugin + Send + Sync>>) {
        static STARTED: OnceCell<()> = OnceCell::new();
        static OFFSETS_CELL: OnceCell<Arc<Offsets>> = OnceCell::new();
        static OUTPUT_CELL: OnceCell<Arc<Box<dyn DataOutputPlugin + Send + Sync>>> = OnceCell::new();
        let _ = OFFSETS_CELL.set(offsets_db);
        let _ = OUTPUT_CELL.set(shared_output);
        if STARTED.set(()).is_ok() {
            tokio::spawn(async move {
                loop {
                    let _guard = COMPACTION_ASYNC_LOCK.lock().await;
                    let mut to_compact: Vec<( (String, String, Option<i64>, String), WalPartition)> = Vec::new();
                    let force_once = FORCE_COMPACT_ONCE.swap(false, AtomicOrdering::Relaxed);
                    for item in WAL_INDEX.iter() {
                        let k = item.key().clone();
                        let v = item.value().clone();
                        if v.check_wal_rotate(force_once) { to_compact.push((k, v)); }
                    }
                    if !to_compact.is_empty() {
                        // remove from index to avoid double work
                        for (k, _) in to_compact.iter() { WAL_INDEX.remove(k); }
                        let tuned = crate::metrics::counters::WAL_COMPACTION_CONCURRENCY_TARGET.load(std::sync::atomic::Ordering::Relaxed).clamp(1, 64);
                        let mut in_flight: futures::stream::FuturesUnordered<Pin<Box<dyn Future<Output = u64> + Send>>> = futures::stream::FuturesUnordered::new();
                        let mut iter = to_compact.into_iter();
                        let offsets_cell = OFFSETS_CELL.get().unwrap().clone();
                        let output_cloned = OUTPUT_CELL.get().unwrap().clone();
                        for _ in 0..tuned {
                            if let Some((_k, p)) = iter.next() {
                                let mut wal = p;
                                let offsets_cloned = offsets_cell.clone();
                                let output_cloned = output_cloned.clone();
                                in_flight.push(Box::pin(async move { wal.compact_batches_to_parquet(offsets_cloned, output_cloned).await }));
                            }
                        }
                        let mut compacted_bytes: u64 = 0;
                        while let Some(bytes_done) = in_flight.next().await {
                            compacted_bytes += bytes_done as u64;
                            if let Some((_k, p)) = iter.next() {
                                let mut wal = p;
                                let offsets_cloned = OFFSETS_CELL.get().unwrap().clone();
                                let output_cloned = OUTPUT_CELL.get().unwrap().clone();
                                in_flight.push(Box::pin(async move { wal.compact_batches_to_parquet(offsets_cloned, output_cloned).await }));
                            }
                        }
                        WAL_BYTES_TOTAL.fetch_update(std::sync::atomic::Ordering::Relaxed, std::sync::atomic::Ordering::Relaxed, |v| Some(v.saturating_sub(compacted_bytes))).ok();
                    }
                    tokio_sleep(TokioDuration::from_millis(500)).await;
                }
            });
        }
    }

    pub async fn compact_all_partitions(force: bool, offsets_db: Arc<Offsets>, shared_output: Arc<Box<dyn DataOutputPlugin + Send + Sync>>) {

        // Repeat a small number of times to tolerate eventual S3 listings and late arrivals
        for _attempt in 0..3 {
            // Step 1: Single-flight compaction: rebuild the index and extract partitions
            let _guard = COMPACTION_ASYNC_LOCK.lock().await;
            wal_recover(offsets_db.clone()).expect("Failed to recover WAL index");
            let partitions_to_compact: Vec<WalPartition> = {
                let mut parts = Vec::new();
                for item in WAL_INDEX.iter() {
                    if force || item.value().check_wal_rotate(false) { parts.push(item.value().clone()); }
                }
                // Drop index to avoid double work
                WAL_INDEX.clear();
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
                    let offsets_db_clone = offsets_db.clone();
                    let shared_output2 = shared_output.clone();
                    in_flight.push(Box::pin(async move { p.compact_batches_to_parquet(offsets_db_clone, shared_output2).await }));
                }
            }

            let mut total_compacted_bytes: u64 = 0;
            while let Some(bytes_done) = in_flight.next().await {
                total_compacted_bytes += bytes_done as u64;
                if let Some(mut p) = iter.next() {
                    let offsets_db_clone = offsets_db.clone();
                    let shared_output2 = shared_output.clone();
                    in_flight.push(Box::pin(async move { p.compact_batches_to_parquet(offsets_db_clone, shared_output2).await }));
                }
            }

            // Step 3: Reduce WAL bytes counter
            WAL_BYTES_TOTAL.fetch_update(std::sync::atomic::Ordering::Relaxed, std::sync::atomic::Ordering::Relaxed, |v| Some(v.saturating_sub(total_compacted_bytes))).ok();
        }
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

pub fn wal_recover(offsets_db: Arc<Offsets>) -> io::Result<()> {
    // Delegate based on backend
    if Config::get_wal_storage().eq_ignore_ascii_case("s3") {
        return Buffers::wal_recover_s3(offsets_db);
    }
    Buffers::wal_recover_disk(offsets_db)
}

impl Buffers {
pub fn wal_recover_disk(offsets_db: Arc<Offsets>) -> io::Result<()> {
        let started = std::time::Instant::now();
        let mut count = 0;
        let mut bytes = 0;

        println!("Indexing WAL");

        let wal_files = Self::list_wal_files()?;

        let wal_files_count = wal_files.len();

        if wal_files_count == 0 {
            println!("Indexed {} of {} WAL files", count, wal_files_count);
            FORCE_COMPACT_ONCE.store(true, AtomicOrdering::Relaxed);
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
                    let part = occ.get_mut();
                    if part.updated_at < wal_file.updated_at { part.updated_at = wal_file.updated_at; }
                    part.bytes += wal_file.bytes;
                    part.files.push(wal_file.clone());
                }
                Entry::Vacant(vac) => {
                    let mut part = WalPartition {
                        files: Vec::new(),
                        namespace: partition_key.0.clone(),
                        partition: partition_key.1.clone(),
                        time: partition_key.2,
                        shard: partition_key.3.clone(),
                        updated_at: SystemTime::UNIX_EPOCH,
                        bytes: 0,
                    };
                    if part.updated_at < wal_file.updated_at { part.updated_at = wal_file.updated_at; }
                    part.bytes += wal_file.bytes;
                    part.files.push(wal_file.clone());
                    vac.insert(part);
                }
            }

            WAL_BYTES_TOTAL.fetch_add(wal_file.bytes, AtomicOrdering::Relaxed);

            if count % 1000 == 0 {
                println!("Indexed {} of {} WAL files for {} namespaces in {} partitions", count, wal_files_count, namespaces.len(), WAL_INDEX.len());
                FORCE_COMPACT_ONCE.store(true, AtomicOrdering::Relaxed);
            }
        }

        println!("Indexed {} of {} WAL files for {} namespaces in {} partitions", count, wal_files_count, namespaces.len(), WAL_INDEX.len());
        FORCE_COMPACT_ONCE.store(true, AtomicOrdering::Relaxed);
        let elapsed = started.elapsed().as_secs_f64();
        if elapsed > 0.0 {
            let rate = (count as f64 / elapsed) as u64;
            println!("WAL indexing took {:.2}s ~ {} files/s, {} total bytes", elapsed, rate, Helpers::human_readable_size(bytes as u64));
        }

        let mut wal_index_metrics: WalIndexMetrics = WalIndexMetrics { metrics: Vec::new() };

        for (namespace, partition_key) in namespace_partitions {
            let human_bytes = Helpers::human_readable_size(namespace_partition_bytes.iter().filter(|(k, _v)| k.0 == namespace).map(|(_k, v)| v).sum::<u64>());
            println!("Namespace {} contains {} partitions and {} files of {}", namespace, partition_key.len(), namespace_partition_files.iter().filter(|(k, _v)| k.0 == namespace).map(|(_k, v)| v).sum::<u64>(), human_bytes);

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

        println!("Syncing offsets to DB");

        for item in WAL_INDEX.iter() {
            let wal_partition = &mut item.value().clone();
            // ensure offsets committed
            for wal_file in wal_partition.files.iter_mut() {
                wal_file.offsets.iter().for_each(|(offset, position)| {
                    let offset_key = OffsetKey { namespace: offset.namespace.clone(), partition: offset.partition.clone() };
                    offsets_db.insert(&offset_key, OffsetTypes::Position, *position);
                    offsets_db.insert(&offset_key, OffsetTypes::Closed, 1);
                });
            }
        }

        offsets_db.flush();

        Ok(())
    }

    /// Blocking S3 WAL index recovery used for end-of-ingest compaction
    pub fn wal_recover_s3(_offsets_db: Arc<Offsets>) -> io::Result<()> {
        let started = std::time::Instant::now();
        if Config::get_wal_storage().eq_ignore_ascii_case("s3") {
            println!("Indexing WAL from S3");
            let wal_bucket = Config::get_wal_s3_bucket();
            let wal_prefix = Config::get_wal_s3_prefix().trim_matches('/').to_string();
            let prefixes = vec![format!("{}/", wal_prefix), wal_prefix.clone()];

            // Collect entries in async context, then apply to self outside without taking global locks
            let (tx, rx) = std::sync::mpsc::channel::<(Vec<(PartitionKey, WalFile)>, HashSet<String>, u64, u64)>();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                rt.block_on(async move {
                    let started = std::time::Instant::now();
                    let aws_conf = aws_config::defaults(aws_config::BehaviorVersion::latest()).load().await;
                    let s3 = S3Client::new(&aws_conf);
                    let mut files_indexed: u64 = 0;
                    let mut total_bytes: u64 = 0;
                    let mut namespaces: HashSet<String> = HashSet::new();
                    let mut entries: Vec<(PartitionKey, WalFile)> = Vec::new();
                    for pfx in prefixes.iter() {
                        let mut page_count: u64 = 0;
                        let mut paginator = s3.list_objects_v2().bucket(&wal_bucket).prefix(pfx).into_paginator().send();
                        while let Some(page) = paginator.next().await {
                            match page {
                                Ok(resp) => {
                                    page_count += 1;
                                    for obj in resp.contents() {
                                        if let (Some(key), Some(size)) = (obj.key(), obj.size()) {
                                            if !key.ends_with(".wal") { continue; }
                                            let remainder = key.strip_prefix(&format!("{}/", wal_prefix)).unwrap_or(key);
                                            let mut parts = remainder.split('/');
                                            let shard_dir = match parts.next() { Some(s) => s, None => "" };
                                            let filename = match parts.next() { Some(f) => f, None => "" };
                                            if filename.is_empty() { continue; }
                                            let namespace = BufferChunker::decode_file_namespace(filename);
                                            let partition = BufferChunker::decode_file_partition(filename);
                                            let time_val = BufferChunker::decode_file_time(filename);
                                            let time_opt = if time_val > 0 { Some(time_val) } else { None };
                                            let shard = BufferChunker::decode_file_shard(filename);
                                            let data_dir = Config::get_data_dir();
                                            let base = format!("{}/ingest_buffer", data_dir);
                                            let path = PathBuf::from(format!("{}/{}/{}", base, shard_dir, filename));
                                            let wal_file = WalFile {
                                                path,
                                                namespace: namespace.clone(),
                                                partition: partition.clone(),
                                                time: time_opt,
                                                shard: shard.clone(),
                                                bytes: size as u64,
                                                file: Arc::new(TimedRwLock::new("wal_file".to_string(), None)),
                                                updated_at: obj.last_modified().map(|dt| SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(dt.secs() as u64)).unwrap_or(SystemTime::now()),
                                                offsets: HashMap::new(),
                                            };
                                            entries.push(((namespace.clone(), partition.clone(), time_opt, shard.clone()), wal_file));
                                            namespaces.insert(namespace);
                                            total_bytes += size as u64;
                                            files_indexed += 1;
                                        }
                                    }
                                }
                                Err(_e) => { break; }
                            }
                        }
                        if files_indexed > 0 { break; }
                    }
                    let elapsed = started.elapsed().as_secs_f64();
                    println!("Indexed {} WAL files on S3 in {:.2}s (blocking), {} total bytes", files_indexed, elapsed, Helpers::human_readable_size(total_bytes));
                    let _ = tx.send((entries, namespaces, total_bytes, files_indexed));
                });
            }).join().unwrap();
            let (entries, namespaces, total_bytes, _files_indexed) = rx.recv().unwrap();

            // Apply results to this index without taking global WAL_PARTITION_INDEX locks
            WAL_INDEX.clear();
            WAL_BYTES_TOTAL.store(0, AtomicOrdering::Relaxed);
            for (partition_key, wal_file) in entries.into_iter() {
                use dashmap::mapref::entry::Entry;
                match WAL_INDEX.entry(partition_key.clone()) {
                    Entry::Occupied(mut occ) => {
                        let part = occ.get_mut();
                        if part.updated_at < wal_file.updated_at { part.updated_at = wal_file.updated_at; }
                        part.bytes += wal_file.bytes;
                        part.files.push(wal_file.clone());
                    }
                    Entry::Vacant(vac) => {
                        let mut part = WalPartition {
                            files: Vec::new(),
                            namespace: partition_key.0.clone(),
                            partition: partition_key.1.clone(),
                            time: partition_key.2,
                            shard: partition_key.3.clone(),
                            updated_at: SystemTime::UNIX_EPOCH,
                            bytes: 0,
                        };
                        if part.updated_at < wal_file.updated_at { part.updated_at = wal_file.updated_at; }
                        part.bytes += wal_file.bytes;
                        part.files.push(wal_file.clone());
                        vac.insert(part);
                    }
                }
                WAL_BYTES_TOTAL.fetch_add(wal_file.bytes, AtomicOrdering::Relaxed);
            }
            let mut metrics = METRICS.write();
            metrics.wal_index_namespaces_total = namespaces.len() as u64;
            metrics.wal_index_partitions_total = WAL_INDEX.len() as u64;
            metrics.wal_index_files_total = WAL_INDEX.iter().map(|p| p.value().files.len() as u64).sum();
            metrics.wal_index_bytes_total = WAL_BYTES_TOTAL.load(AtomicOrdering::Relaxed) as u64;
            return Ok(());
        }
        // Disk mode: fall back to normal recover
        Self::wal_recover_disk(_offsets_db)
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
                println!("Removing empty WAL file: {}", path.to_str().unwrap());
                fs::remove_file(&path).unwrap();
            }

            // remove .tmp files
            if path.is_file() && path.extension().and_then(OsStr::to_str) == Some("tmp") {
                println!("Removing temp WAL file: {}", path.to_str().unwrap());
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

#[derive(Clone)]
struct WalPartition {
    files: Vec<WalFile>,
    pub(crate) namespace: String,
    pub(crate) partition: String,
    pub(crate) time: Option<i64>,
    pub(crate) shard: String,
    updated_at: SystemTime,
    bytes: u64
}

impl WalPartition {
    fn prune_tombstone_wals(&mut self) {
        // println!("Purging tombstone WAL files");
        let data_dir = Config::get_data_dir();
        let wal_dir = PathBuf::from(format!("{}/ingest_buffer/done", data_dir));
        fs::remove_dir_all(&wal_dir).unwrap_or_default();
        fs::create_dir_all(&wal_dir).unwrap_or_default()
    }

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
            println!("Compacting WAL partition Namespace: {}, Partition: {}, Time: {}, of Bytes: {}, Elapsed Secs: {}, Segment Files: {}", self.namespace, self.partition, self.time.unwrap_or(0), self.bytes, elapsed, self.files.len());
            return true
        }

        false
    }

    async fn compact_batches_to_parquet(&mut self, offsets_db: Arc<Offsets>, shared_output: Arc<Box<dyn DataOutputPlugin + Send + Sync>>) -> u64 {
    // async fn compact_batches_to_parquet(&mut self, offsets_db: Arc<Offsets>, shared_output: Arc<TimedRwLock<Box<dyn DataOutputPlugin + Send + Sync>>>) {
        let data_dir = Config::get_data_dir();
        let mut output_file_name = BufferChunker::encode_chunk_name(
            "output",
            Some(&self.namespace),
            Some(&self.partition),
            self.time,
            Some(&self.shard)
        );

        // NOTE: offsets are committed AFTER successful upload now (moved below)
        
        // Deterministic output name for idempotent compaction uploads: hash of WAL file signatures
        // This ensures repeated compactions of the same partition overwrite the same object, avoiding duplicates
        let mut sigs: Vec<String> = self.files.iter().map(|wf| {
            let ts = wf.updated_at.duration_since(SystemTime::UNIX_EPOCH).unwrap_or(std::time::Duration::from_secs(0)).as_secs();
            format!("{}:{}:{}", wf.path.to_string_lossy(), wf.bytes, ts)
        }).collect();
        sigs.sort();
        let joined = sigs.join("|");
        let stable_hash = format!("{:x}", md5::compute(joined));
        output_file_name = format!("{}-{}", output_file_name, stable_hash);

        // println!("Compacting WAL partition to Parquet, Namespace: {} Partition: {} {}", self.namespace, self.partition, self.time.unwrap_or(0));

        let mut wal_compacted_bytes_total = 0;
        let mut wal_compacted_rows_total = 0;
        let mut wal_compacted_files_total = 0;
       
        let first_file_opt = self.files.first_mut();
        if first_file_opt.is_none() {
                println!("No WAL files to compact for partition: {} {}", self.namespace, self.partition);
                return wal_compacted_bytes_total;
            }

        let wal_bucket = Config::get_wal_s3_bucket();
        let wal_prefix = Config::get_wal_s3_prefix().trim_matches('/').to_string();
        let data_dir = Config::get_data_dir();
        let base = format!("{}/ingest_buffer", data_dir);
        let storage = Config::get_wal_storage();
        let s3 = if storage.eq_ignore_ascii_case("s3") {
            let aws_conf = aws_config::defaults(aws_config::BehaviorVersion::latest()).load().await;
            Some(S3Client::new(&aws_conf))
        } else { None };

        // Determine schema from the first WAL file in this shard to avoid type mismatches
        let schema: SchemaRef = if storage.eq_ignore_ascii_case("disk") {
            match self.files.first_mut().and_then(|wf| wf.read_schema_from_stream().ok()) {
                Some(s) => s,
                None => {
                    println!("Failed to read schema from local WAL for namespace: {}", self.namespace);
                    return wal_compacted_bytes_total;
                }
            }
        } else {
            // s3: read schema from first object
            if let (Some(s3c), Some(first)) = (s3.as_ref(), self.files.first()) {
                let rel = first.path.to_string_lossy().replace(&base, "").trim_start_matches('/').to_string();
                let key = if wal_prefix.is_empty() { rel.clone() } else { format!("{}/{}", wal_prefix, rel) };
                match s3_get_object_with_backoff(s3c, &wal_bucket, &key).await {
                    Ok(resp) => {
                        let bytes = resp.body.collect().await.unwrap().into_bytes();
                        use std::io::{Cursor, Seek};
                        let mut cursor = Cursor::new(bytes);
                        let mut offset_size = [0u8; 8];
                        if std::io::Read::read_exact(&mut cursor, &mut offset_size).is_err() { println!("Failed to read WAL offset header from S3 {}", key); return wal_compacted_bytes_total; }
                        let skip = u64::from_le_bytes(offset_size);
                        let _ = std::io::Seek::seek(&mut cursor, io::SeekFrom::Current(skip as i64));
                        match StreamReader::try_new(cursor, None) {
                            Ok(sr) => sr.schema(),
                            Err(e) => { println!("Failed to init Arrow stream to read schema from S3 WAL {}: {}", key, e); return wal_compacted_bytes_total; }
                        }
                    }
                    Err(e) => { println!("Failed to download WAL from S3 {}: {}", key, e); return wal_compacted_bytes_total; }
                }
            } else {
                println!("WAL_STORAGE is 's3' but missing client or files");
                return wal_compacted_bytes_total;
            }
        };


        // Read from disk when WAL_STORAGE=disk; read from S3 when WAL_STORAGE=s3
        let mut batches: Vec<RecordBatch> = Vec::new();
        for wal_file in self.files.iter_mut() {
            wal_compacted_bytes_total += wal_file.bytes;
            wal_compacted_files_total += 1;
            if storage.eq_ignore_ascii_case("disk") {
                match wal_file.read_from_stream() {
                    Ok(mut local_batches) => {
                        // Enforce schema compatibility; skip mismatched batches defensively
                        let mut kept: Vec<RecordBatch> = Vec::with_capacity(local_batches.len());
                        for b in local_batches.drain(..) {
                            if b.schema().as_ref() != schema.as_ref() {
                                println!("Skipping WAL batch due to schema mismatch for ns={} part={} time={}", self.namespace, self.partition, self.time.unwrap_or(0));
                                continue;
                            }
                            wal_compacted_rows_total += b.num_rows() as u64;
                            kept.push(b);
                        }
                        batches.append(&mut kept);
                    }
                    Err(e) => { println!("Failed reading local WAL {}: {}", wal_file.path.to_string_lossy(), e); }
                }
            } else {
                if let Some(s3c) = &s3 {
                    let rel = wal_file.path.to_string_lossy().replace(&base, "").trim_start_matches('/').to_string();
                    let key = if wal_prefix.is_empty() { rel.clone() } else { format!("{}/{}", wal_prefix, rel) };
                    match s3_get_object_with_backoff(s3c, &wal_bucket, &key).await {
                        Ok(resp) => {
                            let bytes = resp.body.collect().await.unwrap().into_bytes();
                            use std::io::{Cursor, Seek};
                            let mut cursor = Cursor::new(bytes);
                            let mut offset_size = [0u8; 8];
                            // Validate header presence
                            if std::io::Read::read_exact(&mut cursor, &mut offset_size).is_err() {
                                println!("Failed to read WAL offset header from S3 {}", key);
                                if Config::truth_value(&Config::getenv("DELETE_CORRUPT_WAL", "false")) {
                                    let _ = s3c.delete_object().bucket(&wal_bucket).key(&key).send().await;
                                }
                                continue;
                            }
                            let skip = u64::from_le_bytes(offset_size);
                            // Validate we have at least skip bytes available
                            let remaining = cursor.get_ref().len() as i64 - cursor.position() as i64;
                            if remaining < skip as i64 {
                                println!("Invalid WAL header for S3 {}: skip {} exceeds remaining {} bytes", key, skip, remaining);
                                if Config::truth_value(&Config::getenv("DELETE_CORRUPT_WAL", "false")) {
                                    let _ = s3c.delete_object().bucket(&wal_bucket).key(&key).send().await;
                                }
                                continue;
                            }
                            let _ = std::io::Seek::seek(&mut cursor, io::SeekFrom::Current(skip as i64));
                            match StreamReader::try_new(cursor, None) {
                                Ok(sr) => {
                                    for batch_res in sr {
                                        match batch_res {
                                            Ok(batch) => {
                                                if batch.schema().as_ref() != schema.as_ref() {
                                                    println!("Skipping WAL batch from S3 due to schema mismatch for ns={} part={} time={}", self.namespace, self.partition, self.time.unwrap_or(0));
                                                    continue;
                                                }
                                                wal_compacted_rows_total += batch.num_rows() as u64; batches.push(batch);
                                            },
                                            Err(e) => {
                                                println!("Failed reading batch from S3 WAL {}: {}", key, e);
                                                if Config::truth_value(&Config::getenv("DELETE_CORRUPT_WAL", "false")) {
                                                    let _ = s3c.delete_object().bucket(&wal_bucket).key(&key).send().await;
                                                }
                                                break;
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    println!("Failed to init Arrow stream from S3 WAL {}: {}", key, e);
                                    if Config::truth_value(&Config::getenv("DELETE_CORRUPT_WAL", "false")) {
                                        let _ = s3c.delete_object().bucket(&wal_bucket).key(&key).send().await;
                                    }
                                }
                            }
                        }
                        Err(e) => { println!("Failed to download WAL from S3 {}: {}", key, e); }
                    }
                } else {
                    println!("WAL_STORAGE is 's3' but S3 client not initialized; skipping file");
                }
            }
        }

        let batch_stream: SendableRecordBatchStream = Box::pin(MemoryStream::try_new(batches, schema.clone(), None).unwrap());


        // (No re-upload here; WALs were uploaded earlier in flush prior to offset commit.)

        match shared_output.sync(batch_stream, output_file_name).await {
        // match shared_output.write().sync(batch_stream, output_file_name).await {
            Ok(()) => {
                // println!("Synced WAL partition to output: {} {}", self.namespace, self.partition);
        
                metrics_hot::add_wal_compacted_bytes(wal_compacted_bytes_total);
                metrics_hot::add_wal_compacted_rows(wal_compacted_rows_total);
                metrics_hot::add_wal_compacted_files(wal_compacted_files_total);



                // Delete S3 WALs when in S3 mode, and tombstone local files to free disk
                if storage.eq_ignore_ascii_case("s3") {
                    if let Some(s3c) = &s3 {
                        for wal_file in self.files.iter() {
                            let rel = wal_file.path.to_string_lossy().replace(&base, "").trim_start_matches('/').to_string();
                            let key = if wal_prefix.is_empty() { rel.clone() } else { format!("{}/{}", wal_prefix, rel) };
                            let mut retries = 0u32;
                            loop {
                                match s3c.delete_object().bucket(&wal_bucket).key(&key).send().await {
                                    Ok(_) => break,
                        Err(e) => {
                                        retries += 1;
                                        if retries <= 5 {
                                            let delay = 1u64 << retries; // 2,4,8,16,32s
                                            println!("Retrying delete of S3 WAL {} in {}s (attempt {}): {:?}", key, delay, retries, e);
                                            tokio_sleep(TokioDuration::from_secs(delay)).await;
                                        } else {
                                            println!("Failed to delete S3 WAL {} after {} retries: {:?}", key, retries, e);
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // In disk mode we tombstone local files; in S3 mode skip local tombstoning
                if storage.eq_ignore_ascii_case("disk") {
                    for wal_file in self.files.iter() {
                        let tombstone_path = format!("{}/ingest_buffer/done/{}.tombstone", data_dir, Helpers::random_str(32));
                        if let Err(e) = fs::rename(&wal_file.path, tombstone_path) {
                            println!("Failed to tombstone WAL file: {}, Error: {}", wal_file.path.to_string_lossy(), e);
                        }
                    }
                    self.prune_tombstone_wals();
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
        let codec = Some(CompressionType::LZ4_FRAME);
        let options = IpcWriteOptions::default().try_with_compression(codec)?;

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
