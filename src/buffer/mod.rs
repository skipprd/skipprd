use chrono::{DateTime, Datelike, NaiveDateTime, TimeZone, Timelike, Utc};
use url::form_urlencoded;

use std::collections::HashMap;

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};

use std::{fs, str};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::sleep;
use std::time::{SystemTime, UNIX_EPOCH};
use arrow::datatypes;
use arrow::error::ArrowError;
use glob::{glob_with, MatchOptions};
use lru::LruCache;

use parquet::data_type::AsBytes;
use parquet::file::reader::Length;
use crate::{BUFFER_FINALISE_RUNNING, METADATA, OUTPUT_GRACEFUL_SHUTDOWN_COMPLETE, RUNNING};
use crate::converters::skippr_arrow::convert_skippr_to_arrow;
use crate::discover::Metadata;

use crate::helpers::configuration::Config;
use crate::helpers::Helpers;
use crate::ingest_work::OutputFile;
use crate::metrics::MetricsStatus::Running;
use crate::serdes::parquet::SerdeParquet;

pub struct BufferChunker {}

impl BufferChunker {
    fn is_file_size_exceeded(file: &OutputFile) -> bool {
        let buffer_size = Config::get_pipeline_buffer_threshold_bytes(); // 10MB default
        file.bytes > buffer_size as u64
    }

    fn is_file_time_exceeded(file: &OutputFile) -> bool {
        let ttl = Config::get_pipeline_buffer_threshold_seconds(); // 10MB default
        SystemTime::now()
            .duration_since(file.updated_at)
            .unwrap()
            .as_secs()
            > ttl as u64
    }

    fn is_rotated(file: &OutputFile) -> bool {
        file.rotated.is_some()
    }

    pub fn rotate_buffers(force: bool) {

        if BUFFER_FINALISE_RUNNING.read().load(Ordering::SeqCst) {
            return;
        } else {
            BUFFER_FINALISE_RUNNING.write().store(true, Ordering::SeqCst);
        }

        let data_dir = Config::get_data_dir();

        let options = MatchOptions {
            case_sensitive: false,
            require_literal_separator: false,
            require_literal_leading_dot: false,
        };

        let mut file_pointers: HashMap<String, OutputFile> = HashMap::new();

        let mut files_to_finalize: HashMap<String, OutputFile> = HashMap::new();

        let mut paths = glob_with(&format!("{}/ingest_buffer/*.part*", data_dir), options)
            .expect("Failed to read glob pattern")
            .filter_map(Result::ok)
            .collect::<Vec<_>>();

        paths.extend(glob_with(&format!("{}/deadletter_buffer/*.part*", data_dir), options)
            .expect("Failed to read glob pattern")
            .filter_map(Result::ok)
            .collect::<Vec<_>>());

        // limit path to 1000 files
        // paths.truncate(1000);

        for path in paths {
            // if path.is_dir() {
            //     break;
            // }

            let skpr_namespace =
                BufferChunker::decode_file_namespace(path.to_str().unwrap());
            let skpr_partition =
                BufferChunker::decode_file_partition(path.to_str().unwrap());
            let source_time = BufferChunker::decode_file_time(path.to_str().unwrap());
            let mut skpr_time = None;
            if source_time >= 0 {
                skpr_time = Some(source_time);
            }

            let finalised_file_name = BufferChunker::encode_chunk_name(
                "output",
                Some(&skpr_namespace),
                Some(&skpr_partition),
                skpr_time,
            );

            let dir = path.parent().unwrap().to_str().unwrap();

            let new_filename = format!(
                "{}/{}.merged",
                dir,
                finalised_file_name
            );
            let old_path = format!("{}", path.display().to_string());

            // println!("Merging file {} to {}", old_path, new_filename);

            let should_remove = {
                let mut new_file = match file_pointers.get_mut(&new_filename) {
                    Some(output_file) => output_file,
                    None => {
                        let file = match fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&new_filename)
                        {
                            Ok(file) => file,
                            Err(err) => {
                                println!("Error: {}, File: {}", err, new_filename);
                                continue;
                            }
                        };

                        let output_file = OutputFile {
                            bytes: file.len(),
                            updated_at: match file.metadata() {
                                Ok(metadata) => match metadata.modified() {
                                    Ok(time) => time,
                                    Err(err) => SystemTime::now(),
                                }
                                Err(err) => SystemTime::now(),
                            },
                            file,
                            rotated: None,
                        };

                        file_pointers.insert(new_filename.clone(), output_file);
                        file_pointers.get_mut(&new_filename).unwrap()
                    }
                };

                let mut old_file = match File::open(&old_path) {
                    Ok(file) => file,
                    Err(err) => {
                        println!("Error: {}, File: {}", err, old_path);
                        continue;
                    }
                };
                let mut buffer = Vec::new();

                match old_file.read_to_end(&mut buffer) {
                    Ok(_) => {}
                    Err(err) => {
                        println!("Error: {}, File: {}", err, old_path);
                        continue;
                    }
                };
                match new_file.file.write_all(&buffer) {
                    Ok(_) => {}
                    Err(err) => {
                        println!("Error in file {}: {}", new_filename, err)
                    }
                }
                match new_file.file.flush() {
                    Ok(_) => {}
                    Err(err) => {
                        println!("Error in file {}: {}", new_filename, err)
                    }
                }
                match new_file.file.sync_all() {
                    Ok(_) => {}
                    Err(err) => {
                        println!("Error in file {}: {}", new_filename, err)
                    }
                }

                new_file.bytes += buffer.len() as u64;
                new_file.updated_at = SystemTime::now();


                // match fs::remove_file(&old_path) {
                //     Ok(_t) => {}
                //     Err(err) => println!("{:?}", err),
                // }

                // tombstone file, can't delete it as OS may not delete immediately and we may write to it again
                // replace .merged to .tombstone and move to .{output_dir}/done
                let tombstone_file_name = old_path.rsplitn(2, "/").next().unwrap();
                // let tombstone_file_name = file_name_without_dir.replace(".merged", ".tombstone");
                let tombstone_file_path = format!("{}/done/{}", dir, tombstone_file_name);

                match fs::rename(old_path.as_str(), &tombstone_file_path) {
                    Ok(_) => {}
                    Err(_) => {}
                };

                // println!("Tomstoned file {}", &tombstone_file_path);

                let path = PathBuf::from(&new_filename);

                // if !force {
                //     BufferChunker::finalise_buffers(false, new_file, &new_filename)
                // } else {
                    // let finalize_file = OutputFile {
                    //     bytes: new_file.bytes,
                    //     updated_at: SystemTime::now(),
                    //     file: new_file.file.try_clone().unwrap(),
                    //     rotated: None,
                    // };
                    // files_to_finalize.insert(new_filename.clone(), finalize_file);


                    // false
                // }


            };

            // if should_remove {
            //     file_pointers.remove(&new_filename);
            // }

        }


        // Delete all tombstone files in done dir
        let options = MatchOptions {
            case_sensitive: false,
            require_literal_separator: false,
            require_literal_leading_dot: false,
        };

        // Finalize all files at the end if force is true
        // if force {
        //     let mut finalise_files_to_remove = vec![];

            let paths = glob_with(&format!("{}/ingest_buffer/*.merged", data_dir), options)
                .expect("Failed to read glob pattern")
                .filter_map(Result::ok)
                .collect::<Vec<_>>();

            for path in paths {

                let filename = path.to_str().unwrap().to_string();

                let file: OutputFile = match fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&filename)
                {
                    Ok(file) => OutputFile {
                        bytes: file.len(),
                        updated_at: match file.metadata() {
                            Ok(metadata) => match metadata.modified() {
                                Ok(time) => time,
                                Err(err) => SystemTime::now(),
                            },
                            Err(err) => SystemTime::now(),
                        },
                        file,
                        rotated: None,
                    },
                    Err(err) => {
                        println!("Error: {}, File: {}", err, filename);
                        continue;
                    }
                };

                BufferChunker::finalise_buffers(force, &file, &filename);
            }



        let paths = glob_with(&format!("{}/ingest_buffer/done/*", data_dir), options)
            .expect("Failed to read glob pattern")
            .filter_map(Result::ok)
            .collect::<Vec<_>>();

        for path in paths {
            // if path.is_dir() {
            //     break;
            // }

            match fs::remove_file(&path) {
                Ok(_t) => {}
                Err(err) => println!("{:?}", err),
            }
        }

        BUFFER_FINALISE_RUNNING
            .write()
            .store(false, Ordering::SeqCst);
    }

    pub fn finalise_buffers(force: bool, output_file: &OutputFile, filename: &String) -> bool {

        let flatten = Config::get_transform_flatten_events();

        let data_dir = Config::get_data_dir();
        let output_dir = &format!("{}/ingest_buffer", data_dir);
        let finalised_dir = &format!("{}/output_buffer", data_dir);

        let options = MatchOptions {
            case_sensitive: false,
            require_literal_separator: false,
            require_literal_leading_dot: false,
        };

        if force
            || BufferChunker::is_file_size_exceeded(&output_file)
            || BufferChunker::is_file_time_exceeded(&output_file)
        {

            // check filename exists on disk, very often not syned to disk yet
            // only check fs when necessary, i.e. when file is due to be rotated
            if !Path::new(filename).exists() {
                // println!("File {} does not exist", filename);
                // panic!("File {} does not exist", filename);
                // sleep(std::time::Duration::from_millis(5000));
                // panic!("File {} does not exist", filename);
                return false;
            }

            // println!("Finalising output file {}", filename);

            if output_file.bytes == 0 {
                println!("Skipping empty file {}", filename);
                // continue;
                return false;
            }

            // Always regenerate arrow schema incase updated skippr metadata, e.g. discovered a new field
            let mut arrow_schema: Result<datatypes::Schema, ArrowError> = Ok(datatypes::Schema::empty());
            let mut schema_ref = Arc::new(datatypes::Schema::empty());

            let skpr_namespace =
                BufferChunker::decode_file_namespace(filename.as_str());
            // let skpr_partition = BufferChunker::decode_file_partition(path.to_str().unwrap());

            let metadata = METADATA.read();

            if metadata.get(&skpr_namespace).is_some() {
                let mut output_metadata: HashMap<String, Metadata> = HashMap::new();
                if flatten {
                    let mut meta: HashMap<String, Metadata> = HashMap::new();

                    crate::flatten_metadata(metadata.get(&skpr_namespace).unwrap(), &mut meta);

                    let mut flat: Metadata = Metadata::new().unwrap();
                    flat.fields = Box::new(meta);
                    output_metadata.insert(skpr_namespace.clone(), flat);
                } else {
                    output_metadata = metadata.clone();
                }

                let skpr_partition =
                    BufferChunker::decode_file_partition(filename.as_str());
                let source_time = BufferChunker::decode_file_time(filename.as_str());
                let mut skpr_time = None;
                if source_time >= 0 {
                    skpr_time = Some(source_time);
                }

                arrow_schema = convert_skippr_to_arrow(
                    output_metadata.get(&skpr_namespace).unwrap().fields.clone(),
                );

                schema_ref = Arc::new(arrow_schema.unwrap());

                let path = PathBuf::from(&filename);

                let tmp_file_path = SerdeParquet::serialize(path, schema_ref);

                let finalised_file_name = BufferChunker::encode_chunk_name(
                    "output",
                    Some(&skpr_namespace),
                    Some(&skpr_partition),
                    skpr_time,
                );

                let finalised_file_path = &format!(
                    "{}/{}&part={}.parquet",
                    finalised_dir,
                    finalised_file_name,
                    Helpers::random_str(32).as_str()
                );

                match fs::rename(tmp_file_path, finalised_file_path) {
                    Ok(_) => {}
                    Err(_) => {}
                };

                // match std::fs::remove_file(filename.as_str()) {
                //     Ok(_t) => {}
                //     Err(err) => println!("{:?}", err),
                // }

                // @todo - tombstone file, can't delete it as OS may not delete immediately and we may write to it again

                // get filename without dir
                let file_name_without_dir = filename.rsplitn(2, "/").next().unwrap();
                let tombstone_file_name = file_name_without_dir.replace(".merged", ".tombstone");
                let tombstone_file_path = format!("{}/done/{}", output_dir, tombstone_file_name);

                match fs::rename(filename.as_str(), &tombstone_file_path) {
                    Ok(_) => {}
                    Err(_) => {}
                };

                // println!("Tomstoned file {}", &tombstone_file_path);

                println!("Finalised output file {}", finalised_file_path);

                return true;
            }
        }
        //             }
        //             Err(e) => println!("{:?}", e),
        //         }
        //     }
        // }

        false
    }

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

    pub fn encode_chunk_name(
        buffer_name: &str,
        namespace: Option<&str>,
        partition: Option<&str>,
        time_bucket: Option<i64>,
    ) -> String {
        let time_string = match time_bucket {
            Some(time_bucket) => format!("{}", time_bucket),
            None => "".to_string(),
        };

        let mut chunks = vec![
            ("buffer".to_string(), buffer_name.to_string()),
            ("namespace".to_string(), namespace.unwrap_or("").to_string()),
            ("partition".to_string(), partition.unwrap_or("").to_string()),
            ("time".to_string(), time_string),
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

    pub fn next_file(buffer_name: &str) -> Option<String> {
        let data_dir = Config::get_data_dir();
        let pattern = format!("{}/{}_buffer/buffer={}*", data_dir, buffer_name, buffer_name);

        let filenames = glob::glob(&pattern)
            .unwrap()
            .filter_map(Result::ok)
            .collect::<Vec<_>>();

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
        let actual_chunk_name = BufferChunker::encode_chunk_name(buffer_name, None, None, None);
        assert_eq!(expected_chunk_name, actual_chunk_name);
    }

    #[test]
    fn test_encode_chunk_name_with_namespace() {
        let buffer_name = "test_buffer";
        let namespace = Some("test_namespace");
        let expected_chunk_name = "buffer=test_buffer&namespace=test_namespace&partition=&time=";
        let actual_chunk_name =
            BufferChunker::encode_chunk_name(buffer_name, namespace, None, None);
        assert_eq!(expected_chunk_name, actual_chunk_name);
    }

    #[test]
    fn test_encode_chunk_name_with_partition() {
        let buffer_name = "test_buffer";
        let partition = Some("test_partition");
        let expected_chunk_name = "buffer=test_buffer&namespace=&partition=test_partition&time=";
        let actual_chunk_name =
            BufferChunker::encode_chunk_name(buffer_name, None, partition, None);
        assert_eq!(expected_chunk_name, actual_chunk_name);
    }

    #[test]
    fn test_encode_chunk_name_with_time_bucket() {
        let buffer_name = "test_buffer";
        let time_bucket = Some(123);
        let expected_chunk_name = "buffer=test_buffer&namespace=&partition=&time=123";
        let actual_chunk_name =
            BufferChunker::encode_chunk_name(buffer_name, None, None, time_bucket);
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
            BufferChunker::encode_chunk_name(buffer_name, namespace, partition, time_bucket);
        assert_eq!(expected_chunk_name, actual_chunk_name);
    }
}
