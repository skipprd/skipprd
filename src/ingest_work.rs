use crate::buffer::BufferChunker;
use crate::discover::{
    AnalyseSchema, Metadata, OutputMetadata, PipelineMetadata, NUM_ANALYSED_RECORDS,
};
use crate::helpers::configuration::Config;
use crate::helpers::offsets::{OffsetKey, OffsetTypes, OffsetValue, Offsets};
use crate::helpers::{Helpers, IngestTransformSnapshot};
use crate::ingest::ingest::ingest;
use crate::runtime_plugins::protocol::{RuntimeExecutionMode, RuntimeRawIngestBatch};
use crate::runtime_plugins::schema_state::bump_pipeline_schema_version;
use crate::serdes::decode::decode_records;
use crate::serdes::input_format::InputFormat;
use crate::serdes::ndjson_fast::NdjsonLineParser;
use crate::{
    data_dir_ingest_paused, record_data_dir_capacity_error, set_data_dir_ingest_paused,
    ARROW_SCHEMA, ARROW_SCHEMA_VERSION, METADATA, RUNNING,
};
use dashmap::DashMap;
use std::sync::atomic::AtomicBool;
// Per-namespace schema readiness flag to eliminate first-batch races
static SCHEMA_READY: once_cell::sync::Lazy<DashMap<String, AtomicBool>> =
    once_cell::sync::Lazy::new(|| DashMap::new());
static SCHEMA_PREP_LOCKS: once_cell::sync::Lazy<DashMap<String, Arc<std::sync::Mutex<()>>>> =
    once_cell::sync::Lazy::new(|| DashMap::new());
// Single-flight guard for metadata evolution per namespace
static EVOLUTION_LOCKS: once_cell::sync::Lazy<DashMap<String, Arc<tokio::sync::Mutex<()>>>> =
    once_cell::sync::Lazy::new(|| DashMap::new());
static DISCOVERY_COMPLETE: once_cell::sync::Lazy<AtomicBool> =
    once_cell::sync::Lazy::new(|| AtomicBool::new(false));
static DATA_DIR_INGEST_PAUSE_LAST_LOG_SECS: once_cell::sync::Lazy<AtomicU64> =
    once_cell::sync::Lazy::new(|| AtomicU64::new(0));
static TRANSFORM_INJECT_FIELDS: Lazy<HashMap<String, Value>> =
    Lazy::new(Config::get_transform_inject_fields);
use crate::metrics::counters as metrics_hot;
use crate::metrics::ingest_profile;
// Bounded concurrency for background metadata writes and Glue schema syncs
static METADATA_WRITE_SEM: once_cell::sync::Lazy<Arc<tokio::sync::Semaphore>> =
    once_cell::sync::Lazy::new(|| Arc::new(tokio::sync::Semaphore::new(2)));
pub(crate) static INGEST_RT: once_cell::sync::Lazy<runtime::Runtime> =
    once_cell::sync::Lazy::new(|| {
        runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .thread_name("skipprd-ingest-rt")
            .build()
            .expect("shared ingest runtime")
    });

use once_cell::sync::Lazy;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};

use std::io::BufRead as _;

use std::process::exit;
use std::string::ToString;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc::channel;
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};
use threadpool::ThreadPool;
extern crate num_cpus;
use std::sync::atomic::Ordering::AcqRel;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
// use dashmap::{DashMap};

use crate::buffer::ingest_buffer::{Buffers, IngestBufferBatch, IngestRecord};
use crate::discover::evolution::{infer_specs_for_record, EvolutionProposal};
use crate::ingest::exact_arrow::{
    append_row_to_exact_partition, resolve_cached_exact_plan, ExactArrowPartition, ExactArrowPlan,
};
use crate::ingest::fast_ingest::{
    create_default_nested_message, fast_path_ingest, DEFAULT_NESTED_MESSAGE,
};
use crate::ingest::record_types::{NormalizedRecord, SourceRecord};
// use crate::converters::skippr_avro::convert_skippr_to_avro_field_types;

use crate::converters::skippr_arrow::convert_skippr_to_arrow;
use crate::plugins::DataSink;
use arrow::compute::concat_batches;
use arrow::datatypes;
use arrow::error::ArrowError;
use arrow::json::ReaderBuilder as ArrowJsonReaderBuilder;
use arrow::record_batch::RecordBatch;
use arrow_schema::SchemaRef;
use tokio::runtime;
use tokio::sync::{mpsc, oneshot};
// wal_accumulator removed

// Single-threaded slow-ingest queue to serialize metadata evolution and value coercion
#[derive(Debug)]
struct SlowIngestTask {
    namespace: String,
    record: SourceRecord,
    flatten: bool,
    resp_tx: oneshot::Sender<Result<Value, String>>,
}

static SLOW_INGEST_TX: once_cell::sync::OnceCell<mpsc::Sender<SlowIngestTask>> =
    once_cell::sync::OnceCell::new();

/// Create an ingest thread pool, backing off thread count when the OS returns EAGAIN.
fn build_ingest_thread_pool(requested: usize) -> (ThreadPool, usize) {
    let requested = requested.max(1);
    let mut threads = requested;
    loop {
        match std::panic::catch_unwind(|| ThreadPool::new(threads)) {
            Ok(pool) => {
                if threads != requested {
                    warn!(
                        "Reduced ingest thread pool from {requested} to {threads} after thread spawn failure (EAGAIN)"
                    );
                }
                return (pool, threads);
            }
            Err(_) => {
                if threads <= 1 {
                    panic!(
                        "Failed to create ingest thread pool even with 1 thread (os error 11 / EAGAIN)"
                    );
                }
                threads = (threads / 2).max(1);
            }
        }
    }
}

fn ensure_slow_ingest_worker() {
    if SLOW_INGEST_TX.get().is_some() {
        return;
    }
    let (tx, mut rx) = mpsc::channel::<SlowIngestTask>(10_000);
    let _ = SLOW_INGEST_TX.set(tx);
    // Spawn single worker
    let worker = async move {
        while let Some(task) = rx.recv().await {
            // Serialize evolution: take per-namespace lock to reduce contention
            let ns_lock = EVOLUTION_LOCKS
                .entry(task.namespace.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone();
            let _guard = ns_lock.lock().await;
            // Evolve against global METADATA snapshot.  If the namespace
            // doesn't exist yet we create it here (single-threaded) so that
            // the read-then-write from multiple ingest threads can never
            // regress already-evolved metadata.
            let mut md_local = METADATA.load().as_ref().clone();
            if !md_local.metadata.contains_key(&task.namespace) {
                info!("Discovered new namespace: {}", task.namespace);
                md_local
                    .metadata
                    .insert(task.namespace.clone(), Metadata::new().unwrap());
            }
            let mut updated = "no".to_string();
            let ingest_result = if let Some(ns_meta) = md_local.metadata.get_mut(&task.namespace) {
                ingest(
                    task.record.inner(),
                    &mut ns_meta.fields,
                    &task.namespace,
                    &mut updated,
                    task.flatten,
                )
                .map_err(|e| e.to_string())
            } else {
                Err(format!("No metadata for namespace {}", task.namespace))
            };

            let result = match ingest_result {
                Ok(v) => {
                    if updated == "yes" {
                        // Monotonic schema evolution is an ingest-time contract:
                        // this per-namespace worker is the only place that mutates
                        // metadata for incoming records. Normalize field identity,
                        // publish metadata/Arrow templates, and wait for the schema
                        // sink before returning records to the write path.
                        match md_local.normalize_namespace(&task.namespace) {
                            Ok(_) => {
                                METADATA.store(Arc::new(md_local.clone()));
                                let _ = Ingest::prepare_arrow_schema_with_metadata(
                                    &task.namespace,
                                    &md_local.metadata,
                                    task.flatten,
                                );

                                if let Err(err) =
                                    Config::sync_output_schema_namespace_blocking(&task.namespace)
                                        .await
                                {
                                    Err(err)
                                } else {
                                    if let Ok(handle) = tokio::runtime::Handle::try_current() {
                                        let md_clone = md_local.clone();
                                        let sem = METADATA_WRITE_SEM.clone();
                                        handle.spawn(async move {
                                            let _permit = sem.acquire().await;
                                            Config::set_metadata(&md_clone, false).await;
                                        });
                                    } else {
                                        error!(
                                            "No tokio runtime to persist metadata for namespace {}",
                                            task.namespace
                                        );
                                    }
                                    Ok(v)
                                }
                            }
                            Err(err) => Err(err),
                        }
                    } else {
                        Ok(v)
                    }
                }
                Err(err) => Err(err),
            };
            let _ = task.resp_tx.send(result);
        }
    };
    let handle = runtime::Handle::try_current()
        .expect("slow_ingest_worker must be started inside a tokio runtime");
    handle.spawn(worker);
}

fn merge_inject_fields(record: &mut Value, fields: &HashMap<String, Value>) {
    if fields.is_empty() {
        return;
    }
    if let Some(obj) = record.as_object_mut() {
        for (key, value) in fields {
            let insert = match obj.get(key) {
                None => true,
                Some(existing) => {
                    existing.is_null() || matches!(existing.as_str(), Some(s) if s.is_empty())
                }
            };
            if insert {
                obj.insert(key.clone(), value.clone());
            }
        }
    }
}

fn apply_transform_inject_fields(record: &mut Value) {
    if TRANSFORM_INJECT_FIELDS.is_empty() {
        return;
    }
    merge_inject_fields(record, &*TRANSFORM_INJECT_FIELDS);
}

const EMPTY_PARTITION: &str = "";

fn ndjson_object_stream_eligible(
    format: InputFormat,
    payload: &str,
    entity_field_dot: &str,
    is_cdc_batch: bool,
) -> bool {
    format == InputFormat::Json
        && !Config::get_enable_single_quote_parsing()
        && !Config::get_enable_unicode_parsing()
        && entity_field_dot.is_empty()
        && !is_cdc_batch
        && {
            let trimmed = payload.trim();
            trimmed.contains('\n') && !trimmed.starts_with('[')
        }
}

enum IngestFlattenItem {
    Row {
        line: u64,
        value: Value,
    },
    NotObject {
        line: String,
        offset_pos: u64,
    },
    ParseError {
        line: String,
        offset_pos: u64,
        error: String,
    },
}

struct IngestFlattenIter<'a> {
    source: IngestFlattenSource<'a>,
    logical_line: u64,
    queue: VecDeque<Value>,
    ndjson_parser: NdjsonLineParser,
}

enum IngestFlattenSource<'a> {
    Buffered(std::vec::IntoIter<Value>),
    Ndjson(std::str::Lines<'a>),
}

impl<'a> IngestFlattenIter<'a> {
    fn from_decoded(records: Vec<Value>) -> Self {
        Self {
            source: IngestFlattenSource::Buffered(records.into_iter()),
            logical_line: 0,
            queue: VecDeque::new(),
            ndjson_parser: NdjsonLineParser::new(),
        }
    }

    fn from_ndjson(payload: &'a str) -> Self {
        Self {
            source: IngestFlattenSource::Ndjson(payload.lines()),
            logical_line: 0,
            queue: VecDeque::new(),
            ndjson_parser: NdjsonLineParser::new(),
        }
    }
}

impl Iterator for IngestFlattenIter<'_> {
    type Item = IngestFlattenItem;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(value) = self.queue.pop_front() {
                self.logical_line += 1;
                return Some(IngestFlattenItem::Row {
                    line: self.logical_line,
                    value,
                });
            }

            match &mut self.source {
                IngestFlattenSource::Buffered(iter) => {
                    let record = iter.next()?;
                    self.logical_line += 1;
                    match record {
                        Value::Object(_) => {
                            return Some(IngestFlattenItem::Row {
                                line: self.logical_line,
                                value: record,
                            });
                        }
                        Value::Array(values) => {
                            self.queue.extend(values);
                            self.logical_line -= 1;
                        }
                        _ => {
                            return Some(IngestFlattenItem::NotObject {
                                line: record.to_string(),
                                offset_pos: self.logical_line,
                            });
                        }
                    }
                }
                IngestFlattenSource::Ndjson(lines) => {
                    let line = lines.next()?;
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    self.logical_line += 1;
                    let parse_started = Instant::now();
                    match self.ndjson_parser.parse_value(trimmed) {
                        Ok(value) => match value {
                            Value::Object(_) => {
                                ingest_profile::add_decode_ns(
                                    parse_started.elapsed().as_nanos() as u64
                                );
                                return Some(IngestFlattenItem::Row {
                                    line: self.logical_line,
                                    value,
                                });
                            }
                            Value::Array(values) => {
                                ingest_profile::add_decode_ns(
                                    parse_started.elapsed().as_nanos() as u64
                                );
                                self.queue.extend(values);
                                self.logical_line -= 1;
                            }
                            _ => {
                                ingest_profile::add_decode_ns(
                                    parse_started.elapsed().as_nanos() as u64
                                );
                                return Some(IngestFlattenItem::NotObject {
                                    line: trimmed.to_string(),
                                    offset_pos: self.logical_line,
                                });
                            }
                        },
                        Err(err) => {
                            ingest_profile::add_decode_ns(parse_started.elapsed().as_nanos() as u64);
                            return Some(IngestFlattenItem::ParseError {
                                line: trimmed.to_string(),
                                offset_pos: self.logical_line,
                                error: err.to_string(),
                            });
                        }
                    }
                }
            }
        }
    }
}

fn count_logical_ingest_records(records: &[Value]) -> usize {
    let mut count = 0usize;
    for record in records {
        match record {
            Value::Object(_) => count += 1,
            Value::Array(values) => count += values.len(),
            _ => count += 1,
        }
    }
    count
}

fn slow_ingest_blocking(
    namespace: &str,
    record: &SourceRecord,
    flatten: bool,
) -> Result<Value, String> {
    ensure_slow_ingest_worker();
    let tx = SLOW_INGEST_TX
        .get()
        .expect("slow ingest channel unavailable")
        .clone();
    let (resp_tx, resp_rx) = oneshot::channel();
    let task = SlowIngestTask {
        namespace: namespace.to_string(),
        record: record.clone(),
        flatten,
        resp_tx,
    };
    // Send and wait
    if let Err(_e) = tx.blocking_send(task) {
        return Err("Slow ingest worker unavailable".to_string());
    }
    resp_rx
        .blocking_recv()
        .unwrap_or_else(|_| Err("Slow ingest response dropped".to_string()))
}

use crate::ingest::deadletter::{self, DeadletterRecord};

fn partition_key_for_record(
    primary_sink_ref: &str,
    ns: &str,
    part: &str,
    time_b: &Option<i64>,
    schema_hash: &SchemaHash,
) -> IngestPartitionKey {
    (
        primary_sink_ref.to_string(),
        ns.to_string(),
        part.to_string(),
        time_b.clone(),
        schema_hash.hash.clone(),
    )
}

fn ensure_partition_buffer_entry<'a>(
    buf: &'a mut HashMap<IngestPartitionKey, IngestBufferBatch>,
    key: &IngestPartitionKey,
    insert_with: impl FnOnce() -> IngestBufferBatch,
) -> &'a mut IngestBufferBatch {
    if !buf.contains_key(key) {
        buf.insert(key.clone(), insert_with());
    }
    buf.get_mut(key).expect("partition buffer entry")
}

fn update_partition_offset(entry: &mut IngestBufferBatch, ok: &OffsetKey, pos: u64) {
    match entry.offsets.get_mut(ok) {
        Some(existing) => *existing = (*existing).max(pos),
        None => {
            entry.offsets.insert(ok.clone(), pos);
        }
    }
}

fn push_cdc_row_meta(
    cdc_row_buf: &mut HashMap<IngestPartitionKey, Vec<crate::plugins::cdc::WalRowMeta>>,
    key: &IngestPartitionKey,
    meta: crate::plugins::cdc::WalRowMeta,
) {
    if let Some(rows) = cdc_row_buf.get_mut(key) {
        rows.push(meta);
    } else {
        cdc_row_buf.insert(key.clone(), vec![meta]);
    }
}

fn track_partition_offset_with_key(
    buf: &mut HashMap<IngestPartitionKey, IngestBufferBatch>,
    primary_sink_ref: &str,
    key: &IngestPartitionKey,
    schema_hash: &SchemaHash,
    ns: &str,
    part: &str,
    time_b: &Option<i64>,
    ok: &OffsetKey,
    pos: u64,
    cdc_meta: Option<crate::plugins::cdc::WalRowMeta>,
    cdc_row_buf: &mut HashMap<IngestPartitionKey, Vec<crate::plugins::cdc::WalRowMeta>>,
) {
    let buffer_started = Instant::now();
    let entry = ensure_partition_buffer_entry(buf, key, || IngestBufferBatch {
        offsets: HashMap::new(),
        sink_ref: primary_sink_ref.to_string(),
        _namespace: ns.to_string(),
        _partition: part.to_string(),
        _time: time_b.clone(),
        _schema_fingerprint: String::new(),
        schema: schema_hash.schema.clone(),
        record_batches: None,
        cdc_rows: None,
    });
    update_partition_offset(entry, ok, pos);
    if let Some(meta) = cdc_meta {
        push_cdc_row_meta(cdc_row_buf, key, meta);
    }
    ingest_profile::add_buffer_offset_ns(buffer_started.elapsed().as_nanos() as u64);
}

fn track_partition_offset_with_schema(
    buf: &mut HashMap<IngestPartitionKey, IngestBufferBatch>,
    primary_sink_ref: &str,
    schema_hash: &SchemaHash,
    ns: &str,
    part: &str,
    time_b: &Option<i64>,
    ok: &OffsetKey,
    pos: u64,
    cdc_meta: Option<crate::plugins::cdc::WalRowMeta>,
    cdc_row_buf: &mut HashMap<IngestPartitionKey, Vec<crate::plugins::cdc::WalRowMeta>>,
) -> IngestPartitionKey {
    let buffer_started = Instant::now();
    let key = partition_key_for_record(primary_sink_ref, ns, part, time_b, schema_hash);
    let entry = ensure_partition_buffer_entry(buf, &key, || IngestBufferBatch {
        offsets: HashMap::new(),
        sink_ref: primary_sink_ref.to_string(),
        _namespace: ns.to_string(),
        _partition: part.to_string(),
        _time: time_b.clone(),
        _schema_fingerprint: String::new(),
        schema: schema_hash.schema.clone(),
        record_batches: None,
        cdc_rows: None,
    });
    update_partition_offset(entry, ok, pos);
    if let Some(meta) = cdc_meta {
        push_cdc_row_meta(cdc_row_buf, &key, meta);
    }
    ingest_profile::add_buffer_offset_ns(buffer_started.elapsed().as_nanos() as u64);
    key
}

fn track_partition_offset(
    buf: &mut HashMap<IngestPartitionKey, IngestBufferBatch>,
    primary_sink_ref: &str,
    schema_hash_cache: &mut HashMap<String, SchemaHash>,
    flatten: bool,
    ns: &str,
    part: &str,
    time_b: &Option<i64>,
    ok: &OffsetKey,
    pos: u64,
    cdc_meta: Option<crate::plugins::cdc::WalRowMeta>,
    cdc_row_buf: &mut HashMap<IngestPartitionKey, Vec<crate::plugins::cdc::WalRowMeta>>,
) -> IngestPartitionKey {
    let schema_hash = resolve_partition_schema(ns, flatten, schema_hash_cache);
    track_partition_offset_with_schema(
        buf,
        primary_sink_ref,
        &schema_hash,
        ns,
        part,
        time_b,
        ok,
        pos,
        cdc_meta,
        cdc_row_buf,
    )
}

fn enqueue_legacy_record(
    buf: &mut HashMap<IngestPartitionKey, IngestBufferBatch>,
    raw_values: &mut HashMap<IngestPartitionKey, Vec<IngestRecord>>,
    cdc_row_buf: &mut HashMap<IngestPartitionKey, Vec<crate::plugins::cdc::WalRowMeta>>,
    schema_hash_cache: &mut HashMap<String, SchemaHash>,
    primary_sink_ref: &str,
    flatten: bool,
    ns: &str,
    part: &str,
    time_b: &Option<i64>,
    source: SourceRecord,
    normalized: NormalizedRecord,
    ok: &OffsetKey,
    pos: u64,
    cdc_meta: Option<crate::plugins::cdc::WalRowMeta>,
) {
    let key = track_partition_offset(
        buf,
        primary_sink_ref,
        schema_hash_cache,
        flatten,
        ns,
        part,
        time_b,
        ok,
        pos,
        cdc_meta,
        cdc_row_buf,
    );
    match raw_values.get_mut(&key) {
        Some(records) => records.push(IngestRecord {
            source,
            normalized,
            _namespace: ns.to_string(),
            _partition: part.to_string(),
            _time: time_b.clone(),
            _offset_pos: pos,
        }),
        None => {
            raw_values.insert(
                key,
                vec![IngestRecord {
                    source,
                    normalized,
                    _namespace: ns.to_string(),
                    _partition: part.to_string(),
                    _time: time_b.clone(),
                    _offset_pos: pos,
                }],
            );
        }
    }
}

fn resolve_partition_schema(
    ns: &str,
    flatten: bool,
    cache: &mut HashMap<String, SchemaHash>,
) -> SchemaHash {
    let version = ARROW_SCHEMA_VERSION
        .get(ns)
        .map(|v| v.value().load(Ordering::Acquire))
        .unwrap_or(0);
    let version_key = format!("{version}");
    if let Some(cached) = cache.get(ns) {
        if cached.hash == version_key {
            return cached.clone();
        }
    }
    let md_snapshot = METADATA.load();
    let sh = Ingest::load_stable_schema_hash(ns, &md_snapshot.metadata, flatten);
    drop(md_snapshot);
    cache.insert(ns.to_string(), sh.clone());
    sh
}

pub(crate) fn namespace_schema_version(ns: &str) -> u64 {
    ARROW_SCHEMA_VERSION
        .get(ns)
        .map(|v| v.value().load(Ordering::Acquire))
        .unwrap_or(0)
}

/// Reload metadata when the namespace schema version advances (e.g. slow-path evolution).
pub(crate) fn refresh_metadata_snapshot_for_namespace(
    ns: &str,
    snapshot: &mut Arc<PipelineMetadata>,
    versions: &mut HashMap<String, u64>,
) {
    let version = namespace_schema_version(ns);
    if versions.get(ns) == Some(&version) {
        return;
    }
    let started = Instant::now();
    *snapshot = METADATA.load().clone();
    ingest_profile::add_metadata_load_ns(started.elapsed().as_nanos() as u64);
    versions.insert(ns.to_string(), version);
}

fn merge_record_batches(
    schema: SchemaRef,
    batches: Vec<RecordBatch>,
) -> Result<Vec<RecordBatch>, String> {
    if batches.is_empty() {
        return Ok(vec![]);
    }
    if batches.len() == 1 {
        return Ok(batches);
    }
    let merged = concat_batches(&schema, &batches).map_err(|e| e.to_string())?;
    Ok(vec![merged])
}

fn try_serialize_arrow_batch(
    schema: SchemaRef,
    values: &[&serde_json::Value],
) -> Result<Vec<RecordBatch>, String> {
    let mut decoder = ArrowJsonReaderBuilder::new(schema)
        .build_decoder()
        .map_err(|e| format!("build_decoder: {e}"))?;
    decoder
        .serialize(values)
        .map_err(|e| format!("serialize: {e}"))?;
    match decoder.flush() {
        Ok(Some(b)) => Ok(vec![b]),
        Ok(None) => Err("flush returned no batch".into()),
        Err(e) => Err(format!("flush: {e}")),
    }
}

fn bisect_serialize_indices(
    schema: SchemaRef,
    records: &[IngestRecord],
    indices: &[usize],
) -> (Vec<RecordBatch>, Vec<usize>) {
    if indices.is_empty() {
        return (vec![], vec![]);
    }
    let values: Vec<&serde_json::Value> = indices
        .iter()
        .map(|&i| records[i].normalized.inner())
        .collect();
    match try_serialize_arrow_batch(schema.clone(), &values) {
        Ok(batches) => (batches, vec![]),
        Err(_) if indices.len() == 1 => (vec![], vec![indices[0]]),
        Err(_) => {
            let mid = indices.len() / 2;
            let (left, right) = indices.split_at(mid);
            let (mut ok, mut fail) = bisect_serialize_indices(schema.clone(), records, left);
            let (right_ok, right_fail) = bisect_serialize_indices(schema, records, right);
            ok.extend(right_ok);
            fail.extend(right_fail);
            (ok, fail)
        }
    }
}

fn max_offset_pos_among(records: &[IngestRecord], indices: &[usize]) -> Option<u64> {
    if indices.is_empty() {
        None
    } else {
        Some(
            indices
                .iter()
                .map(|&i| records[i]._offset_pos)
                .max()
                .unwrap_or(0),
        )
    }
}

fn set_entry_offsets_max(entry: &mut IngestBufferBatch, max_pos: u64) {
    for pos in entry.offsets.values_mut() {
        *pos = max_pos;
    }
}

fn merge_dl_offsets_for_records(
    dl_offsets: &mut HashMap<OffsetKey, u64>,
    entry: &IngestBufferBatch,
    records: &[IngestRecord],
    indices: &[usize],
) {
    let Some(max_pos) = max_offset_pos_among(records, indices) else {
        return;
    };
    if let Some((ok, _)) = entry.offsets.iter().next() {
        merge_dl_offset_for_key(dl_offsets, ok, max_pos);
    }
}

fn merge_dl_offset_for_key(
    dl_offsets: &mut HashMap<OffsetKey, u64>,
    offset_key: &OffsetKey,
    offset_pos: u64,
) {
    dl_offsets
        .entry(offset_key.clone())
        .and_modify(|p| *p = (*p).max(offset_pos))
        .or_insert(offset_pos);
}

/// Whether a record at `offset_pos` should be ingested given one offset snapshot.
/// Immutable inputs rely on `Closed`; line positions apply only to CDC or explicit `offset_pos`.
fn should_ingest_at_offset(
    snapshot: Option<&OffsetValue>,
    is_cdc_batch: bool,
    track_position: bool,
    offset_pos: u64,
) -> bool {
    if is_cdc_batch {
        return true;
    }
    let Some(snap) = snapshot else {
        return true;
    };
    if snap.closed.get() != 0 {
        return false;
    }
    if track_position && snap.line.get() >= offset_pos {
        return false;
    }
    true
}

/// Physical metadata / warehouse table key for a source namespace.
///
/// Delegates to [`Helpers::clean_field_name`] (same rules as legacy namespace parsing):
/// lowercase snake_case, non-alphanumeric characters → `_`, collapsed underscores.
/// Apply once at ingest; sinks (Athena, Glue, etc.) must use the namespace as-is.
pub fn storage_namespace(namespace: &str) -> String {
    Helpers::clean_field_name(namespace.to_string())
}

static STORAGE_PATH_NON_ALNUM: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r"[^a-z0-9_]+").expect("valid regex"));
static STORAGE_PATH_COLLAPSE: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r"_+").expect("valid regex"));

/// Sanitize a path token (partition value or simple key) without field-name rules
/// (no leading-digit strip, no `item_` prefix for numeric-only tokens).
fn storage_path_token(raw: &str) -> String {
    let lower = raw.to_lowercase();
    let out = STORAGE_PATH_NON_ALNUM.replace_all(&lower, "_");
    STORAGE_PATH_COLLAPSE
        .replace_all(out.as_ref(), "_")
        .trim_matches('_')
        .to_string()
}

/// Sanitize a partition key or offset partition string for storage paths.
///
/// Hive-style paths (`col=val/...`) keep `/` and `=`; segment names use [`Helpers::clean_field_name`];
/// values use [`storage_path_token`] so years and timestamps keep leading digits.
pub fn storage_partition(partition: &str) -> String {
    if partition.is_empty() {
        return String::new();
    }
    if partition.contains('/') {
        partition
            .split('/')
            .filter(|segment| !segment.is_empty())
            .map(|segment| {
                if let Some((key, value)) = segment.split_once('=') {
                    format!(
                        "{}={}",
                        Helpers::clean_field_name(key.to_string()),
                        storage_path_token(value)
                    )
                } else {
                    Helpers::clean_field_name(segment.to_string())
                }
            })
            .collect::<Vec<_>>()
            .join("/")
    } else {
        storage_path_token(partition)
    }
}

// Bare metal platforms usually have very small amounts of RAM
// (in the order of hundreds of KB)
pub const _WRITE_BUF_SIZE: usize = if cfg!(target_os = "espidf") {
    512
} else {
    512 * 1024
};

thread_local! {
    pub static PARSE_NAMESPACE_CACHE: Lazy<RwLock<HashMap<String, String>>> = Lazy::new(|| RwLock::new(HashMap::new()));

    pub static PARTITION_ALLOWED_VALUES_CACHE: Lazy<RwLock<HashSet<String>>> = Lazy::new(|| RwLock::new(HashSet::new()));
}

fn cleaned_partition_allowed_values(raw: &str) -> HashSet<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| Helpers::clean_field_name(value.to_string()))
        .filter(|value| !value.is_empty())
        .collect()
}

#[cfg(test)]
mod partition_allowed_values_tests {
    use super::cleaned_partition_allowed_values;

    #[test]
    fn empty_partition_allowed_values_remain_unrestricted() {
        assert!(cleaned_partition_allowed_values("").is_empty());
        assert!(cleaned_partition_allowed_values(" , ").is_empty());
    }

    #[test]
    fn partition_allowed_values_are_cleaned_and_filtered() {
        let allowed = cleaned_partition_allowed_values("Foo Bar, 123, , baz");

        assert!(allowed.contains("foo_bar"));
        assert!(allowed.contains("item_123"));
        assert!(allowed.contains("baz"));
        assert_eq!(allowed.len(), 3);
    }
}

#[derive(Clone, Debug)]
struct SchemaHash {
    schema: SchemaRef,
    hash: String,
}

type IngestPartitionKey = (String, String, String, Option<i64>, String);

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct IngestBatch {
    pub offset_key: OffsetKey,
    pub data: String,
    pub bytes: usize,
    pub offset_pos: Option<u64>,
    #[allow(dead_code)]
    pub source_uri: String,
    /// Explicit namespace override from the input plugin (e.g. table name for
    /// MSSQL). When `None`, the pipeline name + `transform.namespace_fields`
    /// config is used to derive the namespace from message content.
    pub namespace: Option<String>,
    /// Per-row CDC metadata. When `Some`, each element aligns 1:1 with the
    /// rows parsed from `data`. Sources that don't produce CDC leave this as
    /// `None` (append semantics).
    pub cdc_rows: Option<Vec<crate::plugins::cdc::WalRowMeta>>,
}

impl IngestBatch {
    fn normalize_cdc_rows(
        rows: Option<Vec<crate::plugins::cdc::WalRowMeta>>,
    ) -> Option<Vec<crate::plugins::cdc::WalRowMeta>> {
        rows.filter(|rows| !rows.is_empty())
    }

    pub fn normalized_offset_key(
        namespace: impl Into<String>,
        partition: impl Into<String>,
    ) -> OffsetKey {
        OffsetKey {
            namespace: storage_namespace(&namespace.into()),
            partition: storage_partition(&partition.into()),
        }
    }

    pub fn new(
        offset_key: OffsetKey,
        data: String,
        bytes: usize,
        source_uri: String,
        namespace: Option<String>,
        cdc_rows: Option<Vec<crate::plugins::cdc::WalRowMeta>>,
    ) -> Self {
        let offset_key = Self::normalized_offset_key(offset_key.namespace, offset_key.partition);
        Self {
            offset_key,
            data,
            bytes,
            offset_pos: None,
            source_uri,
            namespace: namespace.map(|ns| storage_namespace(&ns)),
            cdc_rows: Self::normalize_cdc_rows(cdc_rows),
        }
    }

    pub fn new_with_offset_pos(
        offset_key: OffsetKey,
        data: String,
        bytes: usize,
        offset_pos: u64,
        source_uri: String,
        namespace: Option<String>,
        cdc_rows: Option<Vec<crate::plugins::cdc::WalRowMeta>>,
    ) -> Self {
        let mut batch = Self::new(offset_key, data, bytes, source_uri, namespace, cdc_rows);
        batch.offset_pos = Some(offset_pos);
        batch
    }

    pub fn offset_key(&self) -> &OffsetKey {
        &self.offset_key
    }

    pub fn data(&self) -> &str {
        &self.data
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn offset_pos_for_line(&self, line: u64) -> u64 {
        self.offset_pos
            .map(|base| base.saturating_add(line.saturating_sub(1)))
            .unwrap_or(line)
    }

    pub fn source_uri(&self) -> &str {
        &self.source_uri
    }

    pub fn namespace_override(&self) -> Option<&str> {
        self.namespace.as_deref()
    }

    pub fn cdc_rows(&self) -> Option<&[crate::plugins::cdc::WalRowMeta]> {
        self.cdc_rows.as_deref()
    }
}

#[cfg(test)]
mod ingest_batch_tests {
    use super::*;

    #[test]
    fn explicit_offset_pos_overrides_line_number() {
        let batch = IngestBatch::new_with_offset_pos(
            OffsetKey::new("postgres.orders", "orders"),
            "{}".to_string(),
            2,
            42,
            "postgres://orders".to_string(),
            Some("postgres.orders".to_string()),
            None,
        );

        assert_eq!(batch.offset_pos_for_line(1), 42);
        assert_eq!(batch.offset_pos_for_line(3), 44);
    }

    #[test]
    fn default_offset_pos_uses_line_number() {
        let batch = IngestBatch::new(
            OffsetKey::new("file.orders", "orders"),
            "{}".to_string(),
            2,
            "file://orders".to_string(),
            Some("file.orders".to_string()),
            None,
        );

        assert_eq!(batch.offset_pos_for_line(3), 3);
    }

    #[test]
    fn normalized_offset_key_matches_constructor() {
        let raw_key = OffsetKey::new("source-bucket", "raw/path/date=2026-06-24/file-1.json.gz");
        let helper_key = IngestBatch::normalized_offset_key(
            raw_key.namespace.clone(),
            raw_key.partition.clone(),
        );
        let batch = IngestBatch::new(
            raw_key,
            "{}".to_string(),
            2,
            "s3://source-bucket/raw/path/date=2026-06-24/file-1.json.gz".to_string(),
            None,
            None,
        );

        assert_eq!(batch.offset_key, helper_key);
    }
}

impl From<IngestBatch> for RuntimeRawIngestBatch {
    fn from(batch: IngestBatch) -> Self {
        Self {
            offset_key: batch.offset_key,
            data: batch.data,
            bytes: batch.bytes,
            offset_pos: batch.offset_pos,
            source_uri: batch.source_uri,
            namespace: batch.namespace,
            cdc_rows: batch.cdc_rows,
        }
    }
}

impl From<RuntimeRawIngestBatch> for IngestBatch {
    fn from(batch: RuntimeRawIngestBatch) -> Self {
        let mut ingest_batch = IngestBatch::new(
            batch.offset_key,
            batch.data,
            batch.bytes,
            batch.source_uri,
            batch.namespace,
            batch.cdc_rows,
        );
        ingest_batch.offset_pos = batch.offset_pos;
        ingest_batch
    }
}

#[derive(Clone)]
pub struct IngestTask {
    pub(crate) datas: Arc<Vec<IngestBatch>>,
    offset_db: Arc<Offsets>,
    shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
    submit_id: u64,
}

impl IngestTask {
    pub fn new(
        datas: Vec<IngestBatch>,
        offset_db: Arc<Offsets>,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> IngestTask {
        IngestTask {
            datas: Arc::new(datas),
            offset_db,
            shared_output,
            submit_id: 0,
        }
    }

    pub fn with_submit_id(mut self, submit_id: u64) -> Self {
        self.submit_id = submit_id;
        self
    }
}

#[derive(Clone)]
pub struct IngestTasks {
    tasks: Vec<IngestTask>,
    bytes: usize,
}

impl IngestTasks {
    pub fn new() -> IngestTasks {
        let num_cpus = num_cpus::get();

        IngestTasks {
            tasks: Vec::with_capacity(num_cpus),
            bytes: 0,
        }
    }

    pub fn add(&mut self, task: IngestTask) {
        self.bytes += task.datas.iter().map(|v| v.bytes).sum::<usize>();
        self.tasks.push(task);
    }
}

/// Return type for ingest_file that includes throughput metrics
#[derive(Clone, Debug)]
pub struct ThroughputMetrics {
    pub bytes_per_second: u64,
    pub active_cores: usize,
    pub queue_length: usize,
    pub optimal_chunk_size: usize,
}

/// Main struct for managing ingestion of data
/// Handles queueing, processing, and distribution of tasks to worker threads
pub struct Ingest {
    thread_pool: Arc<ThreadPool>,
    num_cpus: usize,
    execution_mode: RuntimeExecutionMode,
    tx: Sender<u64>,
    active_count: Arc<AtomicUsize>, // Track total number of active threads
    queue_length: Arc<AtomicUsize>, // Track total number of queued tasks
    /// Sum of `IngestBatch::bytes` for tasks not yet signaled complete on `tx`
    /// (queued or running). Paired with completion messages on `tx`.
    outstanding_bytes: Arc<AtomicUsize>,
    analyse_schema: AnalyseSchema,
    throughput_window: Arc<RwLock<VecDeque<(Instant, u64)>>>,
    throughput_lock: Arc<RwLock<()>>,
    window_size: Duration,
    task_queue: Arc<RwLock<VecDeque<IngestTask>>>,
    queue_lock: Arc<RwLock<()>>,
    queue_cv: Arc<(Mutex<()>, Condvar)>,
    max_queue_length: usize, // Maximum number of tasks to queue
    optimal_chunk_size: Arc<AtomicUsize>,
    throughput_history: Arc<RwLock<VecDeque<(Instant, u64)>>>, // Track throughput over time
    max_chunk_size: usize,
}

use tracing::{debug, error, info, warn};

impl Drop for Ingest {
    fn drop(&mut self) {
        let outstanding = self.queue_length.load(Ordering::SeqCst);
        if outstanding == 0 {
            return;
        }

        info!(
            "Completing, waiting for {} ingest tasks to finish",
            outstanding
        );

        self.wait_for_completion();
    }
}

impl Ingest {
    pub fn reset_discovery_progress() {
        *NUM_ANALYSED_RECORDS.write() = 0;
        DISCOVERY_COMPLETE.store(false, Ordering::Release);
    }

    pub fn discovery_complete() -> bool {
        DISCOVERY_COMPLETE.load(Ordering::Acquire)
    }

    fn normalize_data_dir_watermarks(high: u8, low: u8) -> Option<(u8, u8)> {
        if high == 0 {
            return None;
        }
        let high = high.clamp(1, 99);
        let low = low.clamp(0, 98);
        if low < high {
            Some((high, low))
        } else {
            Some((high, high.saturating_sub(5).max(1)))
        }
    }

    fn data_dir_watermarks() -> Option<(u8, u8)> {
        let high = Config::getenv("DATA_DIR_HIGH_WATERMARK_PCT", "90")
            .parse::<u8>()
            .unwrap_or(90);
        let low = Config::getenv("DATA_DIR_LOW_WATERMARK_PCT", "80")
            .parse::<u8>()
            .unwrap_or(80);
        Self::normalize_data_dir_watermarks(high, low)
    }

    #[cfg(unix)]
    fn data_dir_disk_usage() -> Option<(u64, u64, f64)> {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::path::Path;

        let data_dir = Config::get_data_dir();
        let path = Path::new(&data_dir);
        let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        let rc = unsafe { libc::statvfs(c_path.as_ptr(), stats.as_mut_ptr()) };
        if rc != 0 {
            return None;
        }
        let stats = unsafe { stats.assume_init() };
        let block_size = u128::from(if stats.f_frsize > 0 {
            stats.f_frsize
        } else {
            stats.f_bsize
        });
        let total_bytes_u128 = u128::from(stats.f_blocks).checked_mul(block_size)?;
        let avail_bytes_u128 = u128::from(stats.f_bavail).checked_mul(block_size)?;
        let total_bytes = u64::try_from(total_bytes_u128).ok()?;
        let avail_bytes = u64::try_from(avail_bytes_u128).ok()?;
        if total_bytes == 0 {
            return None;
        }
        let used_pct = 100.0 - ((avail_bytes as f64 * 100.0) / total_bytes as f64);
        Some((avail_bytes, total_bytes, used_pct))
    }

    #[cfg(not(unix))]
    fn data_dir_disk_usage() -> Option<(u64, u64, f64)> {
        None
    }

    const DEFAULT_MIN_FREE_BYTES: u64 = 5 * 1024 * 1024 * 1024; // 5 GB

    fn min_free_bytes() -> u64 {
        Config::getenv("DATA_DIR_MIN_FREE_BYTES", "")
            .parse::<u64>()
            .unwrap_or(Self::DEFAULT_MIN_FREE_BYTES)
    }

    fn data_dir_should_block(
        avail_bytes: u64,
        used_pct: f64,
        high_watermark: u8,
        min_free_bytes: u64,
    ) -> bool {
        avail_bytes < min_free_bytes || used_pct >= high_watermark as f64
    }

    fn data_dir_should_resume(
        avail_bytes: u64,
        used_pct: f64,
        low_watermark: u8,
        min_free_bytes: u64,
    ) -> bool {
        avail_bytes >= min_free_bytes && used_pct <= low_watermark as f64
    }

    fn data_dir_capacity_fatal_message(
        avail_bytes: u64,
        total_bytes: u64,
        used_pct: f64,
        min_free_bytes: u64,
        high_watermark: u8,
        below_min_free: bool,
        above_high_watermark: bool,
    ) -> String {
        let mut reasons = Vec::new();
        if below_min_free {
            reasons.push(format!(
                "free space {} is below minimum {}",
                Helpers::human_readable_size(avail_bytes),
                Helpers::human_readable_size(min_free_bytes)
            ));
        }
        if above_high_watermark {
            reasons.push(format!(
                "usage {:.1}% is above high watermark {}%",
                used_pct, high_watermark
            ));
        }
        format!(
            "DATA_DIR capacity exhausted ({}, total {}). This pipeline has no reclaimable committed WAL to compact. Free disk or clean another pipeline before retrying.",
            reasons.join("; "),
            Helpers::human_readable_size(total_bytes)
        )
    }

    fn throughput_metrics(&self) -> ThroughputMetrics {
        ThroughputMetrics {
            bytes_per_second: self.get_current_throughput(),
            active_cores: self.active_count.load(Ordering::Acquire),
            queue_length: self.queue_length.load(Ordering::Acquire),
            optimal_chunk_size: self.optimal_chunk_size.load(Ordering::Acquire),
        }
    }

    fn wait_for_data_dir_capacity() -> bool {
        if crate::data_dir_capacity_exceeded() {
            return false;
        }
        let Some((high_watermark, low_watermark)) = Self::data_dir_watermarks() else {
            return true;
        };
        let mut paused = data_dir_ingest_paused();

        loop {
            let Some((avail_bytes, total_bytes, used_pct)) = Self::data_dir_disk_usage() else {
                if paused {
                    set_data_dir_ingest_paused(false);
                }
                return true;
            };

            // Independent guards: absolute free-space floor and percentage high watermark.
            let min_free_bytes = Self::min_free_bytes();
            let below_min_free = avail_bytes < min_free_bytes;
            let above_high_watermark = used_pct >= high_watermark as f64;
            let should_block =
                Self::data_dir_should_block(avail_bytes, used_pct, high_watermark, min_free_bytes);

            if !paused {
                if !should_block {
                    return true;
                }
                if !Buffers::has_reclaimable_wal() {
                    let message = Self::data_dir_capacity_fatal_message(
                        avail_bytes,
                        total_bytes,
                        used_pct,
                        min_free_bytes,
                        high_watermark,
                        below_min_free,
                        above_high_watermark,
                    );
                    error!("{}", message);
                    record_data_dir_capacity_error(message);
                    return false;
                }
                paused = true;
                set_data_dir_ingest_paused(true);
                Buffers::wake_compactor();
                DATA_DIR_INGEST_PAUSE_LAST_LOG_SECS.store(0, Ordering::Relaxed);
                let progress = crate::buffer::ingest_buffer::Buffers::pause_progress_snapshot();
                if below_min_free {
                    warn!(
                        "Pausing ingest: free {} is below minimum {} (usage {:.1}%, total {}). Force compaction will run with raised concurrency until resume thresholds are met. WAL state: segs_remaining={}, reclaimable_partitions={}",
                        Helpers::human_readable_size(avail_bytes),
                        Helpers::human_readable_size(min_free_bytes),
                        used_pct,
                        Helpers::human_readable_size(total_bytes),
                        progress.segs_remaining,
                        progress.reclaimable_partitions,
                    );
                } else {
                    warn!(
                        "Pausing ingest: DATA_DIR usage {:.1}% is above high watermark {}% (free {} / total {}). Force compaction will run with raised concurrency until resume thresholds are met. WAL state: segs_remaining={}, reclaimable_partitions={}",
                        used_pct,
                        high_watermark,
                        Helpers::human_readable_size(avail_bytes),
                        Helpers::human_readable_size(total_bytes),
                        progress.segs_remaining,
                        progress.reclaimable_partitions,
                    );
                }
            }

            if Self::data_dir_should_resume(avail_bytes, used_pct, low_watermark, min_free_bytes) {
                set_data_dir_ingest_paused(false);
                info!(
                    "Resuming ingest: DATA_DIR usage {:.1}% is below low watermark {}% (free {} / total {}).",
                    used_pct,
                    low_watermark,
                    Helpers::human_readable_size(avail_bytes),
                    Helpers::human_readable_size(total_bytes)
                );
                return true;
            }

            let now_secs = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let last_log = DATA_DIR_INGEST_PAUSE_LAST_LOG_SECS.load(Ordering::Relaxed);
            if now_secs.saturating_sub(last_log) >= 30 {
                DATA_DIR_INGEST_PAUSE_LAST_LOG_SECS.store(now_secs, Ordering::Relaxed);
                let progress = crate::buffer::ingest_buffer::Buffers::pause_progress_snapshot();
                info!(
                    "Ingest remains paused: DATA_DIR usage {:.1}% (resume below {}%, free {} / total {}). Reclaiming WAL: segs_remaining={}, reclaimable_partitions={}, wal_compactions_inflight={}, uploads_inflight={}, wal_compactions_completed={}, wal_txn_completed={}, wal_refs_tombstoned={}",
                    used_pct,
                    low_watermark,
                    Helpers::human_readable_size(avail_bytes),
                    Helpers::human_readable_size(total_bytes),
                    progress.segs_remaining,
                    progress.reclaimable_partitions,
                    progress.wal_compactions_in_flight,
                    progress.uploads_in_flight,
                    progress.wal_compactions_completed,
                    progress.wal_txn_completed,
                    progress.wal_refs_tombstoned,
                );
            }

            if !RUNNING.read().load(Ordering::SeqCst) {
                return true;
            }

            crate::ingest::tuner::paused_tick(num_cpus::get());
            Buffers::wake_compactor();
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    // Lightweight Linux memory readers; on non-Linux fall back to None
    fn read_meminfo_kib(key: &str) -> Option<u64> {
        if let Ok(file) = std::fs::File::open("/proc/meminfo") {
            let reader = std::io::BufReader::new(file);
            for line in reader.lines().flatten() {
                if line.starts_with(key) {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 2 {
                        if let Ok(v) = parts[1].parse::<u64>() {
                            return Some(v);
                        }
                    }
                }
            }
        }
        None
    }
    fn read_mem_total_mib() -> Option<u64> {
        Self::read_meminfo_kib("MemTotal:").map(|kib| kib / 1024)
    }
    fn read_mem_available_mib() -> Option<u64> {
        Self::read_meminfo_kib("MemAvailable:").map(|kib| kib / 1024)
    }

    /// Autotuned cap on summed in-flight ingest payload bytes (queued + active).
    /// With `WAL_STORAGE=s3`, uses this process’s share of the **combined** S3 WAL memory
    /// envelope (see `buffer::s3_wal_memory_budget`): admission + S3 body LRU never exceed
    /// ~70% of MemAvailable (Linux) / total RAM (macOS) together.
    /// Disk WAL keeps a lower base fraction plus mild scaling with ingest thread count.
    fn compute_admission_budget_bytes(
        hint_bytes: usize,
        wal_s3: bool,
        num_ingest_threads: usize,
    ) -> usize {
        let budget = if wal_s3 {
            crate::buffer::s3_wal_memory_budget::split_s3_wal_memory_budget(hint_bytes).0
        } else {
            let n = num_ingest_threads.max(1);
            let raw = hint_bytes
                .saturating_mul(25)
                .saturating_div(100)
                .saturating_mul(n.saturating_add(2))
                .saturating_div(4);
            const MIN_B: usize = 256 * 1024 * 1024;
            const MAX_B: usize = 64 * 1024 * 1024 * 1024;
            raw.clamp(MIN_B, MAX_B)
        };
        budget
    }

    fn autotuned_ingest_admission_budget_bytes(num_ingest_threads: usize) -> usize {
        let hint = crate::buffer::s3_wal_memory_budget::memory_hint_bytes()
            .unwrap_or(crate::buffer::s3_wal_memory_budget::FALLBACK_MEMORY_HINT_BYTES);
        let wal_s3 = Config::get_wal_storage().eq_ignore_ascii_case("s3");
        Self::compute_admission_budget_bytes(hint, wal_s3, num_ingest_threads)
    }

    /// `LOG_WAL`: snapshot of ingest byte admission vs autotuned budget.
    pub(crate) fn log_wal_ingest_pressure_snapshot(&self) {
        if !Config::log_wal_enabled() {
            return;
        }
        let outstanding = self.outstanding_bytes.load(Ordering::Acquire);
        let budget = Self::autotuned_ingest_admission_budget_bytes(self.num_cpus);
        let ql = self.queue_length.load(Ordering::Acquire);
        let active = self.active_count.load(Ordering::Acquire);
        info!(
            "ingest core WAL pressure: outstanding_bytes={} admission_budget_bytes={} queue_len={} active_threads={}/{} max_queue_len={}",
            outstanding,
            budget,
            ql,
            active,
            self.num_cpus,
            self.max_queue_length
        );
    }

    #[cfg(test)]
    pub(crate) fn admission_budget_for_tests(
        hint_bytes: usize,
        wal_s3: bool,
        num_ingest_threads: usize,
    ) -> usize {
        Self::compute_admission_budget_bytes(hint_bytes, wal_s3, num_ingest_threads)
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn outstanding_payload_bytes_for_tests(&self) -> usize {
        self.outstanding_bytes.load(Ordering::Acquire)
    }
    #[allow(dead_code)]
    fn infer_required_type(value: &serde_json::Value) -> (String, Option<String>) {
        use serde_json::Value as V;
        match value {
            V::Null => ("string".to_string(), None),
            V::Bool(_) => ("boolean".to_string(), None),
            V::Number(n) => {
                if n.is_i64() {
                    ("long".to_string(), None)
                } else {
                    ("double".to_string(), None)
                }
            }
            V::String(_) => ("string".to_string(), None),
            V::Array(arr) => {
                if let Some(V::Object(_)) = arr.get(0) {
                    ("array".to_string(), Some("record".to_string()))
                } else {
                    ("array".to_string(), Some("string".to_string()))
                }
            }
            V::Object(_) => ("record".to_string(), None),
        }
    }

    #[allow(dead_code)]
    fn build_evolution_from_record(
        namespace: &str,
        record: &serde_json::Value,
        metadata: &HashMap<String, Metadata>,
    ) -> EvolutionProposal {
        let fields = infer_specs_for_record(record, metadata);
        EvolutionProposal {
            namespace: namespace.to_string(),
            fields,
        }
    }
    #[inline]
    fn load_stable_schema_hash(
        skpr_namespace: &str,
        metadata: &HashMap<String, Metadata>,
        flatten: bool,
    ) -> SchemaHash {
        const MAX_ITERS: u32 = 50; // ~500ms
        let mut iters = 0u32;
        loop {
            let v1 = ARROW_SCHEMA_VERSION
                .get(skpr_namespace)
                .map(|v| v.value().load(Ordering::Acquire))
                .unwrap_or(0);
            let schema_opt = ARROW_SCHEMA
                .get(skpr_namespace)
                .map(|e| Arc::clone(&e.value().load()));
            if schema_opt.is_none() {
                // Singleflight prepare
                let lock = SCHEMA_PREP_LOCKS
                    .entry(skpr_namespace.to_string())
                    .or_insert_with(|| Arc::new(std::sync::Mutex::new(())))
                    .clone();
                let _guard = lock.lock().unwrap();
                if ARROW_SCHEMA.get(skpr_namespace).is_none() {
                    let _ = Ingest::prepare_arrow_schema_with_metadata(
                        skpr_namespace,
                        metadata,
                        flatten,
                    );
                }
            }
            let schema: SchemaRef = ARROW_SCHEMA
                .get(skpr_namespace)
                .map(|e| Arc::clone(&e.value().load()))
                .unwrap();
            let v2 = ARROW_SCHEMA_VERSION
                .get(skpr_namespace)
                .map(|v| v.value().load(Ordering::Acquire))
                .unwrap_or(0);
            if v1 == v2 {
                let hash = format!("{}", v2);
                return SchemaHash { schema, hash };
            }
            if iters >= MAX_ITERS {
                let shard_version = ARROW_SCHEMA_VERSION
                    .get(skpr_namespace)
                    .map(|v| v.value().load(Ordering::Acquire))
                    .unwrap_or(0);
                let hash = format!("{}", shard_version);
                if iters > 0 {
                    warn!("Schema for namespace {} changed during stable read, proceeding with latest version", skpr_namespace);
                }
                return SchemaHash { schema, hash };
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            iters += 1;
        }
    }
    pub fn new() -> Ingest {
        Self::new_for_execution(RuntimeExecutionMode::Sync)
    }

    pub fn new_for_execution(execution_mode: RuntimeExecutionMode) -> Ingest {
        // We always want to use all cores unless overridden by env
        // On CI, cap default threads to reduce contention unless explicitly overridden
        let is_ci = {
            let ga = Config::getenv("GITHUB_ACTIONS", "");
            let ci = Config::getenv("CI", "");
            ga.eq_ignore_ascii_case("true") || ci == "1" || ci.eq_ignore_ascii_case("true")
        };
        let default_threads = match execution_mode {
            RuntimeExecutionMode::Sync => {
                let cores = num_cpus::get();
                if is_ci {
                    cores.min(8)
                } else {
                    cores
                }
            }
            RuntimeExecutionMode::Discover => 1,
        };
        let num_cpus = Config::getenv("INGEST_THREADS", "")
            .parse::<usize>()
            .ok()
            .filter(|v| *v > 0)
            .unwrap_or(default_threads);

        info!("Starting with {} optimized threads for ingest", num_cpus);

        // Optional: cap hot-path concurrencies via env, and auto-cap on CI
        let is_ci = {
            let ga = Config::getenv("GITHUB_ACTIONS", "");
            let ci = Config::getenv("CI", "");
            ga.eq_ignore_ascii_case("true") || ci == "1" || ci.eq_ignore_ascii_case("true")
        };

        // Stats tailer disabled

        // Apply one-time environment overrides and CI caps via tuner
        crate::ingest::tuner::apply_env_caps();

        let (tx, rx) = channel();
        let tx_clone = tx.clone();
        let active_count = Arc::new(AtomicUsize::new(0));
        let active_count_clone = active_count.clone();
        let queue_length = Arc::new(AtomicUsize::new(0));
        let outstanding_bytes = Arc::new(AtomicUsize::new(0));
        let outstanding_bytes_clone = outstanding_bytes.clone();
        let start_chunk: usize = Config::getenv("INGEST_START_CHUNK_BYTES", "5000000")
            .parse::<usize>()
            .unwrap_or(5_000_000);
        // Auto-tune maximum adaptive chunk size from system memory (no flag)
        let total_mib_opt = Self::read_mem_total_mib();
        // Reserve ~20% of total memory for ingest payloads across active tasks (num_cpus) and Arrow overhead (~2x)
        let denom = (num_cpus * 2).max(2);
        let mut max_chunk_size: usize = match total_mib_opt {
            Some(mib) => {
                let budget_bytes = ((mib as usize).saturating_mul(1024 * 1024)) / 5; // 20%
                let per_task = budget_bytes / denom;
                per_task
            }
            None => 64 * 1024 * 1024, // Fallback 64 MiB if memory unknown
        };
        // Clamp to a sane range [4 MiB, 128 MiB]
        if max_chunk_size < 4 * 1024 * 1024 {
            max_chunk_size = 4 * 1024 * 1024;
        }
        if max_chunk_size > 128 * 1024 * 1024 {
            max_chunk_size = 128 * 1024 * 1024;
        }
        let initial_chunk = std::cmp::min(start_chunk, max_chunk_size);
        let optimal_chunk_size = Arc::new(AtomicUsize::new(initial_chunk));
        let queue_length_clone = queue_length.clone();
        let task_queue: Arc<RwLock<VecDeque<IngestTask>>> = Arc::new(RwLock::new(VecDeque::new()));
        let task_queue_clone = task_queue.clone();
        let queue_lock = Arc::new(RwLock::new(()));
        let queue_cv: Arc<(Mutex<()>, Condvar)> = Arc::new((Mutex::new(()), Condvar::new()));
        let queue_lock_clone = queue_lock.clone();
        let queue_cv_clone = queue_cv.clone();

        let (thread_pool, effective_cpus) = build_ingest_thread_pool(num_cpus);
        if effective_cpus != num_cpus {
            info!(
                "Using {} ingest threads (requested {})",
                effective_cpus, num_cpus
            );
        }
        let num_cpus = effective_cpus;
        crate::buffer::wal_writer::start(num_cpus);
        let thread_pool = Arc::new(thread_pool);
        let thread_pool_clone = thread_pool.clone();

        let shared_handle = INGEST_RT.handle().clone();

        let queue_factor: usize =
            Config::getenv("INGEST_MAX_QUEUE_FACTOR", if is_ci { "1" } else { "2" })
                .parse::<usize>()
                .unwrap_or(if is_ci { 1 } else { 2 });
        let max_queue_length = (num_cpus * queue_factor).max(num_cpus);
        std::thread::spawn(move || {
            while let Ok(completed_bytes) = rx.recv() {
                // Check if we're shutting down
                // if is_shutting_down_clone.load(Ordering::SeqCst) > 0 {
                //     self.wait_for_completion();
                //     println!("Shutting down monitoring thread");
                //     break;
                // }

                active_count_clone.fetch_sub(1, AcqRel);
                queue_length_clone.fetch_sub(1, AcqRel);
                outstanding_bytes_clone.fetch_sub(completed_bytes as usize, AcqRel);
                // Wake any producers waiting on queue capacity
                let (lock, cv) = &*queue_cv_clone;
                if let Ok(_g) = lock.lock() {
                    cv.notify_all();
                }

                let current_queue_length = queue_length_clone.load(Ordering::Acquire);
                let current_active_threads = active_count_clone.load(Ordering::Acquire);
                crate::metrics::counters::set_active_threads(current_active_threads);
                crate::metrics::counters::set_queue_length(current_queue_length);

                // println!("Task completed ({} tasks in queue, {}/{} active threads)",
                //          current_queue_length,
                //          current_active_threads,
                //          num_cpus
                // );

                // Process any queued tasks if we have capacity
                if current_active_threads < num_cpus {
                    let _lock = match queue_lock_clone.write() {
                        Ok(lock) => lock,
                        Err(e) => {
                            error!("Failed to acquire queue lock: {:?}", e);
                            continue;
                        }
                    };

                    let mut task_queue = match task_queue_clone.write() {
                        Ok(queue) => queue,
                        Err(e) => {
                            error!("Failed to acquire task queue: {:?}", e);
                            continue;
                        }
                    };

                    // Process tasks from the queue while we have capacity
                    while let Some(ingest_task) = task_queue.pop_front() {
                        if active_count_clone.load(Ordering::Acquire) >= num_cpus {
                            // Put the task back if we're at capacity
                            task_queue.push_front(ingest_task);
                            break;
                        }

                        // Process the queued task
                        let tx = tx_clone.clone();
                        let offset_db_clone = ingest_task.offset_db.clone();
                        let datas_clone = ingest_task.datas.clone();
                        let shared_output_clone = ingest_task.shared_output.clone();
                        let submit_id = ingest_task.submit_id;

                        // Increment active count before spawning
                        active_count_clone.fetch_add(1, AcqRel);
                        crate::metrics::counters::set_active_threads(
                            active_count_clone.load(Ordering::Acquire),
                        );
                        // Note: We don't increment queue_length here since we're processing from the queue,
                        // and the task was already counted in queue_length when it was added to the queue

                        let queued_handle = shared_handle.clone();
                        let completed_bytes: u64 = datas_clone.iter().map(|b| b.bytes as u64).sum();
                        thread_pool_clone.execute(move || {
                            let result =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    Ingest::process_batch(
                                        &datas_clone,
                                        &offset_db_clone,
                                        queued_handle,
                                        shared_output_clone,
                                        submit_id,
                                    );
                                }));
                            if result.is_err() {
                                error!("Queued ingest task panicked; forcing completion signal");
                                crate::buffer::wal_writer::fail_request(
                                    submit_id,
                                    "queued ingest task panicked",
                                );
                            }
                            let _ = tx.send(completed_bytes);
                        });
                    }
                }
            }
            debug!("Monitoring thread exited");
        });

        let analyse_schema: AnalyseSchema = AnalyseSchema { i: 0 };
        let throughput_window = Arc::new(RwLock::new(VecDeque::with_capacity(100)));
        let throughput_lock = Arc::new(RwLock::new(()));
        let window_size = Duration::from_secs(5); // 5 second window for throughput calculation

        let throughput_history = Arc::new(RwLock::new(VecDeque::with_capacity(100)));
        let _last_adjustment = Arc::new(RwLock::new(Instant::now()));
        // let adjustment_cooldown = Duration::from_secs(10); // 10 second cooldown between adjustments

        Ingest {
            num_cpus,
            execution_mode,
            thread_pool,
            tx,
            active_count,
            queue_length,
            outstanding_bytes,
            analyse_schema,
            throughput_window,
            throughput_lock,
            window_size,
            task_queue,
            queue_lock,
            queue_cv,
            max_queue_length,
            optimal_chunk_size,
            throughput_history,
            max_chunk_size,
        }
    }

    pub fn wait_for_completion(&self) {
        let remaining = self.queue_length.load(Ordering::SeqCst);
        if remaining > 0 {
            info!("Draining ingest queue: {remaining} tasks outstanding");
        }

        let mut last_report_time = Instant::now();
        while self.queue_length.load(Ordering::SeqCst) > 0
            || self.outstanding_bytes.load(Ordering::SeqCst) > 0
            || crate::buffer::wal_writer::has_pending_work()
        {
            if last_report_time.elapsed() > Duration::from_secs(5) {
                info!(
                    "Draining ingest queue: {} tasks outstanding ({} active), outstanding_bytes={}, wal_pending_count={}, wal_pending_bytes={}",
                    self.queue_length.load(Ordering::SeqCst),
                    self.active_count.load(Ordering::SeqCst),
                    self.outstanding_bytes.load(Ordering::SeqCst),
                    crate::buffer::wal_writer::pending_count(),
                    crate::buffer::wal_writer::pending_bytes(),
                );
                last_report_time = Instant::now();
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        if remaining > 0 {
            info!("Ingest queue drained");
        }
    }

    fn update_throughput(&self, bytes: u64) {
        let now = Instant::now();
        let _lock = self.throughput_lock.write().unwrap();
        let mut window = self.throughput_window.write().unwrap();

        // Add new measurement
        window.push_back((now, bytes));

        // Remove old measurements outside window
        while let Some((time, _)) = window.front() {
            if now.duration_since(*time) > self.window_size {
                window.pop_front();
            } else {
                break;
            }
        }
    }

    fn get_current_throughput(&self) -> u64 {
        let now = Instant::now();
        let _lock = self.throughput_lock.read().unwrap();
        let window = self.throughput_window.read().unwrap();

        if window.is_empty() {
            return 0;
        }

        let oldest_time = window.front().unwrap().0;
        let total_bytes: u64 = window.iter().map(|(_, bytes)| bytes).sum();
        crate::ingest::tuner::rolling_rate_bytes_per_sec(
            oldest_time,
            now,
            total_bytes,
            crate::ingest::tuner::MIN_THROUGHPUT_RATE_WINDOW,
        )
    }

    // get_optmial_chunk_size removed: logic moved to tuner and applied inline where invoked

    /// Add a file to the ingestion queue
    ///
    /// This function will either process the file immediately if there is capacity
    /// or queue it for later processing. It includes backpressure handling to prevent
    /// unbounded queue growth.
    ///
    /// Returns: A ThroughputMetrics struct containing current system state and optimal chunk size
    pub fn ingest_file(
        &self,
        ingest_batches: &Arc<IngestTasks>,
        offset_db: &Arc<Offsets>,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> ThroughputMetrics {
        if crate::data_dir_capacity_exceeded() {
            info!("Waiting for remaining ingest tasks after DATA_DIR capacity exhaustion");
            self.wait_for_completion();
            return self.throughput_metrics();
        }

        // If we're not running, exit after current threads finish.
        if !RUNNING.read().load(Ordering::SeqCst) {
            info!("Waiting for remaining threads to complete");
            self.wait_for_completion();
            exit(0);
        } else {
            match self.execution_mode {
                RuntimeExecutionMode::Sync => {}
                RuntimeExecutionMode::Discover => {
                    let max_records = 1000;
                    let pipeline_metadata_arc = METADATA.load();
                    let mut pipeline_metadata: PipelineMetadata =
                        pipeline_metadata_arc.as_ref().clone();

                    let mut count: u64 = 0;

                    let first_task_datas = ingest_batches.tasks.first().map(|t| &t.datas);
                    for data in first_task_datas.into_iter().flat_map(|d| d.iter()) {
                        let batch_ns: Option<String> = {
                            let part = &data.offset_key.partition;
                            if !part.is_empty() {
                                Some(part.rsplit('.').next().unwrap_or(part).to_string())
                            } else {
                                None
                            }
                        };
                        count += self.analyse_schema.infer_json_schema(
                            &mut data.data.clone(),
                            Some(max_records),
                            &mut pipeline_metadata.metadata,
                            batch_ns.as_deref(),
                        );

                        {
                            *NUM_ANALYSED_RECORDS.write() += count;
                        }

                        if *NUM_ANALYSED_RECORDS.read() >= max_records {
                            break;
                        }
                    }

                    {
                        let mut new_pm = METADATA.load().as_ref().clone();
                        new_pm.metadata = pipeline_metadata.metadata.clone();
                        METADATA.store(Arc::new(new_pm));
                    }

                    if *NUM_ANALYSED_RECORDS.read() >= max_records {
                        let mut pipeline_metadata = METADATA.load().as_ref().clone();

                        if pipeline_metadata.metadata.len() == 0 {
                            warn!("No data found in data source, skipping schema discovery");
                            DISCOVERY_COMPLETE.store(true, Ordering::Release);
                            return ThroughputMetrics {
                                bytes_per_second: self.get_current_throughput(),
                                active_cores: self.active_count.load(Ordering::Acquire),
                                queue_length: self.queue_length.load(Ordering::Acquire),
                                optimal_chunk_size: self.optimal_chunk_size.load(Ordering::Acquire),
                            };
                        }

                        let flatten = Config::truth_value(
                            &Config::get_transform_config()
                                .flatten_events
                                .unwrap_or("false".to_string()),
                        );

                        for (_namespace, metadata) in pipeline_metadata.metadata.iter_mut() {
                            AnalyseSchema::determine_field_types(
                                &mut metadata.fields,
                                None,
                                flatten,
                            );
                        }

                        info!("Schema discovery complete, writing metadata to Skippr");

                        pipeline_metadata.enabled = true;
                        INGEST_RT.block_on(Config::set_metadata(&pipeline_metadata, false));
                        DISCOVERY_COMPLETE.store(true, Ordering::Release);
                    }

                    info!(
                        "Analysed schema for {} -> {}/{} records",
                        count,
                        *NUM_ANALYSED_RECORDS.read(),
                        max_records
                    );

                    return ThroughputMetrics {
                        bytes_per_second: self.get_current_throughput(),
                        active_cores: self.active_count.load(Ordering::Acquire),
                        queue_length: self.queue_length.load(Ordering::Acquire),
                        optimal_chunk_size: self.optimal_chunk_size.load(Ordering::Acquire),
                    };
                }
            }

            // Calculate total bytes in this batch
            let batch_bytes = ingest_batches.bytes;

            // Per-batch: no pre-wait; we gate per-task below to keep queue depth bounded

            // Get current CPU utilization (refresh inside loop to avoid oversubscription)
            let mut _active_threads_snapshot = self.active_count.load(Ordering::SeqCst);

            for datas in ingest_batches.tasks.iter() {
                if !Self::wait_for_data_dir_capacity() {
                    info!("Stopping ingest after DATA_DIR capacity exhaustion");
                    self.wait_for_completion();
                    return self.throughput_metrics();
                }
                let task_bytes: usize = datas.datas.iter().map(|v| v.bytes).sum();
                let byte_budget =
                    Self::autotuned_ingest_admission_budget_bytes(self.num_cpus).max(task_bytes);
                let (lock, cv) = &*self.queue_cv;
                let mut guard = lock.lock().unwrap();
                let mut pressure_logged = false;
                loop {
                    let ql = self.queue_length.load(Ordering::Acquire);
                    let ob = self.outstanding_bytes.load(Ordering::Acquire);
                    if ql < self.max_queue_length && ob.saturating_add(task_bytes) <= byte_budget {
                        break;
                    }
                    if Config::log_wal_enabled() && !pressure_logged {
                        pressure_logged = true;
                        info!(
                            "ingest admission backpressure: ql={}/{} outstanding_bytes={} task_bytes={} byte_budget={} active={}/{}",
                            ql,
                            self.max_queue_length,
                            ob,
                            task_bytes,
                            byte_budget,
                            self.active_count.load(Ordering::Acquire),
                            self.num_cpus,
                        );
                    }
                    guard = cv.wait(guard).unwrap();
                }
                drop(guard);
                self.outstanding_bytes
                    .fetch_add(task_bytes, Ordering::AcqRel);

                // Refresh snapshot each iteration to avoid spawning beyond capacity
                _active_threads_snapshot = self.active_count.load(Ordering::SeqCst);
                if _active_threads_snapshot < self.num_cpus {
                    let tx = self.tx.clone();
                    let offset_db_clone = offset_db.clone();
                    let datas_clone = datas.datas.clone();
                    let handle = runtime::Handle::current();
                    let shared_output_clone = shared_output.clone();
                    let submit_id = datas.submit_id;
                    let completed_bytes: u64 = datas_clone.iter().map(|b| b.bytes as u64).sum();

                    // Increment active count and queue length before spawning
                    self.active_count.fetch_add(1, Ordering::Acquire);
                    self.queue_length.fetch_add(1, Ordering::Acquire);
                    crate::metrics::counters::set_active_threads(
                        self.active_count.load(Ordering::Acquire),
                    );
                    crate::metrics::counters::set_queue_length(
                        self.queue_length.load(Ordering::Acquire),
                    );

                    // Spawn the task and ensure it's executed
                    self.thread_pool.execute(move || {
                        // Ensure panics do not wedge queue accounting; always signal completion
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            Ingest::process_batch(
                                &datas_clone,
                                &offset_db_clone,
                                handle,
                                shared_output_clone,
                                submit_id,
                            );
                        }));
                        if result.is_err() {
                            error!("Active ingest task panicked; forcing completion signal");
                            crate::buffer::wal_writer::fail_request(
                                submit_id,
                                "active ingest task panicked",
                            );
                        }
                        let _ = tx.send(completed_bytes);
                    });

                    self.update_throughput(completed_bytes);
                } else {
                    // Queue the task for later processing
                    let _lock = self.queue_lock.write().unwrap();
                    self.task_queue.write().unwrap().push_back(IngestTask {
                        datas: datas.datas.clone(),
                        offset_db: offset_db.clone(),
                        shared_output: shared_output.clone(),
                        submit_id: datas.submit_id,
                    });

                    // Increment queue length when adding to queue
                    self.queue_length.fetch_add(1, Ordering::Acquire);
                    crate::metrics::counters::set_queue_length(
                        self.queue_length.load(Ordering::Acquire),
                    );

                    self.update_throughput(task_bytes as u64);
                }
            }

            // Chunk-size tuning (delegated to tuner)
            {
                let active_cores = self.active_count.load(Ordering::Acquire);
                let queue_length = self.queue_length.load(Ordering::Acquire);
                let current_throughput = self.get_current_throughput();
                let current_chunk_size = self.optimal_chunk_size.load(Ordering::Acquire);
                let (optimal_chunk_size, throughput_trend) = {
                    let mut history = self.throughput_history.write().unwrap();
                    crate::ingest::tuner::tune_chunk_size(
                        current_chunk_size,
                        active_cores,
                        queue_length,
                        self.num_cpus,
                        self.max_queue_length,
                        current_throughput,
                        self.max_chunk_size,
                        Self::read_mem_available_mib().map(|v| v as usize),
                        &mut history,
                    )
                };
                if current_chunk_size != optimal_chunk_size {
                    let trend_label = if throughput_trend > 0.0 {
                        "rising"
                    } else if throughput_trend < 0.0 {
                        "falling"
                    } else {
                        "stable"
                    };
                    info!(
                        "Optimising chunk size: active_cores: {}, active_tasks: {}, throughput: {}/s ({}), chunk_size: {} -> {}, adjustment: {:.2}x",
                        active_cores,
                        queue_length,
                        Helpers::human_readable_size(current_throughput),
                        trend_label,
                        Helpers::human_readable_size(current_chunk_size as u64),
                        Helpers::human_readable_size(optimal_chunk_size as u64),
                        (optimal_chunk_size as f64) / (current_chunk_size as f64)
                    );
                    self.optimal_chunk_size
                        .store(optimal_chunk_size, Ordering::SeqCst);
                }
            }

            // Self-tune concurrency targets based on queue pressure and active cores
            {
                let active = self.active_count.load(Ordering::Acquire);
                let queued = self.queue_length.load(Ordering::Acquire);
                let capacity = self.num_cpus;
                let pressure = (queued as f64) / ((self.max_queue_length as f64).max(1.0));

                // Delegate periodic tuning to tuner
                crate::ingest::tuner::tick(active, capacity, queued, pressure);
            }

            // Get current metrics for logging
            let current_queue_length = self.queue_length.load(Ordering::Acquire);
            let current_active_threads = self.active_count.load(Ordering::Acquire);
            let queued_tasks = self.task_queue.read().unwrap().len();

            // Export runtime state for summary logging and metrics payload
            crate::metrics::counters::set_active_threads(current_active_threads);
            crate::metrics::counters::set_queue_length(current_queue_length);

            info!("Queueing {} ingest tasks of {} ({} tasks in queue, {}/{} active threads, {} tasks waiting)",
                ingest_batches.tasks.len(),
                Helpers::human_readable_size(batch_bytes as u64),
                current_queue_length,
                current_active_threads,
                self.num_cpus,
                queued_tasks
            );
            if Config::log_wal_enabled() {
                let outstanding = self.outstanding_bytes.load(Ordering::Acquire);
                let budget = Self::autotuned_ingest_admission_budget_bytes(self.num_cpus);
                info!(
                    "ingest WAL pressure (post-submit): outstanding_bytes={} admission_budget_bytes={}",
                    outstanding, budget
                );
            }

            // Return throughput metrics
            ThroughputMetrics {
                bytes_per_second: self.get_current_throughput(),
                active_cores: current_active_threads,
                queue_length: current_queue_length,
                optimal_chunk_size: self.optimal_chunk_size.load(Ordering::Acquire),
            }
        }
    }

    fn process_batch(
        datas: &Arc<Vec<IngestBatch>>,
        offset_db_clone: &Arc<Offsets>,
        handle: runtime::Handle,
        _shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
        submit_id: u64,
    ) {
        if !Self::wait_for_data_dir_capacity() {
            if submit_id != 0 {
                crate::buffer::wal_writer::fail_request(
                    submit_id,
                    "DATA_DIR capacity exhausted before ingest task could write WAL",
                );
            }
            return;
        }
        let _guard = handle.enter();

        if offset_db_clone.is_remote() {
            panic!("host ingest must use local offsets; runtime sources submit payloads through SourceSyncContext");
        }

        let allowed_values = Config::get_partition_allowed_values();

        PARTITION_ALLOWED_VALUES_CACHE.with(|cache| {
            let mut w = cache.write().unwrap();
            if w.is_empty() {
                w.extend(cleaned_partition_allowed_values(&allowed_values));
            }
        });

        let _default_schema_hash = format!("{:?}", md5::compute(Helpers::random_str(10)));

        let transform_snap = IngestTransformSnapshot::capture();
        let flatten = transform_snap.flatten;

        let data_dir = Config::get_data_dir();
        let _output_dir = format!("{}/ingest_buffer", data_dir);

        let _aprox_now = SystemTime::now();

        let _updated_schema = "no".to_string();

        if Config::debug_enabled() || Config::log_wal_enabled() {
            let input_bytes: usize = datas.iter().map(|batch| batch.bytes).sum();
            let input_sample: Vec<String> = datas
                .iter()
                .take(3)
                .map(|batch| {
                    format!(
                        "{}:{}@{}",
                        batch.offset_key.namespace, batch.offset_key.partition, batch.bytes
                    )
                })
                .collect();
            info!(
                "Ingest: process_batch start input_batches={} input_bytes={} sample_offsets={:?}",
                datas.len(),
                input_bytes,
                input_sample
            );
        }

        let mut bytes: u64 = 0;
        let mut latest_timestamp: i64 = 0;
        let mut i: u64 = 0;
        let mut _j = 0;
        let x = 0;

        let primary_sink_ref = Config::get_pipeline_output_sink_ref();
        let deadletter_sink_ref = Config::get_pipeline_deadletters_ref();
        if deadletter_sink_ref.is_some() {
            deadletter::ensure_namespace_registered();
        }
        let mut dl_records: Vec<DeadletterRecord> = Vec::new();
        let mut dl_offsets: HashMap<OffsetKey, u64> = HashMap::new();
        let mut batch_line: u64;

        let format = match Config::get_pipeline_input_plugin_config() {
            Ok(plugin) => plugin.input_format(),
            Err(_) => Default::default(),
        };

        let entity_field_dot = transform_snap.record_field_path.clone();

        let mut buf: HashMap<(String, String, String, Option<i64>, String), IngestBufferBatch> =
            HashMap::with_capacity(32);
        // Temporary storage for raw JSON records prior to Arrow batch building
        let mut raw_values: HashMap<
            (String, String, String, Option<i64>, String),
            Vec<IngestRecord>,
        > = HashMap::with_capacity(32);
        // Per-partition CDC row metadata accumulated alongside raw_values
        let mut cdc_row_buf: HashMap<
            (String, String, String, Option<i64>, String),
            Vec<crate::plugins::cdc::WalRowMeta>,
        > = HashMap::new();
        let mut exact_partitions: HashMap<IngestPartitionKey, ExactArrowPartition> =
            HashMap::with_capacity(32);
        let mut exact_plan_cache: HashMap<(String, String), Arc<ExactArrowPlan>> =
            HashMap::with_capacity(8);
        #[cfg(test)]
        let use_exact_arrow = !crate::metrics::ingest_profile::FORCE_LEGACY_INGEST_FOR_BENCHMARK
            .load(std::sync::atomic::Ordering::Relaxed);
        #[cfg(not(test))]
        let use_exact_arrow = true;

        let pipeline_name_cached = Config::get_pipeline_name();
        let mut schema_hash_cache: HashMap<String, SchemaHash> = HashMap::new();
        for ingest_batch in datas.iter() {
            bytes += ingest_batch.data.len() as u64;

            let batch_namespace_override: String = ingest_batch
                .namespace
                .clone()
                .unwrap_or_else(|| pipeline_name_cached.clone());

            let offset_snapshot = offset_db_clone.snapshot_value(&ingest_batch.offset_key);
            let is_cdc_batch = ingest_batch.cdc_rows().is_some();
            let track_position = is_cdc_batch || ingest_batch.offset_pos.is_some();

            let use_ndjson_stream = ndjson_object_stream_eligible(
                format,
                &ingest_batch.data,
                &entity_field_dot,
                is_cdc_batch,
            );

            let flatten_started = Instant::now();
            let flatten_iter = if use_ndjson_stream {
                IngestFlattenIter::from_ndjson(&ingest_batch.data)
            } else {
                let decode_started = Instant::now();
                let mut records: Vec<Value> = match decode_records(format, &ingest_batch.data) {
                    Ok(records) => records,
                    Err(err) => {
                        ingest_profile::add_decode_ns(decode_started.elapsed().as_nanos() as u64);
                        let decode_offset_pos = ingest_batch.data.lines().count().max(1) as u64;
                        dl_records.push(DeadletterRecord {
                            namespace: Config::get_pipeline_name(),
                            record: ingest_batch.data.clone(),
                            error: err.to_string(),
                            failure_code: "INPUT_FORMAT".to_string(),
                            event_time: None,
                            source_uri: ingest_batch.source_uri.clone(),
                            offset_key: format!(
                                "{}:{}",
                                ingest_batch.offset_key.namespace,
                                ingest_batch.offset_key.partition
                            ),
                            offset_pos: 1,
                        });
                        dl_offsets
                            .entry(ingest_batch.offset_key.clone())
                            .and_modify(|pos| *pos = (*pos).max(decode_offset_pos))
                            .or_insert(decode_offset_pos);
                        continue;
                    }
                };
                ingest_profile::add_decode_ns(decode_started.elapsed().as_nanos() as u64);
                if Config::debug_enabled() {
                    debug!(
                        "Ingest: decoded {} records for namespace={} format={}",
                        records.len(),
                        batch_namespace_override,
                        format.as_str()
                    );
                }

                if !entity_field_dot.is_empty() {
                    records = match Helpers::process_values(&records, &entity_field_dot) {
                        Some(records) => records,
                        None => Vec::new(),
                    };
                }

                if let Some(cdc_rows) = ingest_batch.cdc_rows() {
                    let logical_count = count_logical_ingest_records(&records);
                    if cdc_rows.len() != logical_count {
                        panic!(
                            "CDC row metadata for {}:{} has {} entries but decoded {} logical records",
                            ingest_batch.offset_key.namespace,
                            ingest_batch.offset_key.partition,
                            cdc_rows.len(),
                            logical_count
                        );
                    }
                }

                IngestFlattenIter::from_decoded(records)
            };
            ingest_profile::add_unwrap_ns(flatten_started.elapsed().as_nanos() as u64);

            batch_line = 0;
            let mut cdc_row_idx: usize = 0;

            let mut metadata_snapshot = METADATA.load().clone();
            let mut metadata_versions: HashMap<String, u64> = HashMap::new();
            let fixed_batch_namespace = ingest_batch.namespace.is_some();
            let mut cached_exact_partition_key: Option<IngestPartitionKey> = None;

            for item in flatten_iter {
                match item {
                    IngestFlattenItem::NotObject { line, offset_pos } => {
                        dl_records.push(DeadletterRecord {
                            namespace: Config::get_pipeline_name(),
                            record: line,
                            error: "Source data is not an object or array".to_string(),
                            failure_code: "INPUT_FORMAT".to_string(),
                            event_time: None,
                            source_uri: ingest_batch.source_uri.clone(),
                            offset_key: format!(
                                "{}:{}",
                                ingest_batch.offset_key.namespace,
                                ingest_batch.offset_key.partition
                            ),
                            offset_pos,
                        });
                        merge_dl_offset_for_key(
                            &mut dl_offsets,
                            &ingest_batch.offset_key,
                            offset_pos,
                        );
                        continue;
                    }
                    IngestFlattenItem::ParseError {
                        line,
                        offset_pos,
                        error,
                    } => {
                        dl_records.push(DeadletterRecord {
                            namespace: Config::get_pipeline_name(),
                            record: line,
                            error,
                            failure_code: "INPUT_FORMAT".to_string(),
                            event_time: None,
                            source_uri: ingest_batch.source_uri.clone(),
                            offset_key: format!(
                                "{}:{}",
                                ingest_batch.offset_key.namespace,
                                ingest_batch.offset_key.partition
                            ),
                            offset_pos,
                        });
                        merge_dl_offset_for_key(
                            &mut dl_offsets,
                            &ingest_batch.offset_key,
                            offset_pos,
                        );
                        continue;
                    }
                    IngestFlattenItem::Row {
                        line,
                        value: mut record,
                    } => {
                        batch_line = line;
                        cdc_row_idx += 1;

                        if record.is_null()
                            || (record.is_object() && record.as_object().unwrap().is_empty())
                            || (record.is_array() && record.as_array().unwrap().is_empty())
                        {
                            let line_str =
                                match ingest_batch.data.lines().nth(batch_line as usize - 1) {
                                    Some(line) => line,
                                    None => "",
                                };

                            let empty_offset_pos = ingest_batch.offset_pos_for_line(batch_line);
                            dl_records.push(DeadletterRecord {
                                namespace: Config::get_pipeline_name(),
                                record: line_str.to_string(),
                                error: "Source data is empty".to_string(),
                                failure_code: "EMPTY_RECORD".to_string(),
                                event_time: None,
                                source_uri: ingest_batch.source_uri.clone(),
                                offset_key: format!(
                                    "{}:{}",
                                    ingest_batch.offset_key.namespace,
                                    ingest_batch.offset_key.partition
                                ),
                                offset_pos: empty_offset_pos,
                            });
                            merge_dl_offset_for_key(
                                &mut dl_offsets,
                                &ingest_batch.offset_key,
                                empty_offset_pos,
                            );

                            continue;
                        }

                        let offset_pos = ingest_batch.offset_pos_for_line(batch_line);

                        if should_ingest_at_offset(
                            offset_snapshot.as_ref(),
                            is_cdc_batch,
                            track_position,
                            offset_pos,
                        ) {
                            i += 1;

                            let partition_started = Instant::now();
                            let mut namespace_scratch: Option<String> = None;
                            let skpr_partition_owned;
                            let skpr_namespace: &str = if fixed_batch_namespace
                                || transform_snap.skip_namespace_field_parse
                            {
                                &batch_namespace_override
                            } else {
                                namespace_scratch =
                                    Some(storage_namespace(&PARSE_NAMESPACE_CACHE.with(|cache| {
                                        let mut namespace_cache = cache.write().unwrap();
                                        Helpers::parse_namespace_field_with_fields(
                                            &record,
                                            batch_namespace_override.clone(),
                                            &mut namespace_cache,
                                            &transform_snap.namespace_fields,
                                        )
                                    })));
                                namespace_scratch.as_deref().unwrap()
                            };

                            let skpr_partition: &str = if transform_snap.skip_partition_parse {
                                EMPTY_PARTITION
                            } else {
                                skpr_partition_owned =
                                    PARTITION_ALLOWED_VALUES_CACHE.with(|cache| {
                                        Helpers::parse_partition_field_with_fields(
                                            &record,
                                            &cache.read().unwrap(),
                                            &transform_snap.partition_fields,
                                        )
                                    });
                                &skpr_partition_owned
                            };
                            let skpr_time = if transform_snap.skip_time_parse {
                                None
                            } else {
                                Helpers::parse_time_field_with_fields(
                                    &record,
                                    &transform_snap.time_fields,
                                )
                            };

                            let mut skpr_time_bucket: Option<i64> = None;

                            if let Some(event_time) = skpr_time {
                                skpr_time_bucket =
                                    Some(BufferChunker::event_time_bucket(event_time));

                                if event_time > latest_timestamp {
                                    latest_timestamp = event_time;
                                }
                            }
                            ingest_profile::add_partition_ns(
                                partition_started.elapsed().as_nanos() as u64,
                            );

                            apply_transform_inject_fields(&mut record);

                            refresh_metadata_snapshot_for_namespace(
                                skpr_namespace,
                                &mut metadata_snapshot,
                                &mut metadata_versions,
                            );

                            let mut used_exact_arrow = false;
                            if use_exact_arrow && !is_cdc_batch {
                                if let Some(ns_metadata) =
                                    metadata_snapshot.metadata.get(skpr_namespace)
                                {
                                    let schema_hash = resolve_partition_schema(
                                        skpr_namespace,
                                        flatten,
                                        &mut schema_hash_cache,
                                    );
                                    if let Some(plan) = resolve_cached_exact_plan(
                                        &mut exact_plan_cache,
                                        skpr_namespace,
                                        &schema_hash.hash,
                                        ns_metadata.fields.as_ref(),
                                        flatten,
                                        schema_hash.schema.clone(),
                                    ) {
                                        let needs_new_key = cached_exact_partition_key
                                            .as_ref()
                                            .map(|cached| {
                                                cached.0 != primary_sink_ref
                                                    || cached.1 != skpr_namespace
                                                    || cached.2 != skpr_partition
                                                    || cached.3 != skpr_time_bucket
                                                    || cached.4 != schema_hash.hash
                                            })
                                            .unwrap_or(true);
                                        if needs_new_key {
                                            cached_exact_partition_key = Some((
                                                primary_sink_ref.clone(),
                                                skpr_namespace.to_string(),
                                                skpr_partition.to_string(),
                                                skpr_time_bucket.clone(),
                                                schema_hash.hash.clone(),
                                            ));
                                        }
                                        let partition_key = cached_exact_partition_key
                                            .as_ref()
                                            .expect("partition key");
                                        let append_started = Instant::now();
                                        let append_result = append_row_to_exact_partition(
                                            &mut exact_partitions,
                                            partition_key,
                                            plan,
                                            &record,
                                            ns_metadata.fields.as_ref(),
                                            1024,
                                        );
                                        ingest_profile::add_exact_append_ns(
                                            append_started.elapsed().as_nanos() as u64,
                                        );
                                        match append_result {
                                            Ok(()) => {
                                                ingest_profile::add_exact_arrow_rows(1);
                                                let row_cdc_meta =
                                                    ingest_batch.cdc_rows.as_ref().and_then(
                                                        |rows| rows.get(cdc_row_idx - 1).cloned(),
                                                    );
                                                track_partition_offset_with_key(
                                                    &mut buf,
                                                    &primary_sink_ref,
                                                    partition_key,
                                                    &schema_hash,
                                                    skpr_namespace,
                                                    skpr_partition,
                                                    &skpr_time_bucket,
                                                    &ingest_batch.offset_key,
                                                    offset_pos,
                                                    row_cdc_meta,
                                                    &mut cdc_row_buf,
                                                );
                                                used_exact_arrow = true;
                                                _j += 1;
                                            }
                                            Err(_) => {
                                                ingest_profile::add_exact_arrow_fallback_rows(1);
                                            }
                                        }
                                    }
                                }
                            }

                            if used_exact_arrow {
                                continue;
                            }

                            let source = SourceRecord::new(record);
                            let fast_started = Instant::now();
                            let msg = match metadata_snapshot.metadata.get(skpr_namespace) {
                                Some(metadata) => fast_path_ingest(
                                    source.inner(),
                                    metadata.fields.as_ref(),
                                    &skpr_namespace,
                                    flatten,
                                ),
                                None => Err(format!(
                                    "Failed to find metadata for namespace: {}",
                                    skpr_namespace
                                )
                                .into()),
                            };
                            ingest_profile::add_fast_path_ns(
                                fast_started.elapsed().as_nanos() as u64
                            );

                            let record_value = match msg {
                                Ok(msg) => msg,
                                Err(_err) => {
                                    let slow_started = Instant::now();
                                    // Simple fallback: single-record slow path
                                    let slow_result =
                                        slow_ingest_blocking(&skpr_namespace, &source, flatten);
                                    ingest_profile::add_slow_path_ns(
                                        slow_started.elapsed().as_nanos() as u64,
                                    );
                                    match slow_result {
                                        Ok(v) => v,
                                        Err(e) => {
                                            if Config::debug_enabled() {
                                                debug!(
                                                    "Ingest: slow-path failed ns={} err={}",
                                                    skpr_namespace, e
                                                );
                                            }
                                            dl_records.push(DeadletterRecord {
                                                namespace: skpr_namespace.to_string(),
                                                record: source.inner().to_string(),
                                                error: e.to_string(),
                                                failure_code: "EVOLUTION_SLOW_PATH".to_string(),
                                                event_time: skpr_time,
                                                source_uri: ingest_batch.source_uri.clone(),
                                                offset_key: format!(
                                                    "{}:{}",
                                                    ingest_batch.offset_key.namespace,
                                                    ingest_batch.offset_key.partition
                                                ),
                                                offset_pos,
                                            });
                                            merge_dl_offset_for_key(
                                                &mut dl_offsets,
                                                &ingest_batch.offset_key,
                                                offset_pos,
                                            );
                                            Value::Null
                                        }
                                    }
                                }
                            };

                            // Skip records that failed both fast-path and slow-path evolution
                            if record_value.is_null() {
                                if Config::debug_enabled() {
                                    debug!(
                                        "Ingest: record dropped after evolution ns={} (null)",
                                        skpr_namespace
                                    );
                                }
                                continue;
                            }

                            let normalized = NormalizedRecord::new(record_value);
                            ingest_profile::add_legacy_normalized_rows(1);
                            let row_cdc_meta = ingest_batch
                                .cdc_rows
                                .as_ref()
                                .and_then(|rows| rows.get(cdc_row_idx - 1).cloned());
                            enqueue_legacy_record(
                                &mut buf,
                                &mut raw_values,
                                &mut cdc_row_buf,
                                &mut schema_hash_cache,
                                &primary_sink_ref,
                                flatten,
                                &skpr_namespace,
                                &skpr_partition,
                                &skpr_time_bucket,
                                source,
                                normalized,
                                &ingest_batch.offset_key,
                                offset_pos,
                                row_cdc_meta,
                            );

                            _j += 1;

                            // Stats tailer removed; no per-record stats emission
                        }
                    }
                }
            }
            if Config::debug_enabled() {
                debug!("Ingest: buffered {} records so far", i);
            }
        }

        // Update metrics early to reflect decode throughput before WAL flush
        let update_result = std::panic::catch_unwind(|| {
            metrics_hot::add_ingested_slow(x);
            metrics_hot::add_messages(i);
            metrics_hot::add_source_bytes(bytes);
            metrics_hot::update_latest_timestamp_max(latest_timestamp as u64);
        });

        if let Err(e) = update_result {
            warn!("Warning: Could not update metrics - {:?}", e);
        }

        // Serialize each entry's normalized JSON into Arrow RecordBatches eagerly
        // (all-or-nothing per batch).
        //
        // Critical invariant: schema discovery/evolution is an ingest-time
        // operation, serialized per namespace by the slow-ingest worker. By the
        // time records reach this point they must already be normalized against
        // a monotonic, backward-compatible schema published from the evolved
        // metadata. This path must not discover schema; a serialization failure
        // here means ingest-time normalization/schema publication missed that
        // invariant and should be handled as data-plane failure, not compaction
        // recovery.
        let arrow_started = Instant::now();
        for (k, entry) in buf.iter_mut() {
            let mut batches: Vec<RecordBatch> = Vec::new();
            if let Some(exact) = exact_partitions.remove(k) {
                if !exact.builders.is_empty() {
                    let finish_started = Instant::now();
                    match exact.builders.finish() {
                        Ok(batch) => batches.push(batch),
                        Err(err) => {
                            warn!(
                                "Exact Arrow finish failed for ns={}: {}",
                                entry._namespace, err
                            );
                        }
                    }
                    ingest_profile::add_exact_finish_ns(finish_started.elapsed().as_nanos() as u64);
                }
            }

            let records_vec = raw_values.remove(k).unwrap_or_default();
            if !records_vec.is_empty() {
                let all_indices: Vec<usize> = (0..records_vec.len()).collect();
                let (legacy_batches, failed_indices) =
                    bisect_serialize_indices(entry.schema.clone(), &records_vec, &all_indices);

                if failed_indices.is_empty() {
                    batches.extend(legacy_batches);
                    if let Some(rows) = cdc_row_buf.remove(k) {
                        if !rows.is_empty() {
                            entry.cdc_rows = Some(rows);
                        }
                    }
                } else {
                    let failed_set: std::collections::HashSet<usize> =
                        failed_indices.iter().copied().collect();
                    let success_indices: Vec<usize> = all_indices
                        .into_iter()
                        .filter(|i| !failed_set.contains(i))
                        .collect();

                    batches.extend(legacy_batches);

                    if !batches.is_empty() {
                        if let Some(max_pos) = max_offset_pos_among(&records_vec, &success_indices)
                        {
                            set_entry_offsets_max(entry, max_pos);
                        }
                        if let Some(all_cdc) = cdc_row_buf.remove(k) {
                            if all_cdc.len() == records_vec.len() {
                                let filtered: Vec<_> = success_indices
                                    .iter()
                                    .map(|&i| all_cdc[i].clone())
                                    .collect();
                                if !filtered.is_empty() {
                                    entry.cdc_rows = Some(filtered);
                                }
                            }
                        }
                    } else {
                        cdc_row_buf.remove(k);
                    }

                    let skpr_namespace = entry._namespace.clone();
                    let off_key_str = match entry.offsets.iter().next() {
                        Some((ok, _)) => format!("{}:{}", ok.namespace, ok.partition),
                        None => String::new(),
                    };
                    let arrow_err_msg =
                        "Arrow serialization invariant failed after ingest-time evolution"
                            .to_string();
                    warn!(
                        "{} (ns={}, failed={}, total={})",
                        arrow_err_msg,
                        skpr_namespace,
                        failed_indices.len(),
                        records_vec.len()
                    );
                    for &idx in &failed_indices {
                        let rec = &records_vec[idx];
                        dl_records.push(DeadletterRecord {
                            namespace: skpr_namespace.clone(),
                            record: rec.source.inner().to_string(),
                            error: arrow_err_msg.clone(),
                            failure_code: "ARROW_SERIALIZE".to_string(),
                            event_time: rec._time,
                            source_uri: String::new(),
                            offset_key: off_key_str.clone(),
                            offset_pos: rec._offset_pos,
                        });
                    }
                    merge_dl_offsets_for_records(
                        &mut dl_offsets,
                        entry,
                        &records_vec,
                        &failed_indices,
                    );
                    if Config::debug_enabled() {
                        debug!(
                            "Serialize bisect: ns={} deadlettered={} succeeded={}",
                            entry._namespace,
                            failed_indices.len(),
                            success_indices.len()
                        );
                    }
                }
            }

            if batches.is_empty() {
                if let Some(rows) = cdc_row_buf.remove(k) {
                    if !rows.is_empty() {
                        entry.cdc_rows = Some(rows);
                    }
                }
                continue;
            }

            match merge_record_batches(entry.schema.clone(), batches) {
                Ok(merged) => {
                    entry.record_batches = Some(merged);
                }
                Err(err) => {
                    warn!(
                        "Failed to merge record batches for ns={}: {}",
                        entry._namespace, err
                    );
                }
            }
        }
        ingest_profile::add_arrow_json_ns(arrow_started.elapsed().as_nanos() as u64);

        for (k, entry) in buf.iter_mut() {
            if entry.record_batches.is_some() {
                continue;
            }
            if let Some(rows) = cdc_row_buf.remove(k) {
                if !rows.is_empty() {
                    entry.cdc_rows = Some(rows);
                }
            }
        }

        // Flush accumulated deadletter records into buf as a regular WAL entry.
        // The deadletter batch carries dl_offsets so the source offsets are
        // committed only when the WAL segment is durably flushed, matching the
        // guarantees for normal data.
        if !dl_records.is_empty() {
            let dl_count = dl_records.len();
            metrics_hot::add_deadletters(dl_count as u64);
            let mut dl_offsets_committed = false;
            if let Some(deadletter_sink_ref) = deadletter_sink_ref.clone() {
                let dl_ns = deadletter::table_name();
                let dl_schema: SchemaRef = deadletter::arrow_schema();
                let dl_time_bucket = BufferChunker::event_time_bucket(
                    SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap()
                        .as_secs() as i64,
                );
                if let Some(batch) = deadletter::build_batch(&dl_records) {
                    let key = (
                        deadletter_sink_ref.clone(),
                        dl_ns.clone(),
                        String::new(),
                        Some(dl_time_bucket),
                        "0".to_string(),
                    );
                    buf.insert(
                        key,
                        IngestBufferBatch {
                            offsets: std::mem::take(&mut dl_offsets),
                            sink_ref: deadletter_sink_ref,
                            _namespace: dl_ns,
                            _partition: String::new(),
                            _time: Some(dl_time_bucket),
                            _schema_fingerprint: String::new(),
                            schema: dl_schema,
                            record_batches: Some(vec![batch]),
                            cdc_rows: None,
                        },
                    );
                    dl_offsets_committed = true;
                    warn!("Deadlettered {} records into WAL", dl_count);
                }
            } else {
                warn!(
                    "Discarded {} deadletter records because no deadletter sink is configured",
                    dl_count
                );
                let sample_limit = std::cmp::min(dl_count, 5);
                for (i, rec) in dl_records.iter().take(sample_limit).enumerate() {
                    warn!(
                        "deadletter[{}/{}]: failure_code={} error={} namespace={} record={}",
                        i + 1,
                        dl_count,
                        rec.failure_code,
                        rec.error,
                        rec.namespace,
                        &rec.record[..std::cmp::min(rec.record.len(), 200)],
                    );
                }
            }
            if !dl_offsets_committed && !dl_offsets.is_empty() {
                for (ok, pos) in dl_offsets.drain() {
                    let offset_key = OffsetKey {
                        namespace: ok.namespace.clone(),
                        partition: ok.partition.clone(),
                    };
                    offset_db_clone.set(&offset_key, OffsetTypes::Closed, 1);
                    offset_db_clone.set(&offset_key, OffsetTypes::Position, pos);
                }
            }
        }

        // Batch write: aggregate all partition batches and enqueue once for the WAL writer.
        let all_batches: Vec<IngestBufferBatch> = buf.into_values().collect();
        if Config::debug_enabled() {
            debug!(
                "Ingest: final buffered partition count before WAL flush = {}",
                all_batches.len()
            );
        }
        if !all_batches.is_empty() {
            if Config::debug_enabled() || Config::log_wal_enabled() {
                let total_batches: usize = all_batches
                    .iter()
                    .map(|e| e.record_batches.as_ref().map(|v| v.len()).unwrap_or(0))
                    .sum();
                let total_offsets: usize =
                    all_batches.iter().map(|batch| batch.offsets.len()).sum();
                let partition_sample: Vec<String> = all_batches
                    .iter()
                    .take(3)
                    .map(|batch| {
                        format!(
                            "{}/{}/offsets={}",
                            batch._namespace,
                            batch._partition,
                            batch.offsets.len()
                        )
                    })
                    .collect();
                info!(
                    "Ingest: enqueueing {} record batches across {} partitions with {} offsets sample_partitions={:?}",
                    total_batches,
                    all_batches.len(),
                    total_offsets,
                    partition_sample
                );
            }
            let arrow_bytes = all_batches
                .iter()
                .flat_map(|batch| batch.record_batches.as_ref().into_iter().flatten())
                .map(|batch| batch.get_array_memory_size())
                .sum::<usize>();
            let raw_bytes = datas.iter().map(|batch| batch.bytes).sum::<usize>();
            let (done_tx, _done_rx) = tokio::sync::oneshot::channel();
            let unit = crate::buffer::wal_writer::WalCommitUnit {
                submit_id,
                batches: all_batches,
                offsets_db: offset_db_clone.clone(),
                raw_bytes,
                arrow_bytes,
                done: done_tx,
            };
            let wal_started = Instant::now();
            if let Err(err) = handle.block_on(crate::buffer::wal_writer::submit(unit)) {
                error!("Ingest: WAL writer enqueue failed: {}", err);
                panic!("WAL writer enqueue failed: {err}");
            }
            ingest_profile::add_wal_enqueue_ns(wal_started.elapsed().as_nanos() as u64);
        } else if submit_id != 0 {
            crate::buffer::wal_writer::complete_request_without_wal(submit_id);
        }
    }

    /// Benchmark/test entrypoint for local ingest profiling.
    #[cfg(test)]
    pub fn run_process_batch_for_benchmark(
        datas: &[IngestBatch],
        offset_db: &Arc<Offsets>,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
        submit_id: u64,
    ) {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("benchmark runtime");
        crate::buffer::wal_writer::start(1);
        let handle = rt.handle().clone();
        Self::process_batch(
            &Arc::new(datas.to_vec()),
            offset_db,
            handle,
            shared_output,
            submit_id,
        );
        rt.block_on(async {
            let _ = crate::buffer::wal_writer::flush_and_drain(offset_db.clone()).await;
        });
    }

    pub fn prepare_arrow_schema_with_metadata(
        skpr_namespace: &str,
        metadata: &HashMap<String, Metadata>,
        flatten: bool,
    ) -> Result<Arc<arrow::datatypes::Schema>, ArrowError> {
        let mut _arrow_schema: Result<datatypes::Schema, ArrowError> =
            Ok(datatypes::Schema::empty());
        let mut _schema_ref = Arc::new(datatypes::Schema::empty());

        let skpr_metadata = metadata.get(skpr_namespace);

        let mut output_metadata: OutputMetadata = OutputMetadata::new();

        if let Some(metadata_for_namespace) = skpr_metadata {
            if flatten {
                output_metadata = OutputMetadata::from_flatterened_metadata(metadata_for_namespace);
            } else {
                output_metadata = OutputMetadata::from_metadata(metadata_for_namespace);
            }
        }

        _arrow_schema = convert_skippr_to_arrow(output_metadata.fields);

        _schema_ref = Arc::new(_arrow_schema.unwrap());

        // Compute new schema hash for change detection using a stable fingerprint
        let new_hash = crate::converters::skippr_arrow::stable_schema_fingerprint(&_schema_ref);

        // Publish schema via ArcSwap per-namespace
        use dashmap::mapref::entry::Entry;
        let mut did_update_schema = false;
        match ARROW_SCHEMA.entry(skpr_namespace.to_string()) {
            Entry::Occupied(o) => {
                let prev = o.get().load();
                let prev_hash = crate::converters::skippr_arrow::stable_schema_fingerprint(&prev);
                if prev_hash != new_hash {
                    // Only accept monotonic (superset) schema changes; ignore regressions
                    if crate::converters::skippr_arrow::is_schema_superset(&_schema_ref, &prev) {
                        o.get().store(_schema_ref.clone());
                        did_update_schema = true;
                        if Config::debug_enabled() {
                            debug!(
                                "Arrow schema updated for namespace {}: {} -> {}",
                                skpr_namespace, prev_hash, new_hash
                            );
                        }
                    } else if Config::debug_enabled() {
                        warn!("Arrow schema change rejected (non-superset) for namespace {}: {} !-> {}", skpr_namespace, prev_hash, new_hash);
                    }
                } else if Config::debug_enabled() {
                    debug!(
                        "Arrow schema unchanged for namespace {}: {}",
                        skpr_namespace, new_hash
                    );
                }
            }
            Entry::Vacant(v) => {
                v.insert(arc_swap::ArcSwap::from(_schema_ref.clone()));
                did_update_schema = true;
                if Config::debug_enabled() {
                    debug!(
                        "Arrow schema initialized for namespace {}: {}",
                        skpr_namespace, new_hash
                    );
                }
            }
        }

        // Refresh default nested message template for fast ingest determinism
        if did_update_schema {
            if let Some(ns_meta) = metadata.get(skpr_namespace) {
                let template = create_default_nested_message(&ns_meta.fields);
                DEFAULT_NESTED_MESSAGE.insert(skpr_namespace.to_string(), Arc::new(template));
            }
        }

        // Bump schema version IMMEDIATELY after updating ARROW_SCHEMA and the
        // template so that load_stable_schema_hash's seqlock never pairs the
        // new schema with the old version number.  The output schema sync is
        // fire-and-forget and must come *after* the version bump.
        if did_update_schema {
            let entry = ARROW_SCHEMA_VERSION
                .entry(skpr_namespace.to_string())
                .or_insert_with(|| AtomicU64::new(0));
            entry.fetch_add(1, Ordering::Release);
            bump_pipeline_schema_version();
        }

        if did_update_schema {
            crate::helpers::configuration::Config::sync_output_schema_namespace(skpr_namespace);
        }

        // Mark schema as ready deterministically for this namespace
        SCHEMA_READY
            .entry(skpr_namespace.to_string())
            .or_insert_with(|| AtomicBool::new(true))
            .store(true, Ordering::Relaxed);

        Ok(_schema_ref)
    }

    // Version for read-only query context: builds/publishes Arrow schema without external side effects
    pub fn prepare_arrow_schema_with_metadata_for_query(
        skpr_namespace: &str,
        metadata: &HashMap<String, Metadata>,
        flatten: bool,
    ) -> Result<Arc<arrow::datatypes::Schema>, ArrowError> {
        let mut _arrow_schema: Result<datatypes::Schema, ArrowError> =
            Ok(datatypes::Schema::empty());
        let mut _schema_ref = Arc::new(datatypes::Schema::empty());

        let skpr_metadata = metadata.get(skpr_namespace);

        let mut output_metadata: OutputMetadata = OutputMetadata::new();

        if let Some(metadata_for_namespace) = skpr_metadata {
            if flatten {
                output_metadata = OutputMetadata::from_flatterened_metadata(metadata_for_namespace);
            } else {
                output_metadata = OutputMetadata::from_metadata(metadata_for_namespace);
            }
        }

        _arrow_schema = convert_skippr_to_arrow(output_metadata.fields);

        _schema_ref = Arc::new(_arrow_schema.unwrap());

        // Compute new schema hash for change detection using a stable fingerprint
        let new_hash = crate::converters::skippr_arrow::stable_schema_fingerprint(&_schema_ref);

        // Publish schema via ArcSwap per-namespace
        use dashmap::mapref::entry::Entry;
        let mut did_update_schema = false;
        match ARROW_SCHEMA.entry(skpr_namespace.to_string()) {
            Entry::Occupied(o) => {
                let prev = o.get().load();
                let prev_hash = crate::converters::skippr_arrow::stable_schema_fingerprint(&prev);
                if prev_hash != new_hash {
                    if crate::converters::skippr_arrow::is_schema_superset(&_schema_ref, &prev) {
                        o.get().store(_schema_ref.clone());
                        did_update_schema = true;
                    }
                }
            }
            Entry::Vacant(v) => {
                v.insert(arc_swap::ArcSwap::from(_schema_ref.clone()));
                did_update_schema = true;
            }
        }

        // Refresh default nested message template for fast ingest determinism
        if did_update_schema {
            if let Some(ns_meta) = metadata.get(skpr_namespace) {
                let template = create_default_nested_message(&ns_meta.fields);
                DEFAULT_NESTED_MESSAGE.insert(skpr_namespace.to_string(), Arc::new(template));
            }
        }

        // Bump schema version for this namespace AFTER updating schema and template
        if did_update_schema {
            let entry = ARROW_SCHEMA_VERSION
                .entry(skpr_namespace.to_string())
                .or_insert_with(|| AtomicU64::new(0));
            entry.fetch_add(1, Ordering::Relaxed);
            bump_pipeline_schema_version();
        }

        // Mark schema as ready deterministically for this namespace
        SCHEMA_READY
            .entry(skpr_namespace.to_string())
            .or_insert_with(|| AtomicBool::new(true))
            .store(true, Ordering::Relaxed);

        Ok(_schema_ref)
    }
}

#[cfg(test)]
mod ndjson_flatten_iter_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ndjson_flatten_iter_yields_object_rows() {
        let payload = format!("{}\n", json!({"id": "row-1", "value": 1}));
        let mut iter = IngestFlattenIter::from_ndjson(&payload);
        let item = iter.next().expect("one row");
        assert!(matches!(item, IngestFlattenItem::Row { .. }));
        assert!(iter.next().is_none());
    }
}

#[cfg(test)]
mod empty_ingest_tasks_tests {
    use super::*;

    #[test]
    fn ingest_file_empty_tasks_in_discover_mode_does_not_panic() {
        let ingest = Ingest::new_for_execution(RuntimeExecutionMode::Discover);
        let empty_tasks = Arc::new(IngestTasks::new());

        let offsets = Arc::new(crate::helpers::offsets::Offsets::init().expect("offset DB init"));
        let noop: Box<dyn crate::plugins::DataSink + Send + Sync> =
            Box::new(crate::plugins::NoopOutputPlugin);
        let output = Arc::new(noop);

        ingest.ingest_file(&empty_tasks, &offsets, output);
    }
}

#[cfg(test)]
mod offset_gate_tests {
    use super::*;
    use zerocopy::U64;

    fn snap(line: u64, closed: u64) -> OffsetValue {
        OffsetValue {
            filesize: U64::new(0),
            line: U64::new(line),
            closed: U64::new(closed),
        }
    }

    #[test]
    fn closed_partition_skips_non_cdc() {
        let snap = snap(0, 1);
        assert!(!should_ingest_at_offset(Some(&snap), false, false, 1));
    }

    #[test]
    fn cdc_ignores_closed_gate() {
        let snap = snap(0, 1);
        assert!(should_ingest_at_offset(Some(&snap), true, true, 1));
    }

    #[test]
    fn position_tracked_when_explicit_offset_pos() {
        let snap = snap(10, 0);
        assert!(!should_ingest_at_offset(Some(&snap), false, true, 5));
        assert!(should_ingest_at_offset(Some(&snap), false, true, 11));
    }

    #[test]
    fn immutable_sources_ignore_line_without_track_position() {
        let snap = snap(10, 0);
        assert!(should_ingest_at_offset(Some(&snap), false, false, 1));
    }
}

#[cfg(test)]
mod transform_inject_fields_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merge_inject_fields_adds_configured_values() {
        let fields = HashMap::from([
            ("workspace_id".to_string(), json!("ws-1")),
            ("domain_id".to_string(), json!("dom-1")),
        ]);
        let mut record = json!({"event": "click"});
        merge_inject_fields(&mut record, &fields);
        assert_eq!(record["workspace_id"], "ws-1");
        assert_eq!(record["domain_id"], "dom-1");
        assert_eq!(record["event"], "click");
    }

    #[test]
    fn merge_inject_fields_preserves_existing_non_empty_values() {
        let fields = HashMap::from([("workspace_id".to_string(), json!("new"))]);
        let mut record = json!({"workspace_id": "old", "event": "click"});
        merge_inject_fields(&mut record, &fields);
        assert_eq!(record["workspace_id"], "old");
    }

    #[test]
    fn merge_inject_fields_fills_missing_domain() {
        let fields = HashMap::from([("domain".to_string(), json!("skippr.io"))]);
        let mut record = json!({"event": "click"});
        merge_inject_fields(&mut record, &fields);
        assert_eq!(record["domain"], "skippr.io");
    }

    #[test]
    fn merge_inject_fields_preserves_organic_serp_domain() {
        let fields = HashMap::from([
            ("workspace_id".to_string(), json!("ws-1")),
            ("domain_id".to_string(), json!("skippr.io")),
            ("domain".to_string(), json!("skippr.io")),
        ]);
        let mut record = json!({
            "keyword": "data pipeline",
            "domain": "snowflake.com",
            "url": "https://www.snowflake.com/en/data-cloud/"
        });
        merge_inject_fields(&mut record, &fields);
        assert_eq!(record["domain"], "snowflake.com");
        assert_eq!(record["domain_id"], "skippr.io");
        assert_eq!(record["workspace_id"], "ws-1");
    }
}

#[cfg(test)]
mod ingest_admission_tests {
    use super::*;
    #[test]
    fn admission_budget_s3_fits_shared_70pct_envelope() {
        let hint = 8_usize * 1024 * 1024 * 1024;
        let pool = crate::buffer::s3_wal_memory_budget::combined_pool_bytes(hint);
        let (adm, lru) = crate::buffer::s3_wal_memory_budget::split_s3_wal_memory_budget(hint);
        let ingest_adm = Ingest::admission_budget_for_tests(hint, true, 4);
        assert_eq!(ingest_adm, adm);
        assert!(
            adm.saturating_add(lru) <= pool,
            "admission={adm} lru={lru} pool={pool}"
        );
    }

    #[test]
    fn admission_budget_s3_is_looser_than_disk_for_same_hint() {
        let hint = 4_usize * 1024 * 1024 * 1024;
        let threads = 4;
        let disk = Ingest::admission_budget_for_tests(hint, false, threads);
        let s3 = Ingest::admission_budget_for_tests(hint, true, threads);
        assert!(
            s3 >= disk,
            "expected s3 WAL budget >= disk WAL budget, got disk={disk} s3={s3}"
        );
    }
}

#[cfg(test)]
mod data_dir_watermark_tests {
    use super::Ingest;

    const GB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn zero_high_watermark_disables_pause() {
        assert_eq!(Ingest::normalize_data_dir_watermarks(0, 80), None);
    }

    #[test]
    fn invalid_low_watermark_is_adjusted_below_high() {
        assert_eq!(
            Ingest::normalize_data_dir_watermarks(90, 95),
            Some((90, 85))
        );
    }

    #[test]
    fn valid_watermarks_are_preserved() {
        assert_eq!(
            Ingest::normalize_data_dir_watermarks(92, 80),
            Some((92, 80))
        );
    }

    #[test]
    fn should_block_on_percentage_even_with_plenty_of_absolute_free_space() {
        let min_free = 5 * GB;
        // 1 TB total, 50 GB free (~95% used) — above min_free but above 90% watermark.
        assert!(Ingest::data_dir_should_block(50 * GB, 95.0, 90, min_free));
    }

    #[test]
    fn should_block_on_absolute_floor() {
        let min_free = 5 * GB;
        assert!(Ingest::data_dir_should_block(4 * GB, 96.0, 90, min_free));
    }

    #[test]
    fn should_block_on_large_disk_above_high_watermark() {
        let min_free = 5 * GB;
        // 10 TB total, 800 GB free (~92% used).
        assert!(Ingest::data_dir_should_block(800 * GB, 92.0, 90, min_free));
    }

    #[test]
    fn should_not_block_when_below_high_watermark_and_above_min_free() {
        let min_free = 5 * GB;
        assert!(!Ingest::data_dir_should_block(100 * GB, 70.0, 90, min_free));
    }

    #[test]
    fn should_not_resume_until_both_absolute_and_percentage_clear() {
        let min_free = 5 * GB;
        // Enough absolute free space but usage still above low watermark.
        assert!(!Ingest::data_dir_should_resume(6 * GB, 85.0, 80, min_free));
        assert!(Ingest::data_dir_should_resume(6 * GB, 80.0, 80, min_free));
        // Usage cleared but absolute floor not met.
        assert!(!Ingest::data_dir_should_resume(4 * GB, 75.0, 80, min_free));
    }
}

#[cfg(all(test, feature = "stats_integration"))]
mod stats_integration_tests {
    use super::*;
    // stats_tailer removed
    use crate::helpers::configuration::{Config, PIPELINE_NAME};
    use serde_json::json;

    #[test]
    #[ignore]
    fn emits_and_flushes_stats_s3() {
        // Short flush for test
        std::env::set_var("STATS_FLUSH_SECONDS", "1");
        // Ensure worker started
        ensure_stats_worker();
        // Emit observations for a test namespace
        let ns = "__test_ns__";
        for v in [3, 1, 5] {
            emit_observation(ns, "a", &json!(v));
        }
        emit_observation(ns, "s", &json!("hi"));
        emit_observation(ns, "s", &json!("hello"));
        // Wait longer than default flush
        std::thread::sleep(std::time::Duration::from_millis(1500));
        // Read stats from S3
        PIPELINE_NAME.write().clear();
        PIPELINE_NAME.write().push_str(ns);
        let v: serde_json::Value = {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async { Config::read_namespace_stats_async(ns).await })
                .expect("missing stats in S3")
        };
        let fields = v
            .get("fields")
            .and_then(|x| x.as_object())
            .expect("no fields");
        let a = fields
            .get("a")
            .and_then(|x| x.as_object())
            .expect("no field a");
        assert_eq!(a.get("total").and_then(|x| x.as_u64()).unwrap(), 3);
        assert_eq!(a.get("nulls").and_then(|x| x.as_u64()).unwrap(), 0);
        assert_eq!(a.get("min_numeric").and_then(|x| x.as_f64()).unwrap(), 1.0);
        assert_eq!(a.get("max_numeric").and_then(|x| x.as_f64()).unwrap(), 5.0);
        let s = fields
            .get("s")
            .and_then(|x| x.as_object())
            .expect("no field s");
        assert_eq!(s.get("min_len").and_then(|x| x.as_u64()).unwrap(), 2);
        assert_eq!(s.get("max_len").and_then(|x| x.as_u64()).unwrap(), 5);
    }

    #[test]
    #[ignore]
    fn mixed_types_emit_and_validate_json_s3() {
        std::env::set_var("STATS_FLUSH_SECONDS", "1");
        ensure_stats_worker();
        let ns = "__test_ns_mixed__";
        // numeric int + float
        emit_observation(ns, "num", &json!(10));
        emit_observation(ns, "num", &json!(3.5));
        emit_observation(ns, "num", &json!(7));
        // strings
        emit_observation(ns, "str", &json!("a"));
        emit_observation(ns, "str", &json!("abcdef"));
        // bools
        emit_observation(ns, "flag", &json!(true));
        emit_observation(ns, "flag", &json!(false));
        // nulls
        emit_observation(ns, "only_nulls", &json!(null));
        emit_observation(ns, "only_nulls", &json!(null));
        // arrays/objects (ignored for bounds)
        emit_observation(ns, "complex", &json!([1, 2, 3]));
        emit_observation(ns, "complex", &json!({"k":"v"}));

        std::thread::sleep(std::time::Duration::from_millis(1500));

        let v: serde_json::Value = {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async { Config::read_namespace_stats_async(ns).await })
                .expect("missing stats in S3")
        };
        assert_eq!(v.get("namespace").and_then(|x| x.as_str()).unwrap(), ns);
        let fields = v
            .get("fields")
            .and_then(|x| x.as_object())
            .expect("no fields");

        let num = fields
            .get("num")
            .and_then(|x| x.as_object())
            .expect("no num");
        // min/max should reflect 3.5 .. 10
        let min_n = num.get("min_numeric").and_then(|x| x.as_f64()).unwrap();
        let max_n = num.get("max_numeric").and_then(|x| x.as_f64()).unwrap();
        assert!((min_n - 3.5).abs() < 1e-9, "min_numeric={min_n}");
        assert!((max_n - 10.0).abs() < 1e-9, "max_numeric={max_n}");
        assert_eq!(num.get("total").and_then(|x| x.as_u64()).unwrap(), 3);

        let st = fields
            .get("str")
            .and_then(|x| x.as_object())
            .expect("no str");
        assert_eq!(st.get("min_len").and_then(|x| x.as_u64()).unwrap(), 1);
        assert_eq!(st.get("max_len").and_then(|x| x.as_u64()).unwrap(), 6);
        assert_eq!(st.get("total").and_then(|x| x.as_u64()).unwrap(), 2);

        let fl = fields
            .get("flag")
            .and_then(|x| x.as_object())
            .expect("no flag");
        let approx = fl
            .get("approx_distinct")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        assert!(approx >= 1);
        assert_eq!(fl.get("total").and_then(|x| x.as_u64()).unwrap(), 2);

        let on = fields
            .get("only_nulls")
            .and_then(|x| x.as_object())
            .expect("no only_nulls");
        assert_eq!(on.get("total").and_then(|x| x.as_u64()).unwrap(), 2);
        assert_eq!(on.get("nulls").and_then(|x| x.as_u64()).unwrap(), 2);
        assert!(on.get("min_numeric").unwrap().is_null());
        assert!(on.get("max_numeric").unwrap().is_null());
        assert!(on.get("min_len").unwrap().is_null());
        assert!(on.get("max_len").unwrap().is_null());

        let cx = fields
            .get("complex")
            .and_then(|x| x.as_object())
            .expect("no complex");
        assert_eq!(cx.get("total").and_then(|x| x.as_u64()).unwrap(), 2);
        assert!(cx.get("min_numeric").unwrap().is_null());
        assert!(cx.get("max_numeric").unwrap().is_null());
        assert!(cx.get("min_len").unwrap().is_null());
        assert!(cx.get("max_len").unwrap().is_null());
    }
}

#[cfg(test)]
mod storage_key_tests {
    use super::*;

    #[test]
    fn storage_namespace_sanitizes_special_chars() {
        assert_eq!(
            storage_namespace("acme.source/stream-name"),
            "acme_source_stream_name"
        );
        assert_eq!(storage_namespace("My-Stream/v2"), "my_stream_v2");
    }

    #[test]
    fn storage_partition_preserves_hive_segments() {
        assert_eq!(
            storage_partition("crawl_date=2026-05-30T00:00:00/year=2026"),
            "crawl_date=2026_05_30t00_00_00/year=2026"
        );
    }
}
