pub mod ingest_buffer;
pub mod segment_file;
pub mod segment_object;
pub mod wal_store;
// wal_accumulator removed in simplified model

use chrono::{DateTime, Datelike, TimeZone, Timelike, Utc};
use url::form_urlencoded;

use std::collections::HashMap;
use std::fs::File;

use std::io::ErrorKind;

use std::string::ToString;

use crate::helpers::configuration::Config;
use crate::helpers::Helpers;
use tracing::warn;

pub struct BufferChunker {}

impl BufferChunker {
    #[allow(dead_code)]
    pub fn check_flush_limit(_chunk_name: &str, _chunk: &HashMap<String, usize>) -> bool {
        let mut result = false;

        if Helpers::mem_limit_reached() {
            warn!("Rotating buffer as memory limit has low headroom");
            result = true;
        }

        result
    }

    pub fn event_time_bucket(event_time: i64) -> i64 {
        let datetime = Utc.timestamp_opt(event_time, 0).unwrap();

        let bucket_rounded_timestamp = match Config::get_transform_batch_time_unit().as_str() {
            "year" => {
                let year = datetime.year();
                Utc.with_ymd_and_hms(year, 1, 1, 0, 0, 0)
                    .unwrap()
                    .timestamp()
            }
            "month" => {
                let year = datetime.year();
                let month = datetime.month();
                Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0)
                    .unwrap()
                    .timestamp()
            }
            "day" => {
                let year = datetime.year();
                let month = datetime.month();
                let day = datetime.day();
                Utc.with_ymd_and_hms(year, month, day, 0, 0, 0)
                    .unwrap()
                    .timestamp()
            }
            "hour" => {
                let year = datetime.year();
                let month = datetime.month();
                let day = datetime.day();
                let hour = datetime.hour();
                Utc.with_ymd_and_hms(year, month, day, hour, 0, 0)
                    .unwrap()
                    .timestamp()
            }
            "minute" => {
                let year = datetime.year();
                let month = datetime.month();
                let day = datetime.day();
                let hour = datetime.hour();
                let minute = datetime.minute();
                Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
                    .unwrap()
                    .timestamp()
            }
            _ => 0,
        };

        if bucket_rounded_timestamp > 0 {
            event_time - ((event_time).rem_euclid(bucket_rounded_timestamp))
        } else {
            0
        }
    }

    #[allow(dead_code)]
    pub fn decode_chunk_string_from_filename(filename: &str) -> String {
        let pairs = url::form_urlencoded::parse(filename.as_bytes());
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

        let chunk_name = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(chunks)
            .finish();

        chunk_name
    }

    fn get_file_time(filename: &str) -> i64 {
        let mut time = -1;

        let filename = filename.trim_start_matches("./");
        let filename = filename.split('.').next().unwrap();

        let pairs = url::form_urlencoded::parse(filename.as_bytes());

        for (key, value) in pairs {
            if key == "time" {
                if value.is_empty() {
                    continue;
                }
                time = value.parse::<i64>().unwrap_or(0);
            }
        }

        time
    }

    fn get_file_part(filename: &str, part_name: &str) -> String {
        let mut part = "".to_string();

        let pairs = url::form_urlencoded::parse(filename.as_bytes());

        for (key, value) in pairs {
            if key == part_name {
                part = value.to_string()
            }
        }

        part
    }

    pub fn decode_file_time_to_datetime_string(filename: &str) -> String {
        let time = BufferChunker::get_file_time(filename);

        if time >= 0 {
            DateTime::<Utc>::from_timestamp(time, 0)
                .unwrap_or_default()
                .to_rfc3339()
        } else {
            "".to_string()
        }
    }

    pub fn decode_file_time(filename: &str) -> i64 {
        BufferChunker::get_file_time(filename)
    }

    pub fn decode_file_partition(filename: &str) -> String {
        let mut path_parts = "".to_string();

        let partition = BufferChunker::get_file_part(filename, "partition");

        if !partition.is_empty() {
            let parts = partition.split("%2D");
            let collection: Vec<&str> = parts.collect();

            path_parts = collection.join("/");
        }

        path_parts
    }

    pub fn decode_file_namespace(filename: &str) -> String {
        BufferChunker::get_file_part(filename, "namespace")
    }

    pub fn decode_file_shard(filename: &str) -> String {
        BufferChunker::get_file_part(filename, "shard")
    }

    #[allow(dead_code)]
    pub fn next_file(buffer_name: &str) -> Option<String> {
        let data_dir = Config::get_data_dir();
        let pattern = format!(
            "{}/{}_buffer/buffer={}*.parquet",
            data_dir, buffer_name, buffer_name
        );

        let filenames = match glob::glob(&pattern) {
            Ok(filenames) => filenames.filter_map(Result::ok).collect::<Vec<_>>(),
            Err(e) => {
                warn!("Error globbing for next buffer file, Error: {}", e);
                return None;
            }
        };

        for filename in filenames {
            match File::open(&filename) {
                Err(ref e) if e.kind() == ErrorKind::NotFound => {
                    continue;
                }
                Err(e) => {
                    warn!("Error opening file {}: {}", filename.to_str().unwrap(), e);
                    continue;
                }
                Ok(_file) => {
                    return Some(filename.to_str().unwrap().to_string());
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
        assert_eq!(
            BufferChunker::decode_file_time_to_datetime_string(filename),
            "2022-02-19T18:40:45+00:00"
        );
    }

    #[test]
    fn test_get_file_chunk_time_with_valid_and_shard_and_extentin() {
        let filename =
            "buffer=output&namespace=bike_hire&partition=&time=1645296045&shard=1.merged";
        assert_eq!(
            BufferChunker::decode_file_time_to_datetime_string(filename),
            "2022-02-19T18:40:45+00:00"
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

    #[test]
    fn test_get_file_chunk_time_with_valid_and_shard() {
        let filename = "buffer=test_buffer&namespace=&partition=&time=1645296045&shard=1";
        assert_eq!(BufferChunker::get_file_time(filename), 1645296045);
    }

    #[test]
    fn test_get_file_chunk_time_with_valid_and_shard_and_extentin() {
        let filename =
            "buffer=output&namespace=bike_hire&partition=&time=1645296045&shard=1.merged";
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

#[cfg(test)]
mod encode_chunk_name_tests {

    use crate::buffer::BufferChunker;

    #[test]
    fn test_encode_chunk_name_no_options() {
        let buffer_name = "test_buffer";
        let expected_chunk_name = "buffer=test_buffer&namespace=&partition=&time=&shard=";
        let actual_chunk_name =
            BufferChunker::encode_chunk_name(buffer_name, None, None, None, None);
        assert_eq!(expected_chunk_name, actual_chunk_name);
    }

    #[test]
    fn test_encode_chunk_name_with_namespace() {
        let buffer_name = "test_buffer";
        let namespace = Some("test_namespace");
        let expected_chunk_name =
            "buffer=test_buffer&namespace=test_namespace&partition=&time=&shard=";
        let actual_chunk_name =
            BufferChunker::encode_chunk_name(buffer_name, namespace, None, None, None);
        assert_eq!(expected_chunk_name, actual_chunk_name);
    }

    #[test]
    fn test_encode_chunk_name_with_partition() {
        let buffer_name = "test_buffer";
        let partition = Some("test_partition");
        let expected_chunk_name =
            "buffer=test_buffer&namespace=&partition=test_partition&time=&shard=";
        let actual_chunk_name =
            BufferChunker::encode_chunk_name(buffer_name, None, partition, None, None);
        assert_eq!(expected_chunk_name, actual_chunk_name);
    }

    #[test]
    fn test_encode_chunk_name_with_time_bucket() {
        let buffer_name = "test_buffer";
        let time_bucket = Some(123);
        let expected_chunk_name = "buffer=test_buffer&namespace=&partition=&time=123&shard=";
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
            "buffer=test_buffer&namespace=test_namespace&partition=test_partition&time=456&shard=";
        let actual_chunk_name =
            BufferChunker::encode_chunk_name(buffer_name, namespace, partition, time_bucket, None);
        assert_eq!(expected_chunk_name, actual_chunk_name);
    }
}
