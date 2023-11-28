use crate::buffer::{BUFFER_INDEX, BufferChunker};
use crate::discover::Metadata;
use crate::helpers::configuration::Config;
use crate::helpers::offsets::{OffsetKey, Offsets, OffsetTypes};
use crate::helpers::Helpers;
use crate::ingest::ingest::ingest;
use crate::serdes::json::SerdeJson;
use crate::{BUFFER_FINALISE_RUNNING, helpers, METADATA, METRICS, OUTPUT_GRACEFUL_SHUTDOWN_COMPLETE, OUTPUT_RUNNING, RUNNING};
use glob::{glob_with, MatchOptions};
use lru::LruCache;
use once_cell::sync::Lazy;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::process::exit;
use std::string::ToString;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use threadpool::ThreadPool;
use std::sync::mpsc::channel;
extern crate num_cpus;
use std::sync::mpsc::Sender;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::sleep;
use parquet::data_type::AsBytes;
use crate::ingest::fast_ingest::fast_path_ingest;

use arrow::datatypes;
use arrow::error::ArrowError;
use dashmap::DashMap;
use tokio::io::AsyncWriteExt;
use helpers::timed_rwlock::TimedRwLock;
use crate::serdes::csv::SerderCsv;
use crate::serdes::parquet::SerdeParquet;
use crate::serdes::xml::SerdeXml;
// use crate::converters::skippr_avro::convert_skippr_to_avro_field_types;

pub struct Buffer {
    data: Vec<u8>,
    bytes: u64,
}

impl Buffer {
    pub fn new() -> Self {
        Buffer {
            data: Vec::new(),
            bytes: 0,
        }
    }

    pub fn write(&mut self, data: &[u8]) {
        self.data.extend_from_slice(data);
        self.bytes += data.len() as u64;
    }
}

pub struct Buffers {
    pub(crate) buffers: HashMap<String, Buffer>,
}

impl Buffers {
    pub fn new() -> Self {
        Buffers {
            buffers: HashMap::new(),
        }
    }

    pub fn write(&mut self, key: &str, data: &[u8]) {
        let buffer = self.buffers.entry(key.to_string()).or_insert(Buffer::new());
        buffer.write(data);
    }

    pub fn clear(&mut self, key: &str) {
        if let Some(buffer) = self.buffers.get_mut(key) {
            buffer.data.clear();
            buffer.bytes = 0;
        }
    }

    pub fn clear_all(&mut self) {
        self.buffers = HashMap::new();
    }
}

#[derive(Clone, Debug)]
pub struct IngestBatch {
    pub(crate) offset_key: OffsetKey,
    pub(crate) data: String,
}

// Can't rely on file.metadata() as we don't know we're dealing with a unix FS. e.g. EFS
pub struct OutputFile {
    pub(crate) path: PathBuf,
    pub(crate) bytes: u64,
    pub(crate) updated_at: SystemTime,
    // non buffered writer
    pub(crate) file: Option<std::fs::File>,
    pub(crate) rotated: Option<bool>,
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

}

impl Drop for Ingest {
    fn drop(&mut self) {
        println!("Waiting for {} ingest tasks to finish", self.active_count.load(Ordering::SeqCst));

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

        Ingest {
            num_cpus,
            thread_pool,
            tx,
            active_count,
        }
    }

    pub fn wait_for_completion(&self) {

        let mut current_active_count = self.active_count.load(Ordering::SeqCst);

        while self.active_count.load(Ordering::SeqCst) > 0 {

            if (current_active_count != self.active_count.load(Ordering::SeqCst)) {
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

                // let core_count = self.thread_pool.active_count();

                self.thread_pool.execute(move || {
                    // println!("Processing batch of {} events on core {}", datas_clone.len(), core_count);
                    Ingest::process_batch(&mut datas_clone, &offset_db_clone,);
                    tx.send(()).unwrap();
                });

            // }
        }

    }

    fn deadletter(record: &str, buffers: &mut Buffers) {
        let data_dir = Config::get_data_dir();
        let deadletter_dir = format!("{}/deadletter_buffer", data_dir);

        let output_file = format!("{}/{}", deadletter_dir.clone(), &DEADLETTER_FILE_NAME.as_str());

        buffers.write(&output_file, record.as_bytes());
        buffers.write(&output_file, "\n".as_bytes());

    }

    fn process_batch(
        datas: &mut Vec<IngestBatch>,
        offset_db_clone: &Arc<Offsets>,
    ) {

        // let mut avro_schemas = AVRO_SCHEMA.lock().unwrap();

        let flatten = Config::truth_value(&Config::get_transform_config().flatten_events.or(Some("no".to_string())).unwrap());

        let data_dir = Config::get_data_dir();
        let output_dir = format!("{}/ingest_buffer", data_dir);
        let deadletter_dir = format!("{}/deadletter_buffer", data_dir);

        let aprox_now = SystemTime::now();

        let updated_schema: Arc<Mutex<String>> = Arc::new(Mutex::new("no".to_string()));

        let updated_schema_clone = updated_schema;
        // let offset_db_clone = offset_db.clone();

        let mut buffers: Buffers = Buffers::new();

        let mut bytes: u64 = 0;
        let mut i: u64 = 0;
        let mut j = 0;
        let mut d = 0;
        let mut x = 0;
        let mut batch_line: usize = 0;

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

        for ingest_batch in datas {

            let has_offsets =
                offset_db_clone.validate(&ingest_batch.offset_key, OffsetTypes::Closed, 0);

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

            let mut unwrapped_records: Vec<Value> = Vec::new();

            for record in records {
                match record.as_object() {
                    Some(v) => unwrapped_records.push(record),
                    None => {
                        match record.as_array() {
                            Some(v) => {
                                for item in v {
                                    // println!("Item: {}", item);
                                    unwrapped_records.push(item.clone());
                                }
                            },
                            None => {
                                let line_no = if batch_line == 0 || batch_line > ingest_batch.data.lines().count() {
                                    1
                                } else {
                                    batch_line - 1
                                };

                                // deadletter
                                let line_str = match ingest_batch.data.lines().nth(line_no) {
                                    Some(line) => line,
                                    None => ""
                                };

                                Self::deadletter(line_str, &mut buffers);

                                d += 1;

                            }
                        }
                    }
                };

            }

            batch_line = 0;

            for record in unwrapped_records {

                // println!("Record: {}", record);
                batch_line += 1;
                i += 1;

                if record.is_null()
                    || (record.is_object() && record.as_object().unwrap().is_empty())
                    || (record.is_array() && record.as_array().unwrap().is_empty())
                {

                    // let line batch_line in ingest_batch.data
                    let line_str = match ingest_batch.data.lines().nth(batch_line - 1) {
                        Some(line) => line,
                        None => {
                            // println!("Could not find null line {} in batch", batch_line);
                            // println!("Batch lines: {}", ingest_batch.data.lines().count());
                            // panic!("Could not find null line {} in batch", batch_line);
                            ""
                        }
                    };


                    Self::deadletter(line_str, &mut buffers);

                    d += 1;

                    continue;
                }

                if has_offsets.is_none()
                    || Some(false)
                        != offset_db_clone.validate(&ingest_batch.offset_key, OffsetTypes::Line, i)
                {

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

                    let output_file_name = BufferChunker::encode_chunk_name(
                        "output",
                        Some(&skpr_namespace),
                        Some(&skpr_partition),
                        skpr_time_bucket,
                        None
                    );

                    if METADATA.read().get(&skpr_namespace).is_none() {
                        METADATA.write().insert(skpr_namespace.clone(), Metadata::new().unwrap());
                        println!("New namespace: {}", skpr_namespace);
                    }

                    let msg = match METADATA.read().get(&skpr_namespace) {
                        Some(metadata) => {
                            fast_path_ingest(
                                &record,
                                metadata.fields.as_ref(),
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
                        Err(err) => {

                            // let old_metadata = NEW_METADATA.read().unwrap().clone();

                            // if NEW_METADATA.read().get(&skpr_namespace).is_none() {
                            //     NEW_METADATA.write().insert(skpr_namespace.clone(), Metadata::new().unwrap());
                            // }

                            // println!("Falling back to slow path due to: {}", err);
                            let msg = match ingest(
                                &record,
                                // &mut NEW_METADATA.write().get_mut(&skpr_namespace).unwrap().fields,
                                &mut METADATA.write().get_mut(&skpr_namespace).unwrap().fields,
                                &mut updated_schema_clone.lock().unwrap(),
                                flatten,
                            ) {
                                Ok(msg) => msg,
                                Err(err) => {
                                    // println!("Deadlettring - Could not ingest record: {}, Error: {:?}", record, err);

                                    // deadletter record
                                    let line_str = match ingest_batch.data.lines().nth(batch_line - 1) {
                                        Some(line) => line,
                                        None => {
                                            // println!("Could not find line {} in batch", batch_line);
                                            ""
                                        }
                                    };

                                    Self::deadletter(line_str, &mut buffers);

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
                                    }
                                }

                                x += 1;

                                msg

                            } else { // or just deadletter message for later approval
                                let line_str = match ingest_batch.data.lines().nth(batch_line - 1) {
                                    Some(line) => line,
                                    None => {
                                        // println!("Could not find line {} in batch", batch_line);
                                        ""
                                    }
                                };

                                Self::deadletter(line_str, &mut buffers);

                                d += 1;

                                continue;
                            }


                            // Value::Null

                        }
                    };

                    let output_file = format!("{}/{}.merged", output_dir.clone(), &output_file_name);

                    // let pretty_json = match serde_json::to_string_pretty(&record_value) {
                    //     Ok(pretty_json) => pretty_json,
                    //     Err(err) => {
                    //         println!("Could not pretty print record: {}, Error: {:?}", record_value, err);
                    //         "".to_string()
                    //     }
                    // };
                    // panic!("Pretty json: {}", pretty_json);

                    // Serialize your JSON value to a vector

                    let record_vec = serde_json::to_vec(&record_value).unwrap();
                    bytes += record_vec.len() as u64;
                    buffers.write(&output_file, &record_vec);
                    buffers.write(&output_file, "\n".as_bytes());

                    // i += 1;
                    j += 1;

                    offset_db_clone.insert(&ingest_batch.offset_key, OffsetTypes::Line, i);

                }

            }

            offset_db_clone.insert(&ingest_batch.offset_key, OffsetTypes::Closed, 1);
        }


        // @todo - I'd rather not lock the whole hashmap here, instead we should lock the individual files.
        // @todo - We should also be able to flush the buffers to disk in parallel.
        // @todo - Ideally this would not be a blocking operation.
        // @todo - We probably want to track file metadata in a persistent store, so we can recover from crashes. FS metadata is not reliably available.



            // update metadata at control pane, this may or may not be automatically approved
            // tokio::runtime::Builder::new_multi_thread()
            //     .enable_all()
            //     .build()
            //     .unwrap()
            //     .block_on(async {

                    // Flush all buffers to their respective files.
                    for (new_filename, buffered_records) in buffers.buffers.iter() {

                        let mut buffer = vec![0; 4096]; // Chunk size can be adjusted

                        let has_file: bool = {
                            let index_guard = BUFFER_INDEX.read();
                            let index = match index_guard.get("ingest_buffer_merged") {
                                Some(index) => index,
                                None => {
                                    println!("Failed to find index ingest_buffer_merged");
                                    return;
                                }
                            };
                            let result = match index.get_mut(new_filename) {
                                Some(new_file_ref) => {
                                    // println!("Found file in index {}", new_filename);
                                    true
                                },
                                None => {
                                     false
                                }
                            };

                            result
                        };

                        if !has_file {
                            BufferChunker::create_and_insert_new_file(new_filename.to_string());
                        }

                        let mut index_guard = BUFFER_INDEX.read();
                        let index = index_guard.get_mut("ingest_buffer_merged").unwrap();
                        let mut new_file_ref = index.get_mut(new_filename).expect("Failed to find file");

                        let new_file = new_file_ref.value_mut();


                        let mock_buffer = vec![0; 0];
                        // check file descriptor is open
                        if new_file.file.is_none()
                            || new_file.file.as_mut().is_none()
                            // || new_file.file.as_mut().unwrap().metadata().is_err()
                            || new_file.file.as_mut().unwrap().write(&mock_buffer).is_err()
                            // || new_file.file.as_mut().unwrap().flush().is_err()
                        {
                            // println!("File descriptor changed, re-creating {}", new_filename);

                            let file = match std::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(&new_filename) {
                                Ok(file) => file,
                                Err(err) => {
                                    panic!("Failed to open indexed buffer file {}: {}", new_filename, err);
                                }
                            };

                            new_file.file = Some(file);
                        }


                        let mut total_bytes = 0;

                            buffer = buffered_records.data.clone();

                            match new_file.file.as_mut() {
                                Some(file) => {
                                    match file.write_all(&buffer) {
                                        Ok(_) => {
                                            // println!("Wrote {} bytes to buffer file {}", buffer.len(), new_filename);
                                        }
                                        Err(err) => {
                                            println!("Error writing to buffer file {}: {}", new_filename, err)
                                        }
                                    }
                                },
                                None => {
                                   panic!("Failed to find file descriptor when writing to buffer file {}", new_filename);
                                }
                            }

                            new_file.bytes += buffered_records.bytes;


                        match new_file.file.as_mut().expect(&format!("Failed to find file descriptor when flushing buffer file"))
                            .flush() {
                            Ok(_) => {
                                // println!("Flushed buffer file {}", new_filename);
                            }
                            Err(err) => {
                                println!("Error flushing buffer file {}: {}", new_filename, err)
                            }
                        }
                        match new_file.file.as_mut().expect(&format!("Failed to find file descriptor when syncing buffer file metadata"))
                            .sync_all() {
                            Ok(_) => {
                                // println!("Synced buffer file metadata {}", new_filename);
                            }
                            Err(err) => {
                                println!("Error syncing buffer file metadata {}: {}", new_filename, err)
                            }
                        }

                        new_file.updated_at = SystemTime::now();

                        BufferChunker::finalise_buffers(false, &new_file, &new_filename);

                    }
                // });

        offset_db_clone.flush();

        buffers.clear_all();

        if *updated_schema_clone.lock().unwrap() == "yes".to_string() {
            *updated_schema_clone.lock().unwrap() = "no".to_string();

            // update metadata at control pane, this may or may not be automatically approved
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    Config::set_metadata(&METADATA.read(), true).await;
                    // BufferChunker::rotate_buffers(false).await;
                });
        }

        let mut counter_lock = METRICS.write();
        counter_lock.deadletters_total += d;
        counter_lock.ingeted_slow_total += x;
        counter_lock.messages_total += i;
        counter_lock.bytes_current += bytes;
        counter_lock.bytes_total += bytes;

        // println!("Batch Msg Ingested: {}", j);
        // println!("Batch Msg Fixed: {}", x);
        // println!("Batch Deadletters: {}", d);
        // // Summarize the bytes, rounding to the nearest MB/GB/TB as appropriate.
        // let rounded_bytes = match bytes {
        //     0..=999_999 => format!("{}B", bytes),
        //     1_000_000..=999_999_999 => format!("{:.1}MB", bytes as f64 / 1_000_000.0),
        //     1_000_000_000..=999_999_999_999 => format!("{:.1}GB", bytes as f64 / 1_000_000_000.0),
        //     _ => format!("{:.1}TB", bytes as f64 / 1_000_000_000_000.0),
        // };
        // println!("Batch Bytes: {}", rounded_bytes);


    }


}


