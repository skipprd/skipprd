pub mod ingest_buffer;

use chrono::{DateTime, Datelike, NaiveDateTime, TimeZone, Timelike, Utc};
use url::form_urlencoded;

use std::collections::HashMap;

use std::fs::File;

// use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};

// use std::{fs, str};


use tokio::io::{AsyncReadExt, AsyncWriteExt};

use std::path::{Path, PathBuf};
use std::string::ToString;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use std::time::{SystemTime};
use arrow::datatypes;
use arrow::error::ArrowError;
use dashmap::DashMap;
use glob::{glob_with, MatchOptions};



use parquet::data_type::AsBytes;
use parquet::file::reader::Length;
use crate::{BUFFER_FINALISE_RUNNING, METADATA};
use crate::converters::skippr_arrow::convert_skippr_to_arrow;
use crate::discover::Metadata;

use crate::helpers::configuration::Config;
use crate::helpers::Helpers;
use crate::helpers::timed_rwlock::TimedRwLock;
// use crate::ingest_work::OutputFile;

use crate::serdes::parquet::SerdeParquet;
use once_cell::sync::Lazy;
use crate::buffer::ingest_buffer::WalFile;

pub struct BufferChunker {}

// pub static BUFFER_INDEX: Lazy<Arc<TimedRwLock<DashMap<String, DashMap<String, OutputFile>>>>> = Lazy::new(|| {
//     Arc::new(TimedRwLock::new("buffer_index".to_string(), DashMap::new()))
// });

impl BufferChunker {
    // fn is_file_size_exceeded(file: &WalFile) -> bool {
    //     let buffer_size = Config::get_pipeline_buffer_threshold_bytes(); // 10MB default
    //     file.bytes > buffer_size as u64
    // }
    //
    // fn is_file_time_exceeded(file: &WalFile) -> bool {
    //     let ttl = Config::get_pipeline_buffer_threshold_seconds(); // 10MB default
    //     SystemTime::now()
    //         .duration_since(file.updated_at)
    //         .unwrap()
    //         .as_secs()
    //         > ttl as u64
    // }

    // fn is_rotated(file: &WalFile) -> bool {
    //     file.rotated.is_some()
    // }

    // unsafe function
    // unsafe fn get_unlimit() -> i32 {
    //     let open_file_limit = match unsafe { libc::sysconf(libc::_SC_OPEN_MAX) } {
    //         -1 => 1000,
    //         limit => limit as usize,
    //     };
    //     open_file_limit as i32
    // }

    // pub fn build_buffer_index() -> Result<(), Box<dyn std::error::Error>> {
    //
    //     println!("Building buffer indexs");
    //
    //     let data_dir = Config::get_data_dir(); // Assuming Config::get_data_dir() is defined
    //     let mut patterns = HashMap::new();
    //     // patterns.insert("ingest_buffer_part", format!("{}/ingest_buffer/*.part*", data_dir));
    //     // patterns.insert("deadletter_buffer_part", format!("{}/deadletter_buffer/*.part*", data_dir));
    //     patterns.insert("ingest_buffer_merged", format!("{}/ingest_buffer/*.merged", data_dir));
    //     patterns.insert("deadletter_buffer_merged", format!("{}/deadletter_buffer/*.merged", data_dir));
    //
    //     let options = MatchOptions {
    //         case_sensitive: false,
    //         require_literal_separator: false,
    //         require_literal_leading_dot: false,
    //     };
    //
    //     // get OS open file limit
    //     let open_file_limit = 4096; // @todo - unsafe { BufferChunker::get_unlimit() };
    //
    //     let mut i  = 0;
    //
    //     for pattern in patterns.iter() {
    //         {
    //             let index = BUFFER_INDEX.write();
    //
    //             let _file_dashmap = match index.get_mut(&pattern.0.to_string()) {
    //                 Some(file_dashmap) => file_dashmap,
    //                 None => {
    //                     let file_dashmap = DashMap::new();
    //                     index.insert(pattern.0.to_string(), file_dashmap);
    //                     index.get_mut(&pattern.0.to_string()).unwrap()
    //                 }
    //             };
    //         }
    //
    //
    //         for path in glob_with(pattern.1.as_str(), options).expect("Failed to read glob pattern") {
    //             match path {
    //                 Ok(path) => {
    //                     if let Ok(metadata) = std::fs::metadata(&path) {
    //
    //                         i+=1;
    //
    //                         let filename = format!("./{}", path.to_str().unwrap().to_string());
    //
    //                         let limit_readed = if i >= open_file_limit {
    //                             true
    //                         } else {
    //                             false
    //                         };
    //
    //                         let output_file = match limit_readed {
    //                             false => {
    //
    //                                 let file = std::fs::File::open(&filename)?;
    //
    //                                 OutputFile {
    //                                     bytes: metadata.len(),
    //                                     updated_at: metadata.modified().unwrap_or(SystemTime::now()),
    //                                     file: TimedRwLock::new("index_buf_file".to_string(), Some(file)),
    //                                     rotated: None,
    //                                     path: PathBuf::from(&filename),
    //                                 }
    //                             }
    //                             true => {
    //                                 let output_file = OutputFile {
    //                                     bytes: metadata.len(),
    //                                     updated_at: metadata.modified().unwrap_or(SystemTime::now()),
    //                                     file: TimedRwLock::new("index_buf_file".to_string(), None),
    //                                     rotated: None,
    //                                     path: PathBuf::from(&filename),
    //                                 };
    //
    //                                 // drop(file); // don't exhaust file descriptors
    //
    //                                 output_file
    //                             }
    //                         };
    //
    //                         let index = BUFFER_INDEX.write();
    //
    //                         let file_dashmap= match index.get_mut(&pattern.0.to_string()) {
    //                             Some(file_dashmap) => file_dashmap,
    //                             None => {
    //                                 let file_dashmap = DashMap::new();
    //                                 index.insert(pattern.0.to_string(), file_dashmap);
    //                                 index.get_mut(&pattern.0.to_string()).unwrap()
    //                             }
    //                         };
    //
    //                         // println!("Indexing file {}", filename);
    //
    //                         // let file_dashmap = DashMap::new();
    //                         file_dashmap.insert(filename, output_file);
    //
    //                         // index.insert(pattern.0.to_string(), file_dashmap);
    //
    //                     }
    //                 },
    //                 Err(e) => println!("Glob error: {}", e),
    //             }
    //         }
    //
    //         match BUFFER_INDEX.read().get( &pattern.0.to_string()) {
    //             Some(index) => {
    //                 println!("Indexed {} files in {}", index.len(), pattern.0);
    //             },
    //             None => {
    //                 println!("Indexed 0 files in {}", pattern.0);
    //             }
    //         }
    //         // println!("Indexed {} files in {}", BUFFER_INDEX.read().get( &pattern.0.to_string()).unwrap().len(), pattern.0);
    //     }
    //
    //     Ok(())
    // }

    // pub fn create_and_insert_new_file(new_filename: String) {
    //
    //     let index_guard = BUFFER_INDEX.write();
    //     let index = index_guard.get_mut("ingest_buffer_merged").unwrap();
    //
    //     // println!("Creating merge file {}", new_filename);
    //
    //     let file = match std::fs::OpenOptions::new()
    //         .create(true)
    //         .append(true)
    //         .open(&new_filename) {
    //         Ok(file) => file,
    //         Err(err) => {
    //            panic!("Error creating buffer file index: {}, File: {}", err, new_filename);
    //         }
    //     };
    //
    //     let output_file = OutputFile {
    //         bytes: match file.metadata() {
    //             Ok(metadata) => metadata.len(),
    //             Err(_err) => 0,
    //         },
    //         updated_at: match file.metadata() {
    //             Ok(metadata) => match metadata.modified() {
    //                 Ok(time) => time,
    //                 Err(_err) => SystemTime::now(),
    //             },
    //             Err(_err) => SystemTime::now(),
    //         },
    //         file: TimedRwLock::new("index_buf_file".to_string(), Some(file)),
    //         rotated: None,
    //         path: PathBuf::from(&new_filename),
    //     };
    //
    //     // {
    //
    //
    //         index.insert(new_filename.clone(), output_file);
    //     // }
    //     // index.get_mut(&new_filename).unwrap().value_mut()
    //
    // }


    // pub fn rotate_buffers(force: bool) {
    //     if BUFFER_FINALISE_RUNNING.read().load(Ordering::SeqCst) {
    //         return;
    //     } else {
    //         BUFFER_FINALISE_RUNNING.write().store(true, Ordering::SeqCst);
    //     }
    //
    //     let data_dir = Config::get_data_dir();
    //
    //     let options = MatchOptions {
    //         case_sensitive: false,
    //         require_literal_separator: false,
    //         require_literal_leading_dot: false,
    //     };
    //
    //     // let file_paths = {
    //     //     let index_guard = BUFFER_INDEX.read();
    //     //     let index = index_guard.get("ingest_buffer_part").unwrap();
    //     //     index.iter().map(|entry| entry.key().clone()).collect::<Vec<_>>()
    //     // };
    //
    //
    //     // for file_path in file_paths {
    //     //
    //     //     let skpr_namespace =
    //     //         BufferChunker::decode_file_namespace(&file_path);
    //     //     let skpr_partition =
    //     //         BufferChunker::decode_file_partition(&file_path);
    //     //     let source_time = BufferChunker::decode_file_time(&file_path);
    //     //     let mut skpr_time = None;
    //     //     if source_time >= 0 {
    //     //         skpr_time = Some(source_time);
    //     //     }
    //     //
    //     //     let finalised_file_name = BufferChunker::encode_chunk_name(
    //     //         "output",
    //     //         Some(&skpr_namespace),
    //     //         Some(&skpr_partition),
    //     //         skpr_time,
    //     //     );
    //     //
    //     //     // get directory of the file
    //     //     let dir = Path::new(&file_path).parent().unwrap().to_str().unwrap();
    //     //
    //     //     let new_filename = format!(
    //     //         "{}/{}.merged",
    //     //         dir,
    //     //         finalised_file_name
    //     //     );
    //     //
    //     //     let old_path = format!("{}", file_path);
    //     //
    //     //     let new_file_exists = {
    //     //         let mut index_guard = BUFFER_INDEX.write();
    //     //         let index = index_guard.get_mut("ingest_buffer_merged").unwrap();
    //     //         index.contains_key(&new_filename)
    //     //     };
    //     //
    //     //     if !new_file_exists {
    //     //         // println!("Creating merge file {}", new_filename);
    //     //
    //     //         BufferChunker::create_and_insert_new_file(new_filename.clone()).await;
    //     //         continue;
    //     //     }
    //     //
    //     //     // println!("Merging file {} to {}", old_path, new_filename);
    //     //
    //     //     let mut old_file = match fs::File::open(&old_path).await {
    //     //         Ok(file) => file,
    //     //         Err(err) => {
    //     //             println!("Error: {}, File: {}", err, old_path);
    //     //             continue;
    //     //         }
    //     //     };
    //     //
    //     //     let mut buffer = vec![0; 4096]; // Chunk size can be adjusted
    //     //
    //     //     let mut index_guard = BUFFER_INDEX.write();
    //     //     let index = index_guard.get_mut("ingest_buffer_merged").unwrap();
    //     //     let mut new_file_ref = index.get_mut(&new_filename).unwrap();
    //     //
    //     //     let mut new_file = new_file_ref.value_mut();
    //     //
    //     //     // check file descriptor is open
    //     //     if new_file.file.is_none() || new_file.file.as_mut().unwrap().metadata().await.is_err() {
    //     //         println!("File descriptor changed, re-creating {}", new_filename);
    //     //
    //     //         let file = fs::OpenOptions::new()
    //     //             .create(true)
    //     //             .append(true)
    //     //             .open(&new_filename)
    //     //             .await.unwrap();
    //     //
    //     //         new_file.file = Some(file);
    //     //     }
    //     //
    //     //     // println!("writing to file {}", new_filename);
    //     //
    //     //     let mut total_bytes = 0;
    //     //
    //     //     loop {
    //     //         let bytes_read = old_file.read(&mut buffer).await.expect("Failed to read file");
    //     //         if bytes_read == 0 {
    //     //             break;
    //     //         }
    //     //
    //     //         new_file.file.as_mut().unwrap().write(&buffer[..bytes_read]).await.expect(format!("Failed to write file: {}", new_filename).as_str());
    //     //         new_file.bytes += bytes_read as u64;
    //     //
    //     //         total_bytes += bytes_read;
    //     //
    //     //         // modus 1000000
    //     //         if total_bytes % 1000000 == 0 {
    //     //             println!("written {} bytes", total_bytes);
    //     //         }
    //     //     }
    //     //
    //     //     // println!("flushing file {}", new_filename);
    //     //
    //     //     match new_file.file.as_mut().unwrap().flush().await {
    //     //         Ok(_) => {}
    //     //         Err(err) => {
    //     //             println!("Error in file {}: {}", new_filename, err)
    //     //         }
    //     //     }
    //     //     match new_file.file.as_mut().unwrap().sync_all().await {
    //     //         Ok(_) => {}
    //     //         Err(err) => {
    //     //             println!("Error in file {}: {}", new_filename, err)
    //     //         }
    //     //     }
    //     //
    //     //     new_file.updated_at = SystemTime::now();
    //     //
    //     //     println!("flushed file {}", new_filename);
    //     //
    //     //     // tombstone file, can't delete it as OS may not delete immediately and we may write to it again
    //     //     let tombstone_file_name = old_path.rsplitn(2, "/").next().unwrap();
    //     //     let tombstone_file_path = format!("{}/done/{}", file_path, tombstone_file_name);
    //     //
    //     //     match fs::rename(old_path.as_str(), &tombstone_file_path).await {
    //     //         Ok(_) => {}
    //     //         Err(_) => {}
    //     //     };
    //     //
    //     //     println!("Tomstoned file {}", &tombstone_file_path);
    //     //
    //     //     // update index
    //     //
    //     //     // if called when holding any sort of reference into the map.
    //     //     // while index_guard.get_mut("ingest_buffer_merged").is_none() {
    //     //     //     println!("Waiting for index to be released");
    //     //     //     sleep(std::time::Duration::from_millis(100));
    //     //     // }
    //     //     // let old_index_guard = BUFFER_INDEX.write();
    //     //     // let old_index = old_index_guard.get("ingest_buffer_part").unwrap();
    //     //     // old_index.remove(&old_path);
    //     //     // //
    //     //     // println!("Updated index for file {}", old_path);
    //     //
    //     //
    //     //     let path = PathBuf::from(&new_filename);
    //     // }
    //
    //     // print number of files in ingest_buffer_merged index
    //     // let index_guard = BUFFER_INDEX.read();
    //     // let index = match index_guard.get("ingest_buffer_merged") {
    //     //     Some(index) => index,
    //     //     None => {
    //     //         println!("No files in ingest_buffer_merged");
    //     //         return;
    //     //     }
    //     // };
    //
    //     // println!("\nIndexed {} files in ingest_buffer_merged\n", index.len());
    //
    //     if force {
    //         let options = MatchOptions {
    //             case_sensitive: false,
    //             require_literal_separator: false,
    //             require_literal_leading_dot: false,
    //         };
    //
    //         for path in glob_with(&format!("{}/ingest_buffer/*.merged", data_dir), options)
    //             .expect("Failed to read glob pattern")
    //             .filter_map(Result::ok)
    //             .collect::<Vec<_>>()
    //         {
    //
    //             let new_filename = path.to_str().unwrap().to_string();
    //
    //             let file = match std::fs::OpenOptions::new()
    //                 .create(true)
    //                 .append(true)
    //                 .open(&new_filename) {
    //                 Ok(file) => file,
    //                 Err(err) => {
    //                     println!("Error while opening buffer file: {}, File: {}", err, new_filename);
    //                     continue;
    //                 }
    //             };
    //
    //             let output_file = OutputFile {
    //                 bytes: match file.metadata() {
    //                     Ok(metadata) => metadata.len(),
    //                     Err(_err) => 0,
    //                 },
    //                 updated_at: match file.metadata() {
    //                     Ok(metadata) => match metadata.modified() {
    //                         Ok(time) => time,
    //                         Err(_err) => SystemTime::now(),
    //                     },
    //                     Err(_err) => SystemTime::now(),
    //                 },
    //                 file: TimedRwLock::new("index_buf_file".to_string(), Some(file)),
    //                 rotated: None,
    //                 path: PathBuf::from(&new_filename),
    //             };
    //
    //             BufferChunker::finalise_buffers(force, &output_file, &path.to_str().unwrap().to_string());
    //         }
    //     }
    //
    //     // Delete all tombstone files in done dir
    //     let paths = glob_with(&format!("{}/ingest_buffer/done/*", data_dir), options)
    //         .expect("Failed to read glob pattern")
    //         .filter_map(Result::ok)
    //         .collect::<Vec<_>>();
    //
    //     for path in paths {
    //         match std::fs::remove_file(&path) {
    //             Ok(_t) => {}
    //             Err(err) => println!("{:?}", err),
    //         }
    //     }
    //
    //     BUFFER_FINALISE_RUNNING
    //         .write()
    //         .store(false, Ordering::SeqCst);
    // }

    // pub fn finalise_buffers(force: bool, output_file: &OutputFile, filename: &String) -> bool {
    //
    //     let flatten = Config::get_transform_flatten_events();
    //
    //     let data_dir = Config::get_data_dir();
    //     let output_dir = &format!("{}/ingest_buffer", data_dir);
    //     let finalised_dir = &format!("{}/output_buffer", data_dir);
    //
    //     let _options = MatchOptions {
    //         case_sensitive: false,
    //         require_literal_separator: false,
    //         require_literal_leading_dot: false,
    //     };
    //
    //     if force
    //         || BufferChunker::is_file_size_exceeded(&output_file)
    //         || BufferChunker::is_file_time_exceeded(&output_file)
    //     {
    //
    //         // check filename exists on disk, very often not syned to disk yet
    //         // only check fs when necessary, i.e. when file is due to be rotated
    //         if !Path::new(filename).exists() {
    //             return false;
    //         }
    //
    //         // println!("Finalising output file {}", filename);
    //
    //         // aquire lock on file to prevent writing while we finalise
    //         // let lock = output_file.file.write();
    //
    //         if output_file.bytes == 0 {
    //             println!("Skipping empty file {}", filename);
    //             // continue;
    //             return false;
    //         }
    //
    //         // Always regenerate arrow schema incase updated skippr metadata, e.g. discovered a new field
    //         let mut arrow_schema: Result<datatypes::Schema, ArrowError> = Ok(datatypes::Schema::empty());
    //         let mut schema_ref = Arc::new(datatypes::Schema::empty());
    //
    //         let skpr_namespace =
    //             BufferChunker::decode_file_namespace(filename.as_str());
    //
    //         let metadata = METADATA.read();
    //
    //         if metadata.get(&skpr_namespace).is_some() {
    //             let mut output_metadata: HashMap<String, Metadata> = HashMap::new();
    //             if flatten {
    //                 let mut meta: HashMap<String, Metadata> = HashMap::new();
    //
    //                 crate::flatten_metadata(metadata.get(&skpr_namespace).unwrap(), &mut meta);
    //
    //                 let mut flat: Metadata = Metadata::new().unwrap();
    //                 flat.fields = Box::new(meta);
    //                 output_metadata.insert(skpr_namespace.clone(), flat);
    //             } else {
    //                 output_metadata = metadata.clone();
    //             }
    //
    //             let skpr_partition =
    //                 BufferChunker::decode_file_partition(filename.as_str());
    //             let shard = BufferChunker::decode_file_shard(filename.as_str());
    //             let source_time = BufferChunker::decode_file_time(filename.as_str());
    //             let mut skpr_time = None;
    //             if source_time >= 0 {
    //                 skpr_time = Some(source_time);
    //             }
    //
    //             arrow_schema = convert_skippr_to_arrow(
    //                 output_metadata.get(&skpr_namespace).unwrap().fields.clone(),
    //             );
    //
    //             schema_ref = Arc::new(arrow_schema.unwrap());
    //
    //             let path = PathBuf::from(&filename);
    //
    //             let tmp_file_path = SerdeParquet::serialize(path, schema_ref);
    //
    //             let finalised_file_name = BufferChunker::encode_chunk_name(
    //                 "output",
    //                 Some(&skpr_namespace),
    //                 Some(&skpr_partition),
    //                 skpr_time,
    //                 Some(&shard),
    //             );
    //
    //             let finalised_file_path = &format!(
    //                 "{}/{}&part={}.parquet",
    //                 finalised_dir,
    //                 finalised_file_name,
    //                 Helpers::random_str(32).as_str()
    //             );
    //
    //             match std::fs::rename(tmp_file_path, finalised_file_path) {
    //                 Ok(_) => {}
    //                 Err(_) => {}
    //             };
    //
    //             // drop(lock);
    //
    //             // close file pointer and remove from index
    //             // let mut index_guard = BUFFER_INDEX.write();
    //             // let index = index_guard.get_mut("ingest_buffer_merged").unwrap();
    //             // index.remove(filename);
    //
    //
    //
    //             // tombstone file, can't delete it as OS may not delete immediately and we may write to it again
    //             let file_name_without_dir = filename.rsplitn(2, "/").next().unwrap();
    //             let tombstone_file_name = file_name_without_dir.replace(".merged", ".tombstone");
    //             let tombstone_file_path = format!("{}/done/{}", output_dir, tombstone_file_name);
    //
    //             match std::fs::rename(filename.as_str(), &tombstone_file_path) {
    //                 Ok(_) => {}
    //                 Err(_) => {}
    //             };
    //
    //             // println!("Tomstoned file {}", &tombstone_file_path);
    //
    //             // println!("Finalised output file {}", finalised_file_path);
    //
    //
    //
    //             return true;
    //         }
    //
    //     }
    //
    //     false
    // }

    pub fn check_flush_limit(_chunk_name: &str, _chunk: &HashMap<String, usize>) -> bool {
        let mut result = false;

        if Helpers::mem_limit_reached() {
            println!("Rotating buffer as memory limit has low headroom");
            // println!(format!("Rotating buffer as memory limit has low headroom at {}", BytesToHuman::to_human(memory_get_usage())));
            result = true;
        }

        // if chunk.get("size").unwrap() > Config.flush_mem_buffer_bytes {
        // SkipprLogger::debug(&format!("Rotating memory buffer with size {}", BytesToHuman::to_human(chunk.get("size").unwrap(), true)));
        // result = true;
        // }

        // if (SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).expect("Time went backwards")
        //     .as_secs() - chunk.get("time").unwrap().as_i64()) > Config::FLUSH_MEM_BUFFER_SECONDS {
        // SkipprLogger::debug(&format!("Rotating memory buffer with ttl {} seconds", time::now().to_timespec().sec - chunk.get("time").unwrap()));
        //    result = true;
        // }

        // if chunk.get("count").unwrap() >= Config::FLUSH_MEM_BUFFER_RECORDS {
        // SkipprLogger::debug(&format!("Rotating memory buffer of {} records", chunk.get("count").unwrap()));
        // result = true;
        // }

        // if result {
        // let size = BytesToHuman::to_human(chunk.get("size").unwrap(), true);
        // let time = time::now().to_timespec().sec - chunk.get("time").unwrap();
        // let count = chunk.get("count").unwrap();
        //
        // SkipprLogger::info(&format!("Rotating memory buffer {} of {}, {} records and age of {} seconds to disk", chunk_name, size, count, time));
        // }

        result
    }

    pub fn event_time_bucket(event_time: i64) -> i64 {
        let datetime = Utc.timestamp_opt(event_time, 0).unwrap();

        // let mut bucket_rounded_timestamp: DateTime<Utc> = Utc.ymd(datetime.year(), 1, 1).and_hms(0, 0, 0);
        let mut bucket_rounded_timestamp = 0;

        if let config_duration = Config::get_transform_batch_time_unit() {
            bucket_rounded_timestamp = match config_duration.as_str() {
                "year" => {
                    let year = datetime.year();
                    Utc.ymd(year, 1, 1).and_hms(0, 0, 0).timestamp()
                }
                "month" => {
                    let year = datetime.year();
                    let month = datetime.month();
                    Utc.ymd(year, month, 1).and_hms(0, 0, 0).timestamp()
                }
                "day" => {
                    let year = datetime.year();
                    let month = datetime.month();
                    let day = datetime.day();
                    Utc.ymd(year, month, day).and_hms(0, 0, 0).timestamp()
                }
                "hour" => {
                    let year = datetime.year();
                    let month = datetime.month();
                    let day = datetime.day();
                    let hour = datetime.hour();
                    Utc.ymd(year, month, day).and_hms(hour, 0, 0).timestamp()
                }
                "minute" => {
                    let year = datetime.year();
                    let month = datetime.month();
                    let day = datetime.day();
                    let hour = datetime.hour();
                    let minute = datetime.minute();
                    Utc.ymd(year, month, day)
                        .and_hms(hour, minute, 0)
                        .timestamp()
                }
                _ => 0,
            };
        }

        if bucket_rounded_timestamp > 0 {
            // event_time - (event_time % bucket_seconds)
            event_time - ((event_time).rem_euclid(bucket_rounded_timestamp))
            // event_time.div_euclid(bucket_seconds)
            // event_time.div_rem(bucket_seconds)
        } else {
            0
        }
    }

    pub fn decode_chunk_string_from_filename(filename: &str) -> String {

        // Parse the query string into key-value pairs
        let pairs = url::form_urlencoded::parse(filename.as_bytes());

        // match pairs into:
        // let chunks = vec![
        //     ("buffer".to_string(), buffer_name.to_string()),
        //     ("namespace".to_string(), namespace.unwrap_or("").to_string()),
        //     ("partition".to_string(), partition.unwrap_or("").to_string()),
        //     ("time".to_string(), time_string),
        //     ("shard".to_string(), shard_string),
        // ];
        let mut chunks = HashMap::new();

        pairs.into_iter().for_each(|(key, value)| {
            chunks.insert(key, value);
        });

        let chunk_name = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(chunks)
            .finish();

        chunk_name
    }

    pub fn encode_chunk_name(
        buffer_name: &str,
        namespace: Option<&str>,
        partition: Option<&str>,
        time_bucket: Option<i64>,
        shard: Option<&str>,
    ) -> String {
        let time_string = match time_bucket {
            Some(time_bucket) => format!("{}", time_bucket),
            None => "".to_string(),
        };

        let shard_string = match shard {
            Some(shard) => format!("{}", shard),
            None => "".to_string(),
        };

        let chunks = vec![
            ("buffer".to_string(), buffer_name.to_string()),
            ("namespace".to_string(), namespace.unwrap_or("").to_string()),
            ("partition".to_string(), partition.unwrap_or("").to_string()),
            ("time".to_string(), time_string),
            ("shard".to_string(), shard_string),
        ];
        // if time_bucket.is_some() {
        //     chunks.push(("time".to_string(), time_string));
        // }

        let chunk_name = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(chunks)
            .finish();

        chunk_name
    }

    // pub fn get_chunk_name(buffer_name: &str, filename: &str) -> Vec<String> {
    //     let start_pos = filename.find(buffer_name).unwrap() + buffer_name.len();
    //     let end_pos = filename.rfind("&output_buffer").unwrap();
    //     let encoded_name = &filename[start_pos..end_pos - 43];
    //     let encoded_name = encoded_name.trim_matches('-');
    //
    //     encoded_name.split('-').map(|s| s.to_string()).collect()
    // }

    fn get_file_time(filename: &str) -> i64 {
        let mut time = -1;


        // strip './' from filename if present
        let filename = filename.trim_start_matches("./");
        // strip any file extension
        let filename = filename.split('.').next().unwrap();



        // Parse the query string into key-value pairs
        let pairs = url::form_urlencoded::parse(filename.as_bytes());


        // Print each key-value pair
        for (key, value) in pairs {


            if key == "time" {
                if value.is_empty() {
                    continue;
                }
                // if invalid (not an integer), set to 1970-01-01T00:00:00+00:00 as we probably expect a date. e.g. Hive parittions valus must match
                time = value.parse::<i64>().unwrap_or(0);
            }
        }

        time
    }

    fn get_file_part(filename: &str, part_name: &str) -> String {
        let mut part = "".to_string();

        // Parse the query string into key-value pairs
        let pairs = url::form_urlencoded::parse(filename.as_bytes());

        // Print each key-value pair
        for (key, value) in pairs {
            if key == part_name {
                part = value.to_string()
            }
        }

        part
    }

    pub fn decode_file_time_to_datetime_string(filename: &str) -> String {
        let time = BufferChunker::get_file_time(filename);

        // println!("decoding buffer time {} file {}", date_string, filename);

        if time >= 0 {
            DateTime::<Utc>::from_utc(NaiveDateTime::from_timestamp(time, 0), Utc).to_rfc3339()
        } else {
            "".to_string()
        }
    }

    pub fn decode_file_time(filename: &str) -> i64 {
        BufferChunker::get_file_time(filename)
    }

    pub fn decode_file_partition(filename: &str) -> String {
        // let mut array = form_urlencoded::parse(filename.as_bytes());
        // let partition = array.remove("partition").unwrap_or_default();

        let mut path_parts = "".to_string();

        // println!("decoding buffer partition {} file {}", partition, filename);

        let partition = BufferChunker::get_file_part(filename, "partition");

        if !partition.is_empty() {
            // let parts = partition.split('-');
            let parts = partition.split("%2D"); // hyphen
                                                // .next().unwrap();
                                                // let parts = partition.split("%2D");
            let collection: Vec<&str> = parts.collect();
            // let collection: Vec<&str> = parts.map(| val |val.rsplitn(1, "%3D").next().unwrap()).collect();

            path_parts = collection.join("/");
        }

        path_parts
    }

    pub fn decode_file_namespace(filename: &str) -> String {
        // let mut array = form_urlencoded::parse(filename.as_bytes());
        // let namespace = array.get("namespace").map(|s| s.to_string()).unwrap_or_default();

        // println!("decoding buffer namespace {} from file {}", namespace, filename);

        BufferChunker::get_file_part(filename, "namespace")
    }

    pub fn decode_file_shard(filename: &str) -> String {
        BufferChunker::get_file_part(filename, "shard")
    }

    pub fn next_file(buffer_name: &str) -> Option<String> {
        let data_dir = Config::get_data_dir();
        let pattern = format!("{}/{}_buffer/buffer={}*", data_dir, buffer_name, buffer_name);

        let filenames = match glob::glob(&pattern) {
            Ok(filenames) => filenames
                .filter_map(Result::ok)
                .collect::<Vec<_>>(),
            Err(e) => {
                println!("Error globbing for next buffer file, Error: {}", e);
                return None;
            }
        };

        // filenames.sort_by(|a, b| fs::metadata(a).unwrap().modified().cmp(&fs::metadata(b).unwrap().modified()));

        for filename in filenames {

            if filename.to_str().unwrap().contains(".temp") {
                continue;
            }

            match File::open(&filename) {
                Err(ref e) if e.kind() == ErrorKind::NotFound => {
                    // File was removed by a competing thread
                    continue;
                }
                Err(e) => {
                    // Other errors
                    println!("Error opening file {}: {}", filename.to_str().unwrap(), e);
                    continue;
                }
                Ok(_file) => {
                    // check file is not empty
                    // let metadata = match fs::metadata(&filename) {
                    //     Err(e) => {
                    //         // println!("Error reading file {}: {}", filename.to_str().unwrap(), e);
                    //         continue;
                    //     }
                    //     Ok(metadata) => metadata,
                    // };
                    // if metadata.len() == 0 {
                    //     continue;
                    // }

                    // if self.lock(&file, false) {
                    return Some(filename.to_str().unwrap().to_string());
                    // }
                }
            }
        }

        None
    }
}

#[cfg(test)]
mod decode_chunk_time_tests {
    use super::*;

    #[test]
    fn test_get_file_chunk_time_with_valid_input() {
        let filename = "buffer=test_buffer&namespace=&partition=&time=1645296045";
        assert_eq!(
            BufferChunker::decode_file_time_to_datetime_string(filename),
            "2022-02-19T18:40:45+00:00"
        );
    }

    #[test]
    fn test_get_file_chunk_time_with_missing_time_query_param() {
        let filename = "buffer=test_buffer&namespace=&partition=";
        assert_eq!(
            BufferChunker::decode_file_time_to_datetime_string(filename),
            ""
        );
    }

    #[test]
    fn test_get_file_chunk_time_with_zero_time_query_param() {
        let filename = "buffer=test_buffer&namespace=&partition=&time=0";
        assert_eq!(
            BufferChunker::decode_file_time_to_datetime_string(filename),
            "1970-01-01T00:00:00+00:00"
        );
    }

    #[test]
    fn test_get_file_chunk_time_with_invalid_time_query_param() {
        let filename = "buffer=test_buffer&namespace=&partition=&time=invalid";
        assert_eq!(
            BufferChunker::decode_file_time_to_datetime_string(filename),
            "1970-01-01T00:00:00+00:00"
        );
    }

    #[test]
    fn test_get_file_chunk_time_with_blank_time_query_param() {
        let filename = "buffer=test_buffer&namespace=&partition=&time=";
        assert_eq!(
            BufferChunker::decode_file_time_to_datetime_string(filename),
            ""
        );
    }

    #[test]
    fn test_get_file_chunk_time_with_valid_and_shard() {
        let filename = "buffer=test_buffer&namespace=&partition=&time=1645296045&shard=1";
        assert_eq!(BufferChunker::decode_file_time_to_datetime_string(filename), "2022-02-19T18:40:45+00:00");
    }

    #[test]
    fn test_get_file_chunk_time_with_valid_and_shard_and_extentin() {
        let filename = "buffer=output&namespace=bike_hire&partition=&time=1645296045&shard=1.merged";
        assert_eq!(BufferChunker::decode_file_time_to_datetime_string(filename), "2022-02-19T18:40:45+00:00");
    }
}

#[cfg(test)]
mod get_file_chunk_time_tests {
    use super::*;

    #[test]
    fn test_get_file_chunk_time_with_valid_input() {
        let filename = "buffer=test_buffer&namespace=&partition=&time=1645296045";
        assert_eq!(BufferChunker::get_file_time(filename), 1645296045);
    }

    #[test]
    fn test_get_file_chunk_time_with_missing_time_query_param() {
        let filename = "buffer=test_buffer&namespace=&partition=";
        assert_eq!(BufferChunker::get_file_time(filename), -1);
    }

    #[test]
    fn test_get_file_chunk_time_with_invalid_time_query_param() {
        let filename = "buffer=test_buffer&namespace=&partition=&time=invalid";
        assert_eq!(BufferChunker::get_file_time(filename), 0);
    }

    #[test]
    fn test_get_file_chunk_time_with_valid_and_extention() {
        let filename = "buffer=test_buffer&namespace=&partition=&time=1645296045.part";
        assert_eq!(BufferChunker::get_file_time(filename), 1645296045);
    }

    #[test]
    fn test_get_file_chunk_time_with_valid_and_shard() {
        let filename = "buffer=test_buffer&namespace=&partition=&time=1645296045&shard=1";
        assert_eq!(BufferChunker::get_file_time(filename), 1645296045);
    }

    #[test]
    fn test_get_file_chunk_time_with_valid_and_shard_and_extentin() {
        let filename = "buffer=output&namespace=bike_hire&partition=&time=1645296045&shard=1.merged";
        assert_eq!(BufferChunker::get_file_time(filename), 1645296045);
    }
}

#[cfg(test)]
mod event_time_bucket_tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial]
    fn test_event_time_bucket_year() {
        Config::setenv("TRANSFORM_BATCH_TIME_UNIT", "year");
        assert_eq!(BufferChunker::event_time_bucket(1645296045), 1640995200);
    }

    #[test]
    #[serial]
    fn test_event_time_bucket_month() {
        Config::setenv("TRANSFORM_BATCH_TIME_UNIT", "month");
        assert_eq!(BufferChunker::event_time_bucket(1645296045), 1643673600);
    }

    #[test]
    #[serial]
    fn test_event_time_bucket_day() {
        Config::setenv("TRANSFORM_BATCH_TIME_UNIT", "day");
        assert_eq!(BufferChunker::event_time_bucket(1645296045), 1645228800);
    }

    #[test]
    #[serial]
    fn test_event_time_bucket_hour() {
        Config::setenv("TRANSFORM_BATCH_TIME_UNIT", "hour");
        assert_eq!(BufferChunker::event_time_bucket(1645296045), 1645293600);
    }

    #[test]
    #[serial]
    fn test_event_time_bucket_minute() {
        Config::setenv("TRANSFORM_BATCH_TIME_UNIT", "minute");
        assert_eq!(BufferChunker::event_time_bucket(1645296045), 1645296000);
    }

    #[test]
    #[serial]
    fn test_event_time_bucket_none() {
        Config::setenv("TRANSFORM_BATCH_TIME_UNIT", "");
        assert_eq!(BufferChunker::event_time_bucket(1645296045), 0);
    }
}

// #[cfg(test)]
// mod get_chunk_name_tests {
//     use crate::buffer::BufferChunker;
//
//     #[test]
//     fn test_get_chunk_name() {
//         let buffer_name = "buffer_1";
//         let filename = "buffer_1-chunk-1-abc123-def456&finalised";
//
//         let result = BufferChunker::get_chunk_name(buffer_name, filename);
//
//         assert_eq!(result, vec!["chunk".to_string(), "1".to_string(), "abc123".to_string(), "def456".to_string()]);
//     }
//
//     #[test]
//     fn test_get_chunk_name_with_spaces() {
//         let buffer_name = "buffer 2";
//         let filename = "buffer 2-chunk-5-ghi789-jkl012&finalised";
//
//         let result = BufferChunker::get_chunk_name(buffer_name, filename);
//
//         assert_eq!(result, vec!["chunk".to_string(), "5".to_string(), "ghi789".to_string(), "jkl012".to_string()]);
//     }
//
//     #[test]
//     #[should_panic(expected = "called `Option::unwrap()` on a `None` value")]
//     fn test_get_chunk_name_with_missing_buffer_name() {
//         let buffer_name = "buffer_3";
//         let filename = "buffer_1-chunk-2-mno345-pqr678&finalised";
//
//         BufferChunker::get_chunk_name(buffer_name, filename);
//     }
//
//     #[test]
//     #[should_panic(expected = "called `Option::unwrap()` on a `None` value")]
//     fn test_get_chunk_name_with_missing_finalised() {
//         let buffer_name = "buffer_4";
//         let filename = "buffer_4-chunk-3-stu901-vwx234";
//
//         BufferChunker::get_chunk_name(buffer_name, filename);
//     }
//
// }

#[cfg(test)]
mod encode_chunk_name_tests {
    
    use crate::buffer::BufferChunker;

    #[test]
    fn test_encode_chunk_name_no_options() {
        let buffer_name = "test_buffer";
        let expected_chunk_name = "buffer=test_buffer&namespace=&partition=&time=";
        let actual_chunk_name = BufferChunker::encode_chunk_name(buffer_name, None, None, None, None);
        assert_eq!(expected_chunk_name, actual_chunk_name);
    }

    #[test]
    fn test_encode_chunk_name_with_namespace() {
        let buffer_name = "test_buffer";
        let namespace = Some("test_namespace");
        let expected_chunk_name = "buffer=test_buffer&namespace=test_namespace&partition=&time=";
        let actual_chunk_name =
            BufferChunker::encode_chunk_name(buffer_name, namespace, None, None, None);
        assert_eq!(expected_chunk_name, actual_chunk_name);
    }

    #[test]
    fn test_encode_chunk_name_with_partition() {
        let buffer_name = "test_buffer";
        let partition = Some("test_partition");
        let expected_chunk_name = "buffer=test_buffer&namespace=&partition=test_partition&time=";
        let actual_chunk_name =
            BufferChunker::encode_chunk_name(buffer_name, None, partition, None, None);
        assert_eq!(expected_chunk_name, actual_chunk_name);
    }

    #[test]
    fn test_encode_chunk_name_with_time_bucket() {
        let buffer_name = "test_buffer";
        let time_bucket = Some(123);
        let expected_chunk_name = "buffer=test_buffer&namespace=&partition=&time=123";
        let actual_chunk_name =
            BufferChunker::encode_chunk_name(buffer_name, None, None, time_bucket, None);
        assert_eq!(expected_chunk_name, actual_chunk_name);
    }

    #[test]
    fn test_encode_chunk_name_with_all_options() {
        let buffer_name = "test_buffer";
        let namespace = Some("test_namespace");
        let partition = Some("test_partition");
        let time_bucket = Some(456);
        let expected_chunk_name =
            "buffer=test_buffer&namespace=test_namespace&partition=test_partition&time=456";
        let actual_chunk_name =
            BufferChunker::encode_chunk_name(buffer_name, namespace, partition, time_bucket, None);
        assert_eq!(expected_chunk_name, actual_chunk_name);
    }
}
