use crate::buffer::BufferChunker;
use crate::discover::Metadata;
use crate::helpers::configuration::{Config};
use crate::helpers::offsets::{OffsetKey, OffsetTypes, Offsets};
use crate::helpers::Helpers;
use crate::ingest::ingest::ingest;
use crate::serdes::json::SerdeJson;
use crate::{METADATA, METRICS};
use glob::{glob_with, MatchOptions};
use lru::LruCache;
use once_cell::sync::Lazy;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::num::NonZeroUsize;
use std::ops::Deref;
use std::string::ToString;
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockWriteGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use threadpool::ThreadPool;
use std::sync::mpsc::channel;
extern crate num_cpus;
use std::sync::mpsc::Sender;
use std::sync::atomic::{AtomicUsize, Ordering};
use arrow::json::ReaderBuilder;

use parquet::data_type::AsBytes;
use crate::ingest::fast_ingest::fast_path_ingest;

use avro_rs::{Writer, Schema};
use nix::libc::exit;
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
    pub(crate) bytes: u64,
    pub(crate) upated_at: SystemTime,
    pub(crate) file: BufWriter<File>,
    pub(crate) rotated: Option<bool>,
}

// Bare metal platforms usually have very small amounts of RAM
// (in the order of hundreds of KB)
pub const WRITE_BUF_SIZE: usize = if cfg!(target_os = "espidf") {
    512
} else {
    512 * 1024
};

pub static PARSE_NAMESPACE_CACHE: Lazy<RwLock<HashMap<String, String>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

pub static OUTPUT_FILES_STATIC: Lazy<RwLock<LruCache<String, OutputFile>>> =
    Lazy::new(|| RwLock::new(LruCache::new(NonZeroUsize::new(100).expect(""))));

pub static deadletter_file_name: Lazy<String> = Lazy::new(|| BufferChunker::encode_chunk_name(
    "deadletters",
    Some(Config::get_pipeline_name().as_str()),
    None,
    None));

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
        while self.active_count.load(Ordering::SeqCst) > 0 {
            println!("Waiting for {} ingest tasks to finish", self.active_count.load(Ordering::SeqCst));

            // Here you can do other work while waiting for threads to finish,
            // or just sleep for a while if there's nothing else to do.
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}

impl Ingest {
    pub fn new() -> Ingest {
        // get number of cpus with a minimum of 2
        let num_cpus = num_cpus::get().max(2);
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

    pub fn flush_buffers(force: bool, output_files: &mut RwLockWriteGuard<LruCache<String, OutputFile>>) {
        let data_dir = Config::get_data_dir();
        let output_dir = format!("{}/ingest_buffer", data_dir);

        let mut rotated_files: Vec<String> = Vec::new();

        for (filename, output_file) in output_files.iter_mut() {
            output_file
                .file
                .flush()
                .expect(&format!("Could not flush file {}", filename));

            if force
                || Ingest::is_file_size_exceeded(&output_file)
                || Ingest::is_file_time_exceeded(&output_file)
            {
                if output_file.bytes > 0 {
                    // don't flush empty files when forced
                    // output_file.bytes = 0;
                    // output_file.upated_at = UNIX_EPOCH;
                    // output_file.rotated = Some(true);

                    let new_filename = format!(
                        "{}/done/{}-{}",
                        output_dir,
                        Helpers::random_str(12),
                        &filename
                    );
                    let old_path = format!("{}/{}", output_dir, &filename);

                    match fs::rename(&old_path, &new_filename) {
                        Ok(_) => {}
                        Err(_) => {}
                    };

                    rotated_files.push(filename.to_string());
                }
            }
        }

        for filename in rotated_files {
            output_files.pop(&filename);
        }

        if force {
            let options = MatchOptions {
                case_sensitive: false,
                require_literal_separator: false,
                require_literal_leading_dot: false,
            };

            for entry in glob_with(&format!("{}/*", output_dir), options)
                .expect("Failed to read glob pattern")
            {
                match entry {
                    Ok(path) => {
                        if path.is_dir() {
                            break;
                        }

                        let new_filename = format!(
                            "{}/done/{}-{}",
                            output_dir,
                            Helpers::random_str(12),
                            path.file_name().unwrap().to_str().unwrap()
                        );
                        let old_path = format!("{}", path.display().to_string());

                        println!(
                            "Flushing orphaned ingest buffer: {} to output: {}",
                            path.display().to_string(),
                            new_filename
                        );

                        match fs::rename(&old_path, &new_filename) {
                            Ok(_) => {}
                            Err(err) => {
                                println!("Error: {}", err)
                            }
                        };
                    }
                    _ => {}
                }
            }
        }
    }

    pub fn ingest_file(
        &self,
        datas: Vec<IngestBatch>,
        offset_db: &Arc<Offsets>,
    ) {
        // println!("Ingesting {} events", datas.len());

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

                let datas_clone = datas.clone();

                let core_count = self.thread_pool.active_count();

                self.thread_pool.execute(move || {
                    // println!("Processing batch of {} events on core {}", datas_clone.len(), core_count);
                    Ingest::process_batch(datas_clone, &offset_db_clone);
                    tx.send(()).unwrap();
                });

            // }
        // }

    }

    fn process_batch(
        datas: Vec<IngestBatch>,
        offset_db_clone: &Arc<Offsets>
    ) {

        // let mut avro_schemas = AVRO_SCHEMA.lock().unwrap();

        let flatten = Config::truth_value(&Config::getenv("TRANSFORM_FLATTEN_EVENTS", "no"));

        let data_dir = Config::get_data_dir();
        let output_dir = format!("{}/ingest_buffer", data_dir);
        let deadletter_dir = format!("{}/deadletter_buffer", data_dir);

        let aprox_now = SystemTime::now();

        let updated_schema: Arc<Mutex<String>> = Arc::new(Mutex::new("no".to_string()));

        let updated_schema_clone = updated_schema;
        // let offset_db_clone = offset_db.clone();

        let mut buffers: Buffers = Buffers::new();

        let mut bytes: u64 = 0;
        let mut i = 0;
        let mut j = 0;
        let mut d = 0;
        let mut x = 0;

        for ingest_batch in datas {

            let has_offsets =
                offset_db_clone.validate(&ingest_batch.offset_key, OffsetTypes::Closed, 0);

            let records: Vec<Value> = SerdeJson::deserialize(&ingest_batch.data);

            for record in records {
                if record.is_null()
                    || (record.is_object() && record.as_object().unwrap().is_empty())
                    || (record.is_array() && record.as_array().unwrap().is_empty())
                {

                    let output_file = format!("{}/{}", deadletter_dir.clone(), &deadletter_file_name.as_str());

                    buffers.write(&output_file, record.to_string().as_bytes());
                    buffers.write(&output_file, "\n".as_bytes());

                    i += 1;
                    d += 1;

                    continue;
                }

                if has_offsets.is_none()
                    || Some(false)
                        != offset_db_clone.validate(&ingest_batch.offset_key, OffsetTypes::Line, i)
                {

                    let mut namesapce_cache =  PARSE_NAMESPACE_CACHE.read().unwrap().clone();
                    let skpr_namespace = Helpers::parse_namespace_field(
                        &record,
                        Config::get_pipeline_name(),
                        &mut namesapce_cache,
                    );

                    if namesapce_cache != PARSE_NAMESPACE_CACHE.read().unwrap().clone() {
                        PARSE_NAMESPACE_CACHE.write().unwrap().clear();
                        PARSE_NAMESPACE_CACHE.write().unwrap().extend(namesapce_cache);
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
                    );

                    if METADATA.read().unwrap().get(&skpr_namespace).is_none() {
                        {
                            METADATA.write().unwrap().insert(skpr_namespace.clone(), Metadata::new().unwrap());
                        }
                    }

                    let msg = fast_path_ingest(
                        &record,
                        &METADATA.read().unwrap().get(&skpr_namespace).unwrap().fields,
                        flatten,
                    );

                    let record_value = match msg {
                        Ok(msg) => {
                            // msg.to_string() + "\n"
                            // buffers.write(&output_file_name, buf_str.as_bytes());
                            msg
                        },
                        Err(err) => {
                            let mut metadata = METADATA.write().unwrap();
                            // println!("Falling back to slow path due to: {}", err);
                            let msg = ingest(
                                &record,
                                &mut metadata.get_mut(&skpr_namespace).unwrap().fields,
                                &mut updated_schema_clone.lock().unwrap(),
                                flatten,
                            );

                            x += 1;

                            msg
                            // Value::Null

                        }
                    };

                    let output_file = format!("{}/{}", output_dir.clone(), &output_file_name);

                    let record_vec = serde_json::to_vec(&record_value).unwrap();
                    bytes += record_vec.len() as u64;
                    buffers.write(&output_file, &record_vec);
                    buffers.write(&output_file, "\n".as_bytes());

                    i += 1;
                    j += 1;

                    offset_db_clone.insert(&ingest_batch.offset_key, OffsetTypes::Line, i);

                }
            }

            offset_db_clone.insert(&ingest_batch.offset_key, OffsetTypes::Closed, 1);
        }


        let mut output_files = match OUTPUT_FILES_STATIC.write() {
            Ok(output_files) => output_files,
            Err(err) => {
                panic!("Could not lock buffer files, Error: {:?}", err);
            }
        };

        // Flush all buffers to their respective files.
        for (filename, buffer) in buffers.buffers.iter() {

            if output_files.peek(filename).is_none() {
                let f = match OpenOptions::new()
                    .create(true)
                    .write(true)
                    .append(true)
                    .open(filename.clone()) {
                        Ok(f) => f,
                        Err(err) => {
                            panic!("Could not open file: {}, Error: {:?}", filename, err);
                        }
                    };

                let writer = BufWriter::with_capacity(WRITE_BUF_SIZE, f);

                let new_file = match std::fs::metadata(&filename) {
                    Ok(metadata) => {
                        let secs_since_epoch = metadata
                            .modified()
                            .unwrap()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs();
                        let time = UNIX_EPOCH + Duration::from_secs(secs_since_epoch);

                        OutputFile {
                            bytes: metadata.len(),
                            upated_at: time,
                            file: writer,
                            rotated: None,
                        }
                    }
                    Err(_err) => OutputFile {
                        bytes: 0,
                        upated_at: aprox_now,
                        file: writer,
                        rotated: None,
                    },
                };

                // If the cache is full, remove and flush the least recently used item.
                if output_files.len() == output_files.cap().get() {
                    if let Some((filename, mut evicted)) = output_files.pop_lru() {
                        evicted
                            .file
                            .flush()
                            .expect(&format!("Could not flush file {}", filename));
                    }
                }

                output_files.put(filename.clone(), new_file);
            }

            if let Some(mut output_file) = output_files.get_mut(filename) {
                output_file.file.write_all(&buffer.data).unwrap();
                output_file.bytes += buffer.bytes;
                output_file.upated_at = aprox_now;
            }
        }

        Self::flush_buffers(false, &mut output_files);

        offset_db_clone.flush();

        buffers.clear_all();

        let mut counter_lock = METRICS.write().unwrap();
        counter_lock.deadletters_total += d;
        counter_lock.ingeted_current += j;
        counter_lock.ingeted_slow_current += x;
        counter_lock.messages_total += i;
        counter_lock.bytes_current += bytes;
        counter_lock.bytes_total += bytes;

        println!("Batch Msg Ingested: {}", j);
        println!("Batch Msg Fixed: {}", x);
        println!("Batch Deadletters: {}", d);
        // Summarize the bytes, rounding to the nearest MB/GB/TB as appropriate.
        let rounded_bytes = match bytes {
            0..=999_999 => format!("{}B", bytes),
            1_000_000..=999_999_999 => format!("{:.1}MB", bytes as f64 / 1_000_000.0),
            1_000_000_000..=999_999_999_999 => format!("{:.1}GB", bytes as f64 / 1_000_000_000.0),
            _ => format!("{:.1}TB", bytes as f64 / 1_000_000_000_000.0),
        };
        println!("Batch Bytes: {}", rounded_bytes);

        if *updated_schema_clone.lock().unwrap() == "yes".to_string() {
            *updated_schema_clone.lock().unwrap() = "no".to_string();

            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    Config::set_config(&METADATA.read().unwrap(), true).await;
                });
        }
    }

    fn is_file_size_exceeded(file: &OutputFile) -> bool {
        let buffer_size = Config::getenv("BUFFER_THRESHOLD_BYTES", "10485760"); // 10MB default
        file.bytes > buffer_size.parse::<u64>().unwrap()
    }

    fn is_file_time_exceeded(file: &OutputFile) -> bool {
        let ttl = Config::getenv("BUFFER_THRESHOLD_SECONDS", "300"); // 10MB default
        SystemTime::now()
            .duration_since(file.upated_at)
            .unwrap()
            .as_secs()
            > ttl.parse::<u64>().unwrap()
    }

    fn is_rotated(file: &OutputFile) -> bool {
        file.rotated.is_some()
    }
}
