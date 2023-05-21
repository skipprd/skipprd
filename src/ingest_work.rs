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
use std::io::Write;
use std::sync::{Arc, Mutex, MutexGuard};
use std::{fs};
use std::time::SystemTime;

#[derive(Clone, Debug)]
pub struct IngestBatch {
    pub(crate) offset_key: OffsetKey,
    pub(crate) data: String,
}

// Can't rely on file.metadata() as we don't know we're dealing with a unix FS. e.g. EFS
pub struct OutputFile {
    pub(crate) bytes: u64,
    pub(crate) seconds: u64,
    pub(crate) file: File,
}

// const MAX_BUFFER_SIZE: u64 = 1024 * 1024 * 10;

static parse_namespace_cache: Lazy<Mutex<HashMap<String, String>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub static OUTPUT_FILES_STATIC: Lazy<Mutex<HashMap<String, OutputFile>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub struct Ingest {}

impl Ingest {
    pub fn new() -> Ingest {
        Ingest {}
    }

    pub fn flush_buffers(force: bool, output_files: &mut MutexGuard<HashMap<String, OutputFile>>) {
    // pub fn flush_buffers(force: bool) {

        let data_dir = Config::get_data_dir();
        let output_dir = format!("{}/output", data_dir);

        for (filename, output_file) in output_files.iter_mut() {

            output_file.file.flush().expect(&format!("Could not flush file {}", filename));

            // let mut file= &output_file.file;
            // file.flush().expect(&format!("Could not flush file {}", filename));

            if force || Ingest::is_file_size_exceeded(&output_file) || Ingest::is_file_time_exceeded(&output_file) {
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

                // println!("Rotated buffer file {}", new_filename);
            }
        }
    }

    pub fn ingest_file(
        datas: Vec<IngestBatch>,
        metadata: &Arc<Mutex<HashMap<String, Metadata>>>,
        metrics: &Arc<Mutex<Metrics>>,
        offset_db: &Arc<Offsets>,
    ) {
        let data_dir = Config::get_data_dir();
        let output_dir = format!("{}/output", data_dir);

        let faltten_events = &Config::getenv("DATA_SOURCE_FLATTEN_EVENTS", "no");

        let updated_schema: Arc<Mutex<String>> = Arc::new(Mutex::new("no".to_string()));

        let updated_schema_clone = updated_schema;
        let metadata_clone = metadata.clone();
        let metrcis_clone = metrics.clone();
        let offset_db_clone = offset_db.clone();

        let output_files = &mut OUTPUT_FILES_STATIC.lock().unwrap();

        // thread::spawn(move || {
        for ingest_batch in datas {
            let mut buf_str: String = String::new();

            // datas.par_iter().map( |ingest_batch| {
            // have offsets, don't bother checking each line offset if not.
            // relevant when processing a new file, which is most of the time
            let has_offsets =
                offset_db_clone.validate(&ingest_batch.offset_key, OffsetTypes::Closed, 0);

            let mut bytes: u64 = 0;
            let mut i = 1;
            let mut j = 1;

            let records: Vec<Value> = SerdeJson::deserialize(&ingest_batch.data);

            for mut record in records {
                if record.is_null() {

                    // println!("{}", &ingest_batch.data);
                    let mut counter_lock = metrcis_clone.lock().unwrap();
                    counter_lock.deadletters_total += 1;

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
                        &mut parse_namespace_cache.lock().unwrap(),
                    );
                    let skpr_partition = Helpers::parse_partition_field(&record);
                    let skpr_time = Helpers::parse_time_field(&record);

                    let mut skpr_time_bucket = 0;

                    if skpr_time.is_some() {
                        skpr_time_bucket = BufferChunker::event_time_bucket(skpr_time.unwrap());
                    }

                    if Config::truth_value(faltten_events) {
                        record = Helpers::flatten(&record);
                    }

                    let output_file_name = BufferChunker::encode_chunk_name(
                        "ingest",
                        Some(&skpr_namespace),
                        Some(&skpr_partition),
                        Some(skpr_time_bucket),
                    );
                    let output_file = format!("{}/{}", output_dir.clone(), &output_file_name);

                    if output_files.get_mut(&output_file_name).is_none() {
                        let f = OpenOptions::new()
                            .create(true)
                            .write(true)
                            .append(true)
                            .open(output_file.clone())
                            .unwrap();

                        let new_file = OutputFile {
                            bytes: record_bytes,
                            seconds: SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs(),
                            file: f
                        };

                        output_files.insert(output_file_name.clone(), new_file);
                    }

                    let mut meta = metadata_clone.lock().unwrap();
                    if meta.get(&skpr_namespace).is_none() {
                        meta.insert(skpr_namespace.clone(), Metadata::new().unwrap());
                    }

                    let msg = fast_path_ingest(
                        &record,
                        &mut meta.get_mut(&skpr_namespace).unwrap().fields,
                        &mut updated_schema_clone.lock().unwrap(),
                    );

                    buf_str = msg.to_string() + "\n";

                    output_files
                        .get_mut(&output_file_name)
                        .unwrap()
                        .file
                        .write_all(buf_str.as_bytes())
                        .unwrap();

                    output_files
                        .get_mut(&output_file_name)
                        .unwrap()
                        .bytes += record_bytes;

                    output_files
                        .get_mut(&output_file_name)
                        .unwrap()
                        .seconds = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();

                    buf_str.clear();

                    offset_db_clone.set(&ingest_batch.offset_key, OffsetTypes::Line, i);

                    j += 1;
                }

                i += 1;
            }

            offset_db_clone.set(&ingest_batch.offset_key, OffsetTypes::Closed, 1);

            let mut counter_lock = metrcis_clone.lock().unwrap();
            counter_lock.ingeted_current += j;
            counter_lock.ingeted_total += j;
            counter_lock.msgs_total += i;
            counter_lock.bytes_current += bytes;
        }

        // Self::flush_buffers(true);
        Self::flush_buffers(false, output_files);

        // Retain only items that didn't qualify for flushing
        output_files.retain(|_filename, file| !Ingest::is_file_size_exceeded(file));

        if *updated_schema_clone.lock().unwrap() == "yes".to_string() {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    Config::set_config(
                        &metadata_clone.lock().unwrap(),
                        *updated_schema_clone.lock().unwrap() == "yes".to_string(),
                    )
                    .await;
                });

            *updated_schema_clone.lock().unwrap() = "no".to_string();
        }
    }

    fn is_file_size_exceeded(file: &OutputFile) -> bool {
        let buffer_size = Config::getenv("BUFFER_THRESHOLD_BYTES", "10485760"); // 10MB default

        if file.bytes > buffer_size.parse::<u64>().unwrap() {
            return true
        }

        false
    }

    fn is_file_time_exceeded(file: &OutputFile) -> bool {
        let ttl = Config::getenv("BUFFER_THRESHOLD_SECONDS", "300"); // 10MB default
        file.seconds > ttl.parse::<u64>().unwrap()
    }
}
