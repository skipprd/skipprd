use crate::buffer::{BufferChunker};
use crate::discover::Metadata;
use crate::helpers::configuration::Config;
use crate::helpers::offsets::{OffsetKey, Offsets, OffsetTypes};
use crate::helpers::Helpers;
use crate::ingest::ingest::ingest;
use crate::serdes::json::SerdeJson;
use crate::{ARROW_SCHEMA, helpers, METADATA, METRICS, RUNNING};


use once_cell::sync::Lazy;
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;


use std::io::Write;
use std::ops::Deref;
use std::os::fd::AsRawFd;

use std::path::PathBuf;
use std::process::exit;
use std::string::ToString;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime};
use threadpool::ThreadPool;
use std::sync::mpsc::channel;
extern crate num_cpus;
use std::sync::mpsc::Sender;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::atomic::Ordering::AcqRel;
use arrow::json::ReaderBuilder;
use arrow::record_batch::RecordBatch;
use dashmap::DashMap;
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
use crate::converters::skippr_arrow::convert_skippr_to_arrow;


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
}
pub static DEADLETTER_FILE_NAME: Lazy<String> = Lazy::new(|| BufferChunker::encode_chunk_name(
    "deadletters",
    Some(Config::get_pipeline_name().as_str()),
    None,
    None,
    None
));

// static AVRO_SCHEMA: Lazy<Mutex<HashMap<String, Schema>>> = Lazy::new(|| {
//
//     let mut avro_schemas: HashMap<String, Schema> = HashMap::new();
//
//     for (namespace, schema) in METADATA.read().unwrap().iter() {
//         let raw_schema = convert_skippr_to_avro_field_types(&schema.fields);
//         avro_schemas.insert(namespace.to_string(), raw_schema.unwrap());
//     }
//
//     Mutex::new(avro_schemas)
// });

pub struct Ingest {
    thread_pool: ThreadPool,
    num_cpus: usize,
    tx: Sender<()>,
    active_count: Arc<AtomicUsize>,
    buffers: Arc<TimedRwLock<Buffers>>,
    // run_id: String,
}

impl Drop for Ingest {
    fn drop(&mut self) {
        println!("Dropping Ingest Struct: Waiting for {} ingest tasks to finish", self.active_count.load(Ordering::SeqCst));

        self.wait_for_completion();
    }
}

impl Ingest {
    pub fn new() -> Ingest {
        let num_cpus = num_cpus::get().max(2);
        println!("Ingesting with {} threads", num_cpus);
        let (tx, rx) = channel();
        let active_count = Arc::new(AtomicUsize::new(0));
        let active_count_clone = active_count.clone();

        let thread_pool = ThreadPool::new(num_cpus);
        thread_pool.execute(move || {
            while let Ok(()) = rx.recv() {
                active_count_clone.fetch_sub(1, Ordering::SeqCst);
            }
        });

        let mut buffers = Buffers::new();
        let mut buffers = Arc::new(TimedRwLock::new("buffers".to_string(), buffers));

        // let metrics = METRICS.read();
        // let run_id = metrics.run_id.clone();

        Ingest {
            num_cpus,
            thread_pool,
            tx,
            active_count,
            buffers,
            // run_id: schema_md5_str
        }
    }

    pub fn wait_for_completion(&self) {

        let mut current_active_count = self.active_count.load(Ordering::SeqCst);

        while self.active_count.load(Ordering::SeqCst) > 0 {

            if current_active_count != self.active_count.load(Ordering::SeqCst) {
                println!("Waiting for {} ingest tasks to finish", self.active_count.load(Ordering::SeqCst));
                current_active_count = self.active_count.load(Ordering::SeqCst);
            }

            // Here you can do other work while waiting for threads to finish,
            // or just sleep for a while if there's nothing else to do.
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        println!("All ingest tasks finished");
    }

    pub fn ingest_file(
        &self,
        datas: Vec<IngestBatch>,
        offset_db: &Arc<Offsets>,
    ) {
        // println!("Ingesting {} events", datas.len());

        // If we're not running, exit after current threads finish.
        if !RUNNING.read().load(Ordering::SeqCst) {
            println!("Not running, exiting");
            self.wait_for_completion();
            exit(0);
        } else {

            // Wait for an available thread if there's no capacity
            while self.active_count.load(Ordering::SeqCst) >= self.num_cpus {
                // println!("Waiting for {} tasks to finish", self.active_count.load(Ordering::SeqCst));

                // Here you can do other work while waiting for threads to finish,
                // or just sleep for a while if there's nothing else to do.
                std::thread::sleep(std::time::Duration::from_millis(100));
            }

            // Spawn a new thread for this 'datas' if there's capacity
            // while self.active_count.load(Ordering::SeqCst) <= self.num_cpus {
            // if self.thread_pool.queued_count() < self.num_cpus {
            let tx = self.tx.clone();
            let offset_db_clone = offset_db.clone();

            self.active_count.fetch_add(1, Ordering::SeqCst);

            let mut datas_clone = datas.clone();

            let core_id = self.thread_pool.active_count();

            let buffers_clone = self.buffers.clone();

            // let run_id = self.run_id.clone();

            // let metrics = METRICS.read();
            // let run_id = metrics.run_id.clone();


            let schema = ARROW_SCHEMA.read();
            let run_id = format!("{:?}", md5::compute(format!("{:?}", schema.deref())));

            self.thread_pool.execute(move || {
                // println!("Processing batch of {} events on core {}", datas_clone.len(), core_count);
                // Ingest::process_batch(&mut datas_clone, &offset_db_clone, buffers_clone, &core_id.to_string(), run_id).await;
                // tx.send(()).unwrap();

                // Use a new Tokio runtime or an appropriate async runtime
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async {
                    Ingest::process_batch(&mut datas_clone, &offset_db_clone, buffers_clone, &core_id.to_string(), run_id).await;
                    tx.send(()).unwrap(); // Assuming this is a synchronous channel
                });
            });


            // }
        }

    }

    fn deadletter(record: &str, buffers: &Buffers) {
        let data_dir = Config::get_data_dir();
        let deadletter_dir = format!("{}/deadletter_buffer", data_dir);

        let output_file = format!("{}/{}", deadletter_dir.clone(), &DEADLETTER_FILE_NAME.as_str());

        // buffers.write(&output_file, record.as_bytes());
        // buffers.write(&output_file, "\n".as_bytes());

    }

    async fn process_batch(
        datas: &mut Vec<IngestBatch>,
        offset_db_clone: &Arc<Offsets>,
        buffers: Arc<TimedRwLock<Buffers>>,
        core_id: &str,
        run_id: String,
    ) {

        let mut run_id = run_id.clone();

        // let mut avro_schemas = AVRO_SCHEMA.lock().unwrap();

        let flatten = Config::truth_value(&Config::get_transform_config().flatten_events.or(Some("no".to_string())).unwrap());

        let data_dir = Config::get_data_dir();
        let output_dir = format!("{}/ingest_buffer", data_dir);
        let _deadletter_dir = format!("{}/deadletter_buffer", data_dir);

        let _aprox_now = SystemTime::now();

        let updated_schema: Arc<Mutex<String>> = Arc::new(Mutex::new("no".to_string()));

        let updated_schema_clone = updated_schema;
        // let offset_db_clone = offset_db.clone();

        // let mut buffers: Buffers = BUFFER_INDEX.read().get(&output_dir).unwrap().clone();
        let mut buffers = Buffers::new();

        let mut bytes: u64 = 0;
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
        if format == "xml" {
            let batch = IngestBatch {
                offset_key: datas[0].offset_key.clone(),
                data: datas.iter().map(|v| v.data.as_str()).collect::<Vec<&str>>().join(""),
            };
            datas.clear();
            datas.push(batch);
        }

        let entity_field_dot = match Config::get_transform_config().record_field_path {
            Some(ref field) => field.clone(),
            None => "".to_string()
        };

        let mut batch_offset_lines: HashMap<OffsetKey, u64> = HashMap::new();
        let mut batch_offset_files: HashMap<OffsetKey, u64> = HashMap::new();
        let mut buffer_batchs: HashMap<String, String> = HashMap::new();

        let mut buf: HashMap<(String, String, Option<i64>, String), IngestBufferBatch> = HashMap::new();

        for ingest_batch in datas {

            // let mut buf: IngestBufferBatch = IngestBufferBatch {
            //     offset: OffsetKeySerialize {
            //         position: 0,
            //         source_namespace: ingest_batch.offset_key.namespace.clone(),
            //         source_partition: ingest_batch.offset_key.partition.clone(),
            //     },
            //     records: Vec::new(),
            // };

            let has_offsets =
                offset_db_clone.validate(&ingest_batch.offset_key, OffsetTypes::Closed, 0);
            let current_line_offset = offset_db_clone.validate(&ingest_batch.offset_key, OffsetTypes::Line, 0);

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

                // println!("Record: {}", record);

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

                                Self::deadletter(line_str, &buffers);

                                d += 1;

                            }
                        }
                    }
                };

            }


            for record in unwrapped_records {

                // println!("Record: {}", record);
                batch_line += 1;

                if record.is_null()
                    || (record.is_object() && record.as_object().unwrap().is_empty())
                    || (record.is_array() && record.as_array().unwrap().is_empty())
                {

                    // let line batch_line in ingest_batch.data
                    let line_str = match ingest_batch.data.lines().nth(batch_line as usize - 1) {
                        Some(line) => line,
                        None => {
                            // println!("Could not find null line {} in batch", batch_line);
                            // println!("Batch lines: {}", ingest_batch.data.lines().count());
                            // panic!("Could not find null line {} in batch", batch_line);
                            ""
                        }
                    };


                    Self::deadletter(line_str, &buffers);

                    d += 1;

                    continue;
                }

                if has_offsets.is_none()
                    || current_line_offset.is_none()
                    // || Some(true) == offset_db_clone.validate(&ingest_batch.offset_key, OffsetTypes::Line, batch_line)
                    || Some(true) == has_offsets
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

                    let skpr_partition = Helpers::parse_partition_field(&record);
                    let skpr_time = Helpers::parse_time_field(&record);

                    let mut skpr_time_bucket: Option<i64> = None;

                    if skpr_time.is_some() {
                        skpr_time_bucket =
                            Some(BufferChunker::event_time_bucket(skpr_time.unwrap()));
                    }

                    // let output_file_name = BufferChunker::encode_chunk_name(
                    //     "output",
                    //     Some(&skpr_namespace),
                    //     Some(&skpr_partition),
                    //     skpr_time_bucket,
                    //     Some(core_id),
                    // );

                    if METADATA.read().get(&skpr_namespace).is_none() {
                        METADATA.write().insert(skpr_namespace.clone(), Metadata::new().unwrap());
                        println!("New namespace: {}", skpr_namespace);
                    }

                    let msg = match METADATA.read().get(&skpr_namespace) {
                        Some(metadata) => {
                            fast_path_ingest(
                                &record,
                                metadata.fields.as_ref(),
                                &skpr_namespace,
                                flatten,
                            )
                        }
                        None => {
                            // println!("Failed to find metadata for namespace: {}", skpr_namespace);
                            Err(format!("Failed to find metadata for namespace: {}", skpr_namespace).into())
                        }
                    };

                    let record_value = match msg {
                        Ok(msg) => {
                            // println!("Fast path ingest: {}", msg);
                            // msg.to_string() + "\n"
                            // buffers.write(&output_file_name, buf_str.as_bytes());
                            msg
                        },
                        Err(_err) => {

                            // let old_metadata = NEW_METADATA.read().unwrap().clone();

                            // if NEW_METADATA.read().get(&skpr_namespace).is_none() {
                            //     NEW_METADATA.write().insert(skpr_namespace.clone(), Metadata::new().unwrap());
                            // }

                            // println!("Falling back to slow path due to: {}", err);
                            let msg = match ingest(
                                &record,
                                // &mut NEW_METADATA.write().get_mut(&skpr_namespace).unwrap().fields,
                                &mut METADATA.write().get_mut(&skpr_namespace).unwrap().fields,
                                &skpr_namespace,
                                &mut updated_schema_clone.lock().unwrap(),
                                flatten,
                            ) {
                                Ok(msg) => msg,
                                Err(_err) => {
                                    // println!("Deadlettring - Could not ingest record: {}, Error: {:?}", record, err);

                                    // deadletter record
                                    let line_str = match ingest_batch.data.lines().nth(batch_line as usize - 1) {
                                        Some(line) => line,
                                        None => {
                                            // println!("Could not find line {} in batch", batch_line);
                                            ""
                                        }
                                    };

                                    Self::deadletter(line_str, &buffers);

                                    d += 1;

                                    continue
                                }
                            };


                            // println!("Slow path ingest: {}", msg);

                            // update metadata in runtime and ingest message
                            if Config::get_auto_approve() {

                                if updated_schema_clone.lock().unwrap().as_str() == "yes" {
                                    {
                                        // METADATA.write().clear();
                                        // METADATA.write().extend(NEW_METADATA.read().clone());

                                        // let skpr_namespace = BufferChunker::decode_file_namespace(&output_file_name);

                                        let metadata = METADATA.read();
                                        let default_message = create_default_nested_message(&metadata.get(&skpr_namespace).unwrap().fields);
                                        let mut lock = DEFAULT_NESTED_MESSAGE.write();
                                        lock.insert(skpr_namespace.clone(), default_message);

                                        Ingest::prepare_arrow_schema(&skpr_namespace, flatten).unwrap();

                                        let schema = ARROW_SCHEMA.read();
                                        run_id = format!("{:?}", md5::compute(format!("{:?}", schema.deref())));

                                        println!("Updated schema, new run id: {}", run_id);

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

                                Self::deadletter(line_str, &buffers);

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

                    // println!("run_id: {}", run_id);

                    let schema =  ARROW_SCHEMA.read().get(&skpr_namespace).unwrap().clone();
                    run_id = format!("{:?}", md5::compute(format!("{:?}", schema.deref())));

                    let buf_entry = buf.entry((
                        skpr_namespace.clone(),
                        skpr_partition.clone(),
                        skpr_time_bucket.clone(),
                        run_id.clone(),
                    )).or_insert_with(|| {

                        // let arrow_schema = ARROW_SCHEMA.read().get(&skpr_namespace).unwrap().clone();

                        IngestBufferBatch {
                            offset: OffsetKeySerialize {
                                position: 0,
                                source_namespace: ingest_batch.offset_key.namespace.clone(),
                                source_partition: ingest_batch.offset_key.partition.clone(),
                            },
                            namespace: skpr_namespace.clone(),
                            partition: skpr_partition.clone(),
                            time: skpr_time_bucket,
                            shard: run_id.clone(),
                            records: Vec::new(),
                            schema: schema
                        }
                    });

                    buf_entry.offset.position = batch_line;
                    buf_entry.records.push(ingest_record);

                    j += 1;

                    // offset_db_clone.insert(&ingest_batch.offset_key, OffsetTypes::Line, batch_line);


                }
                // else {
                //     println!("Skipping line {} in batch", batch_line);
                // }

            }



            batch_offset_lines.insert(ingest_batch.offset_key.clone(), batch_line);

            // offset_db_clone.insert(&ingest_batch.offset_key, OffsetTypes::Closed, 1);
            batch_offset_files.insert(ingest_batch.offset_key.clone(), 1);

        }

        // write and flush each buf to Buffer in buffers.buffers
        // for (namespace, ingest_buffer_batch) in buf {
        //
        //     let buffer = buffers.buffers.entry(namespace.clone()).or_insert_with(|| {
        //         TimedRwLock::new(namespace.clone(), Buffer::new(&namespace.clone()))
        //     });
        //
        //     buffer.write().write(ingest_buffer_batch);
        //     buffer.write().flush().unwrap();
        // }

        // @todo - write() Buffers
        buffers.write(buf);

        buffers.flush().await.unwrap();


        // for buffer in buffers.buffers.iter() {
        //     let mut buffer_lock = buffer.write();
        //     buffer_lock.flush().unwrap();
        // }

        batch_offset_lines.iter().for_each(|(offset_key, i)| {
            offset_db_clone.insert(offset_key, OffsetTypes::Line, *i);
        });

        batch_offset_files.iter().for_each(|(offset_key, i)| {
            offset_db_clone.insert(offset_key, OffsetTypes::Closed, 1);
        });

        offset_db_clone.flush();

        if *updated_schema_clone.lock().unwrap() == "yes".to_string() {
            *updated_schema_clone.lock().unwrap() = "no".to_string();

            // update metadata at control pane, this may or may not be automatically approved
            // tokio::runtime::Builder::new_multi_thread()
            //     .enable_all()
            //     .build()
            //     .unwrap()
            //     .block_on(async {
            //         Config::set_metadata(&METADATA.read(), true).await;
            //     });

            Config::set_metadata(&METADATA.read(), true).await;
        }

        let mut counter_lock = METRICS.write();
        counter_lock.deadletters_total += d;
        counter_lock.ingeted_slow_total += x;
        counter_lock.messages_total += i;
        counter_lock.bytes_current += bytes;
        counter_lock.bytes_total += bytes;

    }

    pub(crate) fn prepare_arrow_schema(skpr_namespace: &str, flatten: bool) -> Result<Arc<Schema>, ArrowError> {

        println!("Preparing schema for namespace: {}", skpr_namespace);

        let mut arrow_schema: Result<datatypes::Schema, ArrowError> = Ok(datatypes::Schema::empty());
        let mut schema_ref = Arc::new(datatypes::Schema::empty());

        let metadata = METADATA.read();

        if metadata.get(skpr_namespace).is_none() {
            return Err(ArrowError::SchemaError(format!("Failed to find metadata for namespace: {}", skpr_namespace)));
            // panic!("Failed to find metadata for namespace: {}", skpr_namespace);
        }

        let mut output_metadata: HashMap<String, Metadata> = HashMap::new();
        if flatten {
            let mut meta: HashMap<String, Metadata> = HashMap::new();

            crate::flatten_metadata(metadata.get(skpr_namespace).unwrap(), &mut meta);

            let mut flat: Metadata = Metadata::new().unwrap();
            flat.fields = Box::new(meta);
            output_metadata.insert(skpr_namespace.to_string(), flat);
        } else {
            output_metadata = metadata.clone();
        }

        // println!("Preparing schema for namespace: {}", skpr_namespace);
        // println!("Schema: {:?}", output_metadata);

        // let skpr_partition = BufferChunker::decode_file_partition(filename);
        // let shard = BufferChunker::decode_file_shard(filename);
        // let source_time = BufferChunker::decode_file_time(filename);
        // let mut skpr_time = None;
        // if source_time >= 0 {
        //     skpr_time = Some(source_time);
        // }

        arrow_schema = convert_skippr_to_arrow(
            output_metadata.get(skpr_namespace).unwrap().fields.clone(),
        );

        schema_ref = Arc::new(arrow_schema.unwrap());

        ARROW_SCHEMA.write().insert(skpr_namespace.to_string(), schema_ref.clone());

        Ok(schema_ref)
    }


}


