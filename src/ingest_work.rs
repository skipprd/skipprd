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
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parquet::data_type::AsBytes;
use crate::ingest::fast_ingest::fast_path_ingest;

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

pub static PARSE_NAMESPACE_CACHE: Lazy<Mutex<HashMap<String, String>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub static OUTPUT_FILES_STATIC: Lazy<Mutex<LruCache<String, OutputFile>>> =
    Lazy::new(|| Mutex::new(LruCache::new(NonZeroUsize::new(100).expect(""))));

pub struct Ingest {}

impl Ingest {
    pub fn new() -> Ingest {
        Ingest {}
    }

    pub fn flush_buffers(force: bool, output_files: &mut MutexGuard<LruCache<String, OutputFile>>) {
        let data_dir = Config::get_data_dir();
        let output_dir = format!("{}/output", data_dir);

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
        datas: Vec<IngestBatch>,
        offset_db: &Arc<Offsets>,
    ) {
        let flatten = Config::truth_value(&Config::getenv("TRANSFORM_FLATTEN_EVENTS", "no"));

        let data_dir = Config::get_data_dir();
        let output_dir = format!("{}/output", data_dir);

        let aprox_now = SystemTime::now();

        let updated_schema: Arc<Mutex<String>> = Arc::new(Mutex::new("no".to_string()));

        let updated_schema_clone = updated_schema;
        let offset_db_clone = offset_db.clone();

        let mut buffers: Buffers = Buffers::new();

        let mut output_files = match OUTPUT_FILES_STATIC.lock() {
            Ok(output_files) => output_files,
            Err(err) => {
                panic!("Could not lock buffer files, Error: {:?}", err);
            }
        };

        let mut bytes: u64 = 0;
        let mut i = 0;
        let mut j = 0;
        let mut d = 0;

        for ingest_batch in datas {

            let has_offsets =
                offset_db_clone.validate(&ingest_batch.offset_key, OffsetTypes::Closed, 0);

            let records: Vec<Value> = SerdeJson::deserialize(&ingest_batch.data);

            for record in records {
                if record.is_null()
                    || (record.is_object() && record.as_object().unwrap().is_empty())
                    || (record.is_array() && record.as_array().unwrap().is_empty())
                {

                    i += 1;
                    d += 1;

                    continue;
                }

                if has_offsets.is_none()
                    || Some(false)
                        != offset_db_clone.validate(&ingest_batch.offset_key, OffsetTypes::Line, i)
                {

                    let skpr_namespace = Helpers::parse_namespace_field(
                        &record,
                        Config::get_pipeline_name(),
                        &mut PARSE_NAMESPACE_CACHE.lock().unwrap(),
                    );
                    let skpr_partition = Helpers::parse_partition_field(&record);
                    let skpr_time = Helpers::parse_time_field(&record);

                    let mut skpr_time_bucket: Option<i64> = None;

                    if skpr_time.is_some() {
                        skpr_time_bucket =
                            Some(BufferChunker::event_time_bucket(skpr_time.unwrap()));
                    }

                    let output_file_name = BufferChunker::encode_chunk_name(
                        "ingest",
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
                        Err(_err) => {
                            let mut metadata = METADATA.write().unwrap();
                            // println!("Falling back to slow path due to: {}", err);
                            let msg = ingest(
                                &record,
                                &mut metadata.get_mut(&skpr_namespace).unwrap().fields,
                                &mut updated_schema_clone.lock().unwrap(),
                                flatten,
                            );

                            msg
                            // msg.to_string() + "\n"

                        }
                    };

                    let record_vec = serde_json::to_vec(&record_value).unwrap();
                    bytes += record_vec.len() as u64;
                    buffers.write(&output_file_name, &record_vec);
                    buffers.write(&output_file_name, "\n".as_bytes());

                    i += 1;
                    j += 1;

                    offset_db_clone.insert(&ingest_batch.offset_key, OffsetTypes::Line, i);

                }
            }

            offset_db_clone.insert(&ingest_batch.offset_key, OffsetTypes::Closed, 1);
        }

        // Flush all buffers to their respective files.
        for (filename, buffer) in buffers.buffers.iter() {
            let output_file = format!("{}/{}", output_dir.clone(), &filename);

            if output_files.peek(filename).is_none() {
                let f = OpenOptions::new()
                    .create(true)
                    .write(true)
                    .append(true)
                    .open(output_file.clone())
                    .unwrap();

                let writer = BufWriter::with_capacity(WRITE_BUF_SIZE, f);

                let new_file = match std::fs::metadata(&output_file) {
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

        let mut counter_lock = METRICS.lock().unwrap();
        counter_lock.deadletters_total += d;
        counter_lock.ingeted_current += j;
        counter_lock.messages_total += i;
        counter_lock.bytes_current += bytes;

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
