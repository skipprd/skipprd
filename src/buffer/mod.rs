pub mod compaction_progress;
pub mod compaction_transaction;
pub mod completion_ledger;
pub mod ingest_buffer;
pub mod sink_conflict;
pub mod s3_wal_body_cache;
pub mod s3_wal_memory_budget;
pub mod segment_file;
pub mod segment_object;
pub mod wal_store;
pub mod wal_writer;
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
        sink_ref: Option<&str>,
        namespace: Option<&str>,
        partition: Option<&str>,
        time_bucket: Option<i64>,
        schema_fingerprint: Option<&str>,
    ) -> String {
        let time_string = match time_bucket {
            Some(time_bucket) => format!("{}", time_bucket),
            None => "".to_string(),
        };

        let schema_fingerprint_string = match schema_fingerprint {
            Some(schema_fingerprint) => format!("{}", schema_fingerprint),
            None => "".to_string(),
        };

        let raw_sink = sink_ref.unwrap_or("");
        let (sink_type, sink_name) = raw_sink.split_once('.').unwrap_or((raw_sink, ""));

        let chunks = vec![
            ("buffer".to_string(), buffer_name.to_string()),
            ("sink_type".to_string(), sink_type.to_string()),
            ("sink_name".to_string(), sink_name.to_string()),
            ("namespace".to_string(), namespace.unwrap_or("").to_string()),
            ("partition".to_string(), partition.unwrap_or("").to_string()),
            ("time".to_string(), time_string),
            ("schema_fingerprint".to_string(), schema_fingerprint_string),
        ];

        let chunk_name = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(chunks)
            .finish();

        chunk_name
    }

    fn get_file_time(filename: &str) -> i64 {
        let mut time = -1;

        let filename = filename.trim_start_matches("./");

        let pairs = url::form_urlencoded::parse(filename.as_bytes());

        for (key, value) in pairs {
            if key == "time" {
                if value.is_empty() {
                    continue;
                }
                // Strip trailing file extension (e.g. ".part", ".parquet") from
                // the value; other parameters may legitimately contain periods
                // (e.g. sink=data_outputs.test_datalake), so we only trim here
                // rather than splitting the entire filename on '.'.
                let clean = value.split('.').next().unwrap_or(&value);
                time = clean.parse::<i64>().unwrap_or(0);
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

    pub fn decode_file_sink_ref(filename: &str) -> String {
        let sink_type = BufferChunker::get_file_part(filename, "sink_type");
        let sink_name = BufferChunker::get_file_part(filename, "sink_name");
        if sink_type.is_empty() {
            // Backwards-compat: fall back to single "sink" key for old WAL files
            return BufferChunker::get_file_part(filename, "sink");
        }
        if sink_name.is_empty() {
            sink_type
        } else {
            format!("{}.{}", sink_type, sink_name)
        }
    }

    pub fn decode_file_schema_fingerprint(filename: &str) -> String {
        BufferChunker::get_file_part(filename, "schema_fingerprint")
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
    fn test_get_file_chunk_time_with_valid_and_schema_fingerprint() {
        let filename =
            "buffer=test_buffer&namespace=&partition=&time=1645296045&schema_fingerprint=1";
        assert_eq!(
            BufferChunker::decode_file_time_to_datetime_string(filename),
            "2022-02-19T18:40:45+00:00"
        );
    }

    #[test]
    fn test_get_file_chunk_time_with_valid_and_schema_fingerprint_and_extentin() {
        let filename = "buffer=output&namespace=bike_hire&partition=&time=1645296045&schema_fingerprint=1.merged";
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
    fn test_get_file_chunk_time_with_valid_and_schema_fingerprint() {
        let filename =
            "buffer=test_buffer&namespace=&partition=&time=1645296045&schema_fingerprint=1";
        assert_eq!(BufferChunker::get_file_time(filename), 1645296045);
    }

    #[test]
    fn test_get_file_chunk_time_with_valid_and_schema_fingerprint_and_extentin() {
        let filename = "buffer=output&namespace=bike_hire&partition=&time=1645296045&schema_fingerprint=1.merged";
        assert_eq!(BufferChunker::get_file_time(filename), 1645296045);
    }

    #[test]
    fn test_get_file_chunk_time_with_split_sink_ref() {
        let filename = "buffer=output&sink_type=data_outputs&sink_name=test_datalake&namespace=test&partition=&time=1700000280&schema_fingerprint=abc-c=def";
        assert_eq!(BufferChunker::get_file_time(filename), 1700000280);
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
        let expected_chunk_name = "buffer=test_buffer&sink_type=&sink_name=&namespace=&partition=&time=&schema_fingerprint=";
        let actual_chunk_name =
            BufferChunker::encode_chunk_name(buffer_name, None, None, None, None, None);
        assert_eq!(expected_chunk_name, actual_chunk_name);
    }

    #[test]
    fn test_encode_chunk_name_with_namespace() {
        let buffer_name = "test_buffer";
        let namespace = Some("test_namespace");
        let expected_chunk_name = "buffer=test_buffer&sink_type=&sink_name=&namespace=test_namespace&partition=&time=&schema_fingerprint=";
        let actual_chunk_name =
            BufferChunker::encode_chunk_name(buffer_name, None, namespace, None, None, None);
        assert_eq!(expected_chunk_name, actual_chunk_name);
    }

    #[test]
    fn test_encode_chunk_name_with_partition() {
        let buffer_name = "test_buffer";
        let partition = Some("test_partition");
        let expected_chunk_name = "buffer=test_buffer&sink_type=&sink_name=&namespace=&partition=test_partition&time=&schema_fingerprint=";
        let actual_chunk_name =
            BufferChunker::encode_chunk_name(buffer_name, None, None, partition, None, None);
        assert_eq!(expected_chunk_name, actual_chunk_name);
    }

    #[test]
    fn test_encode_chunk_name_with_time_bucket() {
        let buffer_name = "test_buffer";
        let time_bucket = Some(123);
        let expected_chunk_name = "buffer=test_buffer&sink_type=&sink_name=&namespace=&partition=&time=123&schema_fingerprint=";
        let actual_chunk_name =
            BufferChunker::encode_chunk_name(buffer_name, None, None, None, time_bucket, None);
        assert_eq!(expected_chunk_name, actual_chunk_name);
    }

    #[test]
    fn test_encode_chunk_name_with_all_options() {
        let buffer_name = "test_buffer";
        let namespace = Some("test_namespace");
        let partition = Some("test_partition");
        let time_bucket = Some(456);
        let expected_chunk_name = "buffer=test_buffer&sink_type=&sink_name=&namespace=test_namespace&partition=test_partition&time=456&schema_fingerprint=";
        let actual_chunk_name = BufferChunker::encode_chunk_name(
            buffer_name,
            None,
            namespace,
            partition,
            time_bucket,
            None,
        );
        assert_eq!(expected_chunk_name, actual_chunk_name);
    }

    #[test]
    fn test_encode_decode_sink_ref_roundtrip() {
        let name = BufferChunker::encode_chunk_name(
            "output",
            Some("data_outputs.test_datalake"),
            Some("ns"),
            None,
            Some(100),
            None,
        );
        assert_eq!(
            BufferChunker::decode_file_sink_ref(&name),
            "data_outputs.test_datalake"
        );
        assert_eq!(BufferChunker::get_file_time(&name), 100);
    }
}
