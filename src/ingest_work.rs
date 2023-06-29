use crate::buffer::BufferChunker;
use crate::discover::Metadata;
use crate::helpers::configuration::{Config, Metrics};
use crate::helpers::offsets::{OffsetKey, OffsetTypes, Offsets};
use crate::helpers::Helpers;
use crate::ingest::ingest_fast::fast_path_ingest;
use crate::serdes::json::SerdeJson;
use once_cell::sync::Lazy;
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::sync::{Arc, Mutex, MutexGuard};
use std::{fs};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use glob::{glob_with, MatchOptions};
use crate::{RUNNING};
use lru::LruCache;
use std::num::NonZeroUsize;


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
pub const WRITE_BUF_SIZE: usize = if cfg!(target_os = "espidf") { 512 } else { 4 * 1024 };

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

        for (filename, output_file) in output_files.iter_mut() {
            output_file.file.flush().expect(&format!("Could not flush file {}", filename));

            if force || Ingest::is_file_size_exceeded(&output_file) || Ingest::is_file_time_exceeded(&output_file) {
                if output_file.bytes > 0 { // don't flush empty files when forced
                    output_file.bytes = 0;
                    output_file.upated_at = UNIX_EPOCH;
                    output_file.rotated = Some(true);

                    let new_filename = format!(
                        "{}/done/{}-{}",
                        output_dir,
                        Helpers::random_str(12),
                        filename
                    );
                    let old_path = format!("{}/{}", output_dir, filename);

                    match fs::rename(&old_path, &new_filename) {
                        Ok(_) => {},
                        Err(_) => {}
                    };
                }
            }
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

                        println!("Flushing orphaned ingest buffer: {} to output: {}", path.display().to_string(), new_filename);

                        match fs::rename(&old_path, &new_filename) {
                            Ok(_) => {},
                            Err(err) => {println!("Error: {}", err)}
                        };
                    }
                    _ => {}
                }
            }
        }
    }

    pub fn ingest_file(
        datas: Vec<IngestBatch>,
        metadata: &Arc<Mutex<HashMap<String, Metadata>>>,
        metrics: &Arc<Mutex<Metrics>>,
        offset_db: &Arc<Offsets>,
    ) {

        let flatten = Config::truth_value(&Config::getenv("TRANSFORM_FLATTEN_EVENTS", "no"));

        let data_dir = Config::get_data_dir();
        let output_dir = format!("{}/output", data_dir);

        let aprox_now = SystemTime::now();

        let updated_schema: Arc<Mutex<String>> = Arc::new(Mutex::new("no".to_string()));

        let updated_schema_clone = updated_schema;
        let metadata_clone = metadata.clone();
        let metrcis_clone = metrics.clone();
        let offset_db_clone = offset_db.clone();

        let mut output_files = match OUTPUT_FILES_STATIC.lock() {
            Ok(output_files) => output_files,
            Err(err) => {
                panic!("Could not lock buffer files, Error: {:?}", err);
            },
        };

        let mut bytes: u64 = 0;
        let mut i = 0;
        let mut j = 0;

        for ingest_batch in datas {
            let mut buf_str: String = String::new();

            // have offsets, don't bother checking each line offset if not.
            // relevant when processing a new file, which is most of the time
            let has_offsets =
                offset_db_clone.validate(&ingest_batch.offset_key, OffsetTypes::Closed, 0);

            let records: Vec<Value> = SerdeJson::deserialize(&ingest_batch.data);

            for mut record in records {

                    if record.is_null()
                        || (record.is_object() && record.as_object().unwrap().is_empty())
                        || (record.is_array() && record.as_array().unwrap().is_empty())
                    {

                        // println!("{}", &ingest_batch.data);
                        let mut counter_lock = metrcis_clone.lock().expect("Could not lock metrics for deadletter stats");
                        counter_lock.deadletters_total += 1;

                        i += 1;

                        continue;
                    }

                    if has_offsets.is_none()
                        || Some(false)
                        != offset_db_clone.validate(&ingest_batch.offset_key, OffsetTypes::Line, i)
                    {
                        let usize = serde_json::to_vec(&record).unwrap().len();
                        let record_bytes: u64 = usize.try_into().unwrap();
                        bytes += record_bytes;

                        let skpr_namespace = Helpers::parse_namespace_field(
                            &record,
                            Config::get_pipeline_name(),
                            &mut PARSE_NAMESPACE_CACHE.lock().unwrap(),
                        );
                        let skpr_partition = Helpers::parse_partition_field(&record);
                        let skpr_time = Helpers::parse_time_field(&record);

                        let mut skpr_time_bucket: Option<i64> = None;

                        if skpr_time.is_some() {
                            skpr_time_bucket = Some(BufferChunker::event_time_bucket(skpr_time.unwrap()));
                        }

                        let output_file_name = BufferChunker::encode_chunk_name(
                            "ingest",
                            Some(&skpr_namespace),
                            Some(&skpr_partition),
                            skpr_time_bucket,
                        );
                        let output_file = format!("{}/{}", output_dir.clone(), &output_file_name);

                        if output_files.peek(&output_file_name).is_none() {

                            println!("Creating new output file: {}", output_file_name);

                            let f = OpenOptions::new()
                                .create(true)
                                .write(true)
                                .append(true)
                                .open(output_file.clone())
                                .unwrap();

                            let mut writer = BufWriter::with_capacity(WRITE_BUF_SIZE, f);

                            let new_file = match std::fs::metadata(&output_file) {
                                Ok(metadata) => {

                                    println!("Re-opening existing file: {}", output_file);

                                    let secs_since_epoch = metadata.modified().unwrap().duration_since(UNIX_EPOCH).unwrap().as_secs();
                                    let time = UNIX_EPOCH + Duration::from_secs(secs_since_epoch);

                                    OutputFile {
                                        bytes: metadata.len(),
                                        upated_at: time,
                                        file: writer,
                                        rotated: None
                                    }
                                },
                                Err(err) => {

                                    println!("Creating new file: {}", output_file);

                                    OutputFile {
                                        bytes: record_bytes,
                                        upated_at: aprox_now,
                                        file: writer,
                                        rotated: None
                                    }
                                }
                            };

                            // If the cache is full, remove and flush the least recently used item.
                            if output_files.len() == output_files.cap().get() {
                                if let Some((filename, mut evicted)) = output_files.pop_lru() {
                                    println!("Evicting and flushing file: {}", filename);
                                    evicted.file.flush().expect(&format!("Could not flush file {}", filename));
                                }
                            }

                            output_files.put(output_file_name.clone(), new_file);
                        }

                        let mut meta = metadata_clone.lock().unwrap();
                        if meta.get(&skpr_namespace).is_none() {
                            meta.insert(skpr_namespace.clone(), Metadata::new().unwrap());
                        }

                        let msg = fast_path_ingest(
                            &record,
                            &mut meta.get_mut(&skpr_namespace).unwrap().fields,
                            &mut updated_schema_clone.lock().unwrap(),
                            flatten
                        );

                        buf_str = msg.to_string() + "\n";

                        {
                            let mut output_file = output_files
                                .get_mut(&output_file_name)
                                .unwrap();
                            output_file.file.write_all(buf_str.as_bytes()).unwrap();
                            output_file.bytes += record_bytes;
                            output_file.upated_at = aprox_now;
                        }

                        buf_str.clear();

                        i += 1;
                        j += 1;

                        offset_db_clone.set(&ingest_batch.offset_key, OffsetTypes::Line, i);

                    }
            }

            offset_db_clone.set(&ingest_batch.offset_key, OffsetTypes::Closed, 1);

        }

        Self::flush_buffers(false, &mut output_files);
        drop(output_files);

        offset_db_clone.flush();

        let mut counter_lock = metrcis_clone.lock().unwrap();
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
                    Config::set_config(
                        &metadata_clone.lock().unwrap(),
                        true,
                    )
                    .await;
                });

        }

    }

    fn is_file_size_exceeded(file: &OutputFile) -> bool {
        let buffer_size = Config::getenv("BUFFER_THRESHOLD_BYTES", "10485760"); // 10MB default
        file.bytes > buffer_size.parse::<u64>().unwrap()
    }

    fn is_file_time_exceeded(file: &OutputFile) -> bool {
        let ttl = Config::getenv("BUFFER_THRESHOLD_SECONDS", "300"); // 10MB default
        SystemTime::now().duration_since(file.upated_at).unwrap().as_secs() > ttl.parse::<u64>().unwrap()
    }

    fn is_rotated(file: &OutputFile) -> bool {
        file.rotated.is_some()
    }
}
