use crate::buffer::{BufferChunker};
use crate::discover::{AnalyseSchema, Metadata, NUM_ANALYSED_RECORDS, PipelineMetadata};
use crate::helpers::configuration::Config;
use crate::helpers::offsets::{OffsetKey, Offsets, OffsetTypes};
use crate::helpers::Helpers;
use crate::ingest::ingest::ingest;
use crate::serdes::json::SerdeJson;
use crate::{ARROW_SCHEMA, DISCOVER_RUNNING, helpers, METADATA, METRICS, RUNNING};


use once_cell::sync::Lazy;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::{fs, io};


use std::io::{BufWriter, Write};
use std::ops::Deref;
use std::os::fd::AsRawFd;

use std::path::PathBuf;
use std::process::exit;
use std::string::ToString;
use std::sync::{Arc, RwLock};
use std::time::{Instant, SystemTime};
use threadpool::ThreadPool;
use std::sync::mpsc::channel;
extern crate num_cpus;
use std::sync::mpsc::Sender;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::atomic::Ordering::AcqRel;
use arrow::json::ReaderBuilder;
use arrow::record_batch::RecordBatch;
use dashmap::{DashMap, DashSet};
use nix::libc;

use parquet::data_type::AsBytes;
use crate::ingest::fast_ingest::{create_default_nested_message, DEFAULT_NESTED_MESSAGE, fast_path_ingest};




use tokio::io::AsyncWriteExt;
use helpers::timed_rwlock::TimedRwLock;
use crate::buffer::ingest_buffer::{Buffers, IngestBufferBatch, IngestRecord, OffsetKeySerialize, WalFile};
use crate::serdes::csv::SerderCsv;

use crate::serdes::xml::SerdeXml;
// use crate::converters::skippr_avro::convert_skippr_to_avro_field_types;

use arrow::datatypes::Schema;
use arrow::error::ArrowError;
use arrow::datatypes;
use arrow_schema::SchemaRef;
use tokio::runtime;
use crate::cli::{Cli, CLI_MODE, Mode};
use crate::converters::skippr_arrow::convert_skippr_to_arrow;
use crate::plugins::athena::DataOutputAwsAthenaPlugin;
use crate::plugins::DataOutputPlugin;


#[derive(Clone, Debug)]
pub struct IngestBatch {
    pub(crate) offset_key: OffsetKey,
    pub(crate) data: String,
}

// Bare metal platforms usually have very small amounts of RAM
// (in the order of hundreds of KB)
pub const WRITE_BUF_SIZE: usize = if cfg!(target_os = "espidf") {
    512
} else {
    512 * 1024
};

thread_local! {
    pub static PARSE_NAMESPACE_CACHE: Lazy<RwLock<HashMap<String, String>>> = Lazy::new(|| RwLock::new(HashMap::new()));

    pub static PARTITION_ALLOWED_VALUES_CACHE: Lazy<RwLock<HashSet<String>>> = Lazy::new(|| RwLock::new(HashSet::new()));
}

pub static DEADLETTER_FILE_NAME: Lazy<String> = Lazy::new(|| BufferChunker::encode_chunk_name(
    "deadletters",
    Some(Config::get_pipeline_name().as_str()),
    None,
    None,
    None
));

pub static DEADLETTER_FILE: Lazy<Arc<TimedRwLock<BufWriter<File>>>> = Lazy::new(|| {
    let data_dir = Config::get_data_dir();
    let deadletter_dir = format!("{}/deadletter_buffer", data_dir);
    let output_file = format!("{}/{}", deadletter_dir.clone(), &DEADLETTER_FILE_NAME.as_str());
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&output_file)
        .unwrap();

    let writer = BufWriter::new(file);

    Arc::new(TimedRwLock::new("deadletter_file".to_string(), writer))
});

#[derive(Clone, Debug)]
struct SchemaHash {
    schema: SchemaRef,
    hash: String,
}

pub struct Ingest {
    thread_pool: ThreadPool,
    num_cpus: usize,
    tx: Sender<()>,
    active_count: Arc<AtomicUsize>,
    schema_hashes: DashMap<String, SchemaHash>,
    analyse_schema: AnalyseSchema,
}

impl Drop for Ingest {
    fn drop(&mut self) {
        println!("Completing, waiting for {} ingest tasks to finish", self.active_count.load(Ordering::SeqCst));

        self.wait_for_completion();
    }
}

impl Ingest {
    pub fn new() -> Ingest {
        let num_cpus = num_cpus::get().max(2);
        println!("Starting with {} threads", num_cpus);
        let (tx, rx) = channel();
        let active_count = Arc::new(AtomicUsize::new(0));
        let active_count_clone = active_count.clone();

        let thread_pool = ThreadPool::new(num_cpus);
        thread_pool.execute(move || {
            while let Ok(()) = rx.recv() {
                active_count_clone.fetch_sub(1, Ordering::SeqCst);
            }
        });

        // Schema hashes
        let mut schema_hashes = DashMap::new();

        {
            let schemas = ARROW_SCHEMA.read();
            schema_hashes = schemas.iter().map(|(namespace, schema)| {
                let schema_hash = SchemaHash {
                    schema: Arc::clone(schema),
                    hash: format!("{:?}", md5::compute(format!("{:?}", Arc::clone(schema).deref())))
                };
                (namespace.clone(), schema_hash)
            }).collect::<DashMap<_, _>>();
        }

        let analyse_schema: AnalyseSchema = AnalyseSchema { i: 0 };

        Ingest {
            num_cpus,
            thread_pool,
            tx,
            active_count,
            schema_hashes,
            analyse_schema
        }
    }

    pub fn wait_for_completion(&self) {

        let mut current_active_count = self.active_count.load(Ordering::SeqCst);

        while self.active_count.load(Ordering::SeqCst) > 0 {

            if current_active_count != self.active_count.load(Ordering::SeqCst) {
                println!("Waiting for {} ingest tasks to finish", self.active_count.load(Ordering::SeqCst));
                current_active_count = self.active_count.load(Ordering::SeqCst);
            }
            
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        println!("All ingest tasks finished");
    }

    pub fn ingest_file(
        &self,
        datas: &Arc<Vec<IngestBatch>>,
        offset_db: &Arc<Offsets>,
        shared_output: Arc<TimedRwLock<Box<dyn DataOutputPlugin + Send + Sync>>>,
    ) {
        // If we're not running, exit after current threads finish.
        if !RUNNING.read().load(Ordering::SeqCst) {
            println!("Waiting for remaining threads to complete");
            self.wait_for_completion();
            exit(0);

        } else {

            match CLI_MODE.read().clone() {
                Mode::Sync(_) => {},
                _ => {

                    let max_records = 1000;

                    let mut pipeline_metadata= METADATA.read().clone();

                    let mut i = 0;

                    for data in datas.iter() {

                        i += 1;

                        self.analyse_schema.infer_json_schema(
                            &mut data.data.clone(),
                            Some(max_records),
                            &mut pipeline_metadata.metadata,
                        );

                    }

                    {
                        METADATA.write().metadata = pipeline_metadata.metadata.clone();
                    }

                    println!("Analysed schema for {}/{} records", NUM_ANALYSED_RECORDS.read().load(Ordering::SeqCst), max_records);

                    // println!("Completed schema analysis for batch");

                    if NUM_ANALYSED_RECORDS.read().load(Ordering::SeqCst) >= max_records {
                        let mut pipeline = METADATA.write();
                        *pipeline = pipeline_metadata;

                        DISCOVER_RUNNING.write().store(false, Ordering::SeqCst);
                    }

                    return;
                }
            }

            // Wait for an available thread if there's no capacity
            while self.active_count.load(Ordering::SeqCst) >= self.num_cpus {
                
                std::thread::sleep(std::time::Duration::from_millis(100));
            }

            // Spawn a new thread for this 'datas'
            let tx = self.tx.clone();
            let offset_db_clone = offset_db.clone();

            self.active_count.fetch_add(1, Ordering::SeqCst);

            let mut datas_clone = datas.clone();
            
            let mut schema_hashes = self.schema_hashes.clone();

            let handle = runtime::Handle::current();

            let shared_output_clone = shared_output.clone();

            self.thread_pool.execute(move || {
                Ingest::process_batch(&mut datas_clone, &offset_db_clone, &mut schema_hashes, handle, shared_output_clone);
                tx.send(()).unwrap();
            });
        }

    }

    pub(crate) fn deadletter(lines: &str) {

        let mut deadletter_file = DEADLETTER_FILE.write();

        deadletter_file.write(lines.as_bytes()).or(Err("Could not write to deadletter file")).unwrap();
        deadletter_file.write("\n".as_bytes()).or(Err("Could not write to deadletter file")).unwrap();

        deadletter_file.flush().or(Err("Could not flush deadletter file")).unwrap();

    }

    fn process_batch(
        datas: &Arc<Vec<IngestBatch>>,
        offset_db_clone: &Arc<Offsets>,
        schema_hashes: &mut DashMap<String, SchemaHash>,
        handle: runtime::Handle,
        shared_output: Arc<TimedRwLock<Box<dyn DataOutputPlugin + Send + Sync>>>,
    ) {

        // optional: enforce allowed partition values
        // This is thread local, a micro-optimization would be to move this to a global variable since it's not going to change and therefore no locking is required
        let allowed_values = Config::get_partition_allowed_values();

        PARTITION_ALLOWED_VALUES_CACHE.with(|cache| {
            cache.write().unwrap().extend(allowed_values.split(",").map(|v| {
                Helpers::clean_field_name(v.to_string())
            }).collect::<HashSet<String>>());
        });


        let default_schema_hash = format!("{:?}", md5::compute(Helpers::random_str(10)));
        
        let flatten = Config::truth_value(&Config::get_transform_config().flatten_events.or(Some("no".to_string())).unwrap());

        let data_dir = Config::get_data_dir();
        let output_dir = format!("{}/ingest_buffer", data_dir);
        let _deadletter_dir = format!("{}/deadletter_buffer", data_dir);

        let _aprox_now = SystemTime::now();
        
        let mut updated_schema= "no".to_string();

        let mut buffers = Buffers::new();

        let mut bytes: u64 = 0;
        let mut latest_timestamp: i64 = 0;
        let mut i: u64 = 0;
        let mut j = 0;
        let mut d = 0;
        let mut x = 0;
        let mut batch_line: u64 = 0;

        let format = match Config::get_pipline_plugin_config("input") {
            Ok(plugin) => plugin.format(),
            Err(_) => "json".to_string()
        };

        // @todo - check PluginConfig format is xml
        // if format == "xml" {
        //     let batch = IngestBatch {
        //         offset_key: datas[0].offset_key.clone(),
        //         data: datas.iter().map(|v| v.data.as_str()).collect::<Vec<&str>>().join(""),
        //     };
        //     datas.clear();
        //     datas.push(batch);
        // }

        let entity_field_dot = match Config::get_transform_config().record_field_path {
            Some(ref field) => field.clone(),
            None => "".to_string()
        };
        
        let mut buf: HashMap<(String, String, Option<i64>, String), IngestBufferBatch> = HashMap::new();

        for ingest_batch in datas.iter() {

            bytes += ingest_batch.data.len() as u64;
            
            let has_offsets =
                offset_db_clone.validate(&ingest_batch.offset_key, OffsetTypes::Closed, 0);
            let current_line_offset = offset_db_clone.validate(&ingest_batch.offset_key, OffsetTypes::Position, 0);

            let mut records: Vec<Value> = Vec::new();
            if format == "csv" {
                records = SerderCsv::deserialize(&ingest_batch.data);
            } else if format == "xml" {
                records = SerdeXml::deserialize(ingest_batch.data.as_bytes());
            } else {
                records = SerdeJson::deserialize(&ingest_batch.data.clone());
            }

            if !entity_field_dot.is_empty() {
                records = match Helpers::process_values(&records, &entity_field_dot) {
                    Some(records) => records,
                    None => Vec::new()
                };
            }

            batch_line = 0;

            let mut unwrapped_records: Vec<Value> = Vec::new();

            for record in records {
                
                match record.as_object() {
                    Some(_v) => unwrapped_records.push(record),
                    None => {
                        match record.as_array() {
                            Some(v) => {
                                for item in v {
                                    // println!("Item: {}", item);
                                    unwrapped_records.push(item.clone());
                                }
                            },
                            None => {
                                let line_no = if batch_line == 0 || batch_line > ingest_batch.data.lines().count() as u64 {
                                    1
                                } else {
                                    batch_line - 1
                                };

                                // deadletter
                                let line_str = match ingest_batch.data.lines().nth(line_no as usize) {
                                    Some(line) => line,
                                    None => ""
                                };

                                Self::deadletter(line_str);
                                offset_db_clone.insert(&ingest_batch.offset_key, OffsetTypes::Position, batch_line);

                                d += 1;

                            }
                        }
                    }
                };

            }


            for record in unwrapped_records {

                batch_line += 1;

                if record.is_null()
                    || (record.is_object() && record.as_object().unwrap().is_empty())
                    || (record.is_array() && record.as_array().unwrap().is_empty())
                {

                    let line_str = match ingest_batch.data.lines().nth(batch_line as usize - 1) {
                        Some(line) => line,
                        None => {
                            ""
                        }
                    };


                    Self::deadletter(line_str);
                    offset_db_clone.insert(&ingest_batch.offset_key, OffsetTypes::Position, batch_line);

                    d += 1;

                    continue;
                }

                if has_offsets.is_none()
                    || current_line_offset.is_none()
                    || (Some(true) == has_offsets
                        && Some(true) == offset_db_clone.validate(&ingest_batch.offset_key, OffsetTypes::Position, batch_line))
                {

                    i += 1;

                    let mut namesapce_cache =  PARSE_NAMESPACE_CACHE.with(|cache| cache.read().unwrap().clone());
                    let skpr_namespace = Helpers::parse_namespace_field(
                        &record,
                        Config::get_pipeline_name(),
                        &mut namesapce_cache,
                    );

                    if namesapce_cache != PARSE_NAMESPACE_CACHE.with(|cache| cache.read().unwrap().clone()) {
                        PARSE_NAMESPACE_CACHE.with(|cache| cache.write().unwrap().clear());
                        PARSE_NAMESPACE_CACHE.with(|cache| cache.write().unwrap().extend(namesapce_cache));
                    }

                    let allowed_values = PARTITION_ALLOWED_VALUES_CACHE.with(|cache| cache.read().unwrap().clone());
                    let skpr_partition = Helpers::parse_partition_field(&record, allowed_values);
                    let skpr_time = Helpers::parse_time_field(&record);

                    let mut skpr_time_bucket: Option<i64> = None;

                    if skpr_time.is_some() {
                        skpr_time_bucket =
                            Some(BufferChunker::event_time_bucket(skpr_time.unwrap()));

                        if skpr_time.unwrap() > latest_timestamp {
                            latest_timestamp = skpr_time.unwrap();
                        }
                    }

                    if METADATA.read().metadata.get(&skpr_namespace).is_none() {
                        METADATA.write().metadata.insert(skpr_namespace.clone(), Metadata::new().unwrap());
                        println!("Discovered new namespace: {}", skpr_namespace);
                    }

                    let msg = match METADATA.read().metadata.get(&skpr_namespace) {
                        Some(metadata) => {
                            fast_path_ingest(
                                &record,
                                metadata.fields.as_ref(),
                                &skpr_namespace,
                                flatten,
                            )
                        }
                        None => {
                            Err(format!("Failed to find metadata for namespace: {}", skpr_namespace).into())
                        }
                    };

                    let record_value = match msg {
                        Ok(msg) => {
                            msg
                        },
                        Err(_err) => {

                            // println!("Falling back to slow path due to: {}", _err);

                            // hurrendous allocation, but we're handling an error case.
                            // It's important to not update METADATA mutex for other threads till we know the discovered schema is valid
                            // This may be called very often making troublesome data even worse
                            let mut metadata: PipelineMetadata;
                            {
                                metadata = METADATA.read().clone();
                            }

                            let msg = match ingest(
                                &record,
                                &mut metadata.metadata.get_mut(&skpr_namespace).unwrap().fields,
                                &skpr_namespace,
                                &mut updated_schema,
                                flatten,
                            ) {
                                Ok(msg) => msg,
                                Err(_err) => {

                                    updated_schema = "no".to_string();

                                    drop(metadata); // drop discovered schema to ensure we can accedentally use it

                                    // @todo - if we're going to log this, we should only do it when the schema was evovled for the deadlettered record
                                    if METRICS.read().deadletters_total == 0 {
                                        println!("Record deadlettered, schema evolution for deadletters will be ignored");
                                    }

                                    // deadletter record
                                    let line_str = match ingest_batch.data.lines().nth(batch_line as usize - 1) {
                                        Some(line) => line,
                                        None => {
                                            // println!("Could not find line {} in batch", batch_line);
                                            ""
                                        }
                                    };

                                    Self::deadletter(line_str);
                                    offset_db_clone.insert(&ingest_batch.offset_key, OffsetTypes::Position, batch_line);

                                    d += 1;

                                    continue
                                }
                            };

                            
                            // update metadata in runtime and ingest message
                            if Config::get_auto_approve() {

                                if updated_schema.as_str() == "yes" {
                                    
                                    {
                                        METADATA.write().metadata = metadata.metadata.clone();
                                    }

                                    updated_schema = "no".to_string();

                                    handle.block_on(async {
                                        Config::set_metadata(&metadata, true).await;
                                    });

                                    println!("Updated schema for namespace: {}", skpr_namespace);
                                    
                                    let mut default_message = Value::Null;
                                    {
                                        default_message = create_default_nested_message(&metadata.metadata.get(&skpr_namespace).unwrap().fields);
                                    }

                                    {
                                        let mut lock = DEFAULT_NESTED_MESSAGE.write();
                                        lock.insert(skpr_namespace.clone(), default_message);
                                    }

                                    Ingest::prepare_arrow_schema(&skpr_namespace, flatten).unwrap();

                                    {
                                        let schemas = ARROW_SCHEMA.read();

                                        let schema = Arc::clone(schemas.get(&skpr_namespace).unwrap());
                                        let hash = format!("{:?}", md5::compute(format!("{:?}", schema.deref())));

                                        schema_hashes.insert(skpr_namespace.clone(), SchemaHash {
                                            schema: schema,
                                            hash: hash
                                        });
                                    }
                                }

                                x += 1;

                                msg

                            } else { // or just deadletter message for later approval
                                let line_str = match ingest_batch.data.lines().nth(batch_line as usize - 1) {
                                    Some(line) => line,
                                    None => {
                                        // println!("Could not find line {} in batch", batch_line);
                                        ""
                                    }
                                };

                                Self::deadletter(line_str);
                                offset_db_clone.insert(&ingest_batch.offset_key, OffsetTypes::Position, batch_line);
                                
                                d += 1;

                                continue;
                            }
                        }
                    };

                    let ingest_record = IngestRecord {
                        namespace: skpr_namespace.clone(),
                        partition: skpr_partition.clone(),
                        time: skpr_time_bucket.clone(),
                        record: record_value,
                    };

                    let schema_hash = match schema_hashes.get(&skpr_namespace) {
                        Some(hash) => hash.clone(),
                        None => {

                            let mut schemas: HashMap<String, SchemaRef> = HashMap::new();
                            {
                                schemas = ARROW_SCHEMA.read().clone()
                            }
                            if schemas.get(&skpr_namespace).is_none() {
                                let start_time = Instant::now();

                                let mut i = 0;
                                while ARROW_SCHEMA.read().get(&skpr_namespace).is_none() {
                                    if i == 0 || i % 100 == 0 { // inital and every 10 seconds
                                        println!("Waiting for schema to be prepared for namespace: {}", skpr_namespace);
                                    }
                                    std::thread::sleep(std::time::Duration::from_millis(100));
                                    i += 1;
                                }
                                schemas = ARROW_SCHEMA.read().clone();

                                let elapsed = start_time.elapsed();
                                let nanos = elapsed.as_nanos() as u64;
                                crate::helpers::timed_rwlock::TOTAL_WAIT_TIMES
                                    .entry("new_schema_hash".to_string())
                                    .or_insert_with(|| AtomicU64::new(0))
                                    .fetch_add(nanos, Ordering::Relaxed);
                            }

                            let schema = Arc::clone(schemas.get(&skpr_namespace).unwrap());
                            let hash = format!("{:?}", md5::compute(format!("{:?}", schema.deref())));

                            let schema_hash = SchemaHash {
                                schema: schema,
                                hash: hash
                            };

                            schema_hashes.insert(skpr_namespace.clone(), schema_hash.clone());

                            schema_hash
                        }
                    };

                    let buf_entry = buf.entry((
                        skpr_namespace.clone(),
                        skpr_partition.clone(),
                        skpr_time_bucket.clone(),
                        schema_hash.hash
                    )).or_insert_with(|| {

                        IngestBufferBatch {
                            offsets: HashMap::new(),
                            namespace: skpr_namespace.clone(),
                            partition: skpr_partition.clone(),
                            time: skpr_time_bucket,
                            shard: "".to_string(),
                            records: Vec::new(),
                            schema: schema_hash.schema,
                        }
                    });
                    
                    // an ingest batch consist of many small files/queue messages, etc. Each will need its offset committed in the WAL.
                    buf_entry.offsets.insert(ingest_batch.offset_key.clone(), batch_line);
                    
                    buf_entry.records.push(ingest_record);

                    j += 1;
                    
                }
            }

            // may have deadlettered some records, so we need to update the offset since they won't be in the WAL
            // offset_db_clone.insert(&ingest_batch.offset_key, OffsetTypes::Closed, 1);

        }
        
        buffers.write(buf);

        let offset_db_clone = offset_db_clone.clone();
        let shared_output_clone = shared_output.clone();
        handle.block_on(async {
            buffers.flush(offset_db_clone, shared_output_clone).await.expect("Failed to flush buffers")
        });

        let mut counter_lock = METRICS.write();
        counter_lock.deadletters_total += d;
        counter_lock.ingeted_slow_total += x;
        counter_lock.messages_total += i;
        counter_lock.source_bytes_total += bytes;

        if latest_timestamp as u64 > counter_lock.latest_timestamp {
            counter_lock.latest_timestamp = latest_timestamp as u64;
        }

    }

    pub(crate) fn prepare_arrow_schema(skpr_namespace: &str, flatten: bool) -> Result<Arc<Schema>, ArrowError> {
        
        let mut arrow_schema: Result<datatypes::Schema, ArrowError> = Ok(datatypes::Schema::empty());
        let mut schema_ref = Arc::new(datatypes::Schema::empty());

        let metadata: PipelineMetadata;
        {
            metadata = METADATA.read().clone();
        }

        if metadata.metadata.get(skpr_namespace).is_none() {
            return Err(ArrowError::SchemaError(format!("Failed to find metadata for namespace: {}", skpr_namespace)));
        }

        let mut output_metadata: HashMap<String, Metadata> = HashMap::new();
        if flatten {
            let mut meta: HashMap<String, Metadata> = HashMap::new();

            crate::flatten_metadata(metadata.metadata.get(skpr_namespace).unwrap(), &mut meta);

            let mut flat: Metadata = Metadata::new().unwrap();
            flat.fields = Box::new(meta);
            output_metadata.insert(skpr_namespace.to_string(), flat);
        } else {
            output_metadata = metadata.metadata.clone();
        }

        arrow_schema = convert_skippr_to_arrow(
            output_metadata.get(skpr_namespace).unwrap().fields.clone(),
        );

        schema_ref = Arc::new(arrow_schema.unwrap());

        ARROW_SCHEMA.write().insert(skpr_namespace.to_string(), schema_ref.clone());

        Ok(schema_ref)
    }
    
}


