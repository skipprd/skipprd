use chrono::{DateTime, Datelike, NaiveDateTime, TimeZone, Timelike, Utc};
use url::form_urlencoded;

use std::collections::HashMap;

use std::fs::File;
use std::io::ErrorKind;

use std::str;

use parquet::data_type::AsBytes;

use crate::helpers::configuration::Config;
use crate::helpers::Helpers;

pub struct BufferChunker {}

impl BufferChunker {
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

        if let config_duration = Config::getenv("TRANSFORM_BATCH_TIME_UNIT", "") {
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
        let string = time_bucket.unwrap_or_default().to_string();
        let chunks = vec![
            ("buffer", buffer_name),
            ("namespace", namespace.unwrap_or("")),
            ("partition", partition.unwrap_or("")),
            ("time", &string),
        ];
        // if time_bucket.is_some() {
        //     let string = time_bucket.unwrap_or_default().to_string().clone();
        //     chunks.push(("time", &string));
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
        let mut time = 0;

        // Parse the query string into key-value pairs
        let pairs = url::form_urlencoded::parse(filename.as_bytes());

        // Print each key-value pair
        for (key, value) in pairs {
            if key == "time" {
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

        if time != 0 {
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
            // if filename.contains(".lock") || filename.contains(".checkpoint") {
            //     continue;
            // }

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
    fn test_get_file_chunk_time_with_invalid_time_query_param() {
        let filename = "buffer=test_buffer&namespace=&partition=time=invalid";
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
        assert_eq!(BufferChunker::get_file_time(filename), 0);
    }

    #[test]
    fn test_get_file_chunk_time_with_invalid_time_query_param() {
        let filename = "buffer=test_buffer&namespace=&partition=time=invalid";
        assert_eq!(BufferChunker::get_file_time(filename), 0);
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
        let expected_chunk_name = "buffer=test_buffer&namespace=&partition=&time=0";
        let actual_chunk_name = BufferChunker::encode_chunk_name(buffer_name, None, None, None);
        assert_eq!(expected_chunk_name, actual_chunk_name);
    }

    #[test]
    fn test_encode_chunk_name_with_namespace() {
        let buffer_name = "test_buffer";
        let namespace = Some("test_namespace");
        let expected_chunk_name = "buffer=test_buffer&namespace=test_namespace&partition=&time=0";
        let actual_chunk_name =
            BufferChunker::encode_chunk_name(buffer_name, namespace, None, None);
        assert_eq!(expected_chunk_name, actual_chunk_name);
    }

    #[test]
    fn test_encode_chunk_name_with_partition() {
        let buffer_name = "test_buffer";
        let partition = Some("test_partition");
        let expected_chunk_name = "buffer=test_buffer&namespace=&partition=test_partition&time=0";
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
