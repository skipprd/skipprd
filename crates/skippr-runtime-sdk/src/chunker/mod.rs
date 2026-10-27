use chrono::{DateTime, Utc};
use std::collections::HashMap;
use url::form_urlencoded;

pub struct BufferChunker;

impl BufferChunker {
    pub fn encode_chunk_name(
        buffer_name: &str,
        sink_ref: Option<&str>,
        namespace: Option<&str>,
        partition: Option<&str>,
        time_bucket: Option<i64>,
        schema_fingerprint: Option<&str>,
    ) -> String {
        let time_string = match time_bucket {
            Some(time_bucket) => format!("{time_bucket}"),
            None => "".to_string(),
        };
        let schema_fingerprint_string = schema_fingerprint.unwrap_or("").to_string();
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
        form_urlencoded::Serializer::new(String::new())
            .extend_pairs(chunks)
            .finish()
    }

    fn get_file_time(filename: &str) -> i64 {
        let mut time = -1;
        let filename = filename.trim_start_matches("./");
        for (key, value) in url::form_urlencoded::parse(filename.as_bytes()) {
            if key == "time" {
                if value.is_empty() {
                    continue;
                }
                let clean = value.split('.').next().unwrap_or(&value);
                time = clean.parse::<i64>().unwrap_or(0);
            }
        }
        time
    }

    fn get_file_part(filename: &str, part_name: &str) -> String {
        let mut part = "".to_string();
        for (key, value) in url::form_urlencoded::parse(filename.as_bytes()) {
            if key == part_name {
                part = value.to_string();
            }
        }
        part
    }

    pub fn decode_file_time_to_datetime_string(filename: &str) -> String {
        let time = Self::get_file_time(filename);
        if time >= 0 {
            DateTime::<Utc>::from_timestamp(time, 0)
                .unwrap_or_default()
                .to_rfc3339()
        } else {
            "".to_string()
        }
    }

    pub fn decode_file_time(filename: &str) -> i64 {
        Self::get_file_time(filename)
    }

    pub fn decode_file_partition(filename: &str) -> String {
        let partition = Self::get_file_part(filename, "partition");
        if partition.is_empty() {
            return String::new();
        }
        partition.split("%2D").collect::<Vec<_>>().join("/")
    }

    pub fn decode_file_namespace(filename: &str) -> String {
        Self::get_file_part(filename, "namespace")
    }

    pub fn decode_file_sink_ref(filename: &str) -> String {
        let sink_type = Self::get_file_part(filename, "sink_type");
        let sink_name = Self::get_file_part(filename, "sink_name");
        if sink_type.is_empty() {
            return Self::get_file_part(filename, "sink");
        }
        if sink_name.is_empty() {
            sink_type
        } else {
            format!("{sink_type}.{sink_name}")
        }
    }

    pub fn decode_file_schema_fingerprint(filename: &str) -> String {
        Self::get_file_part(filename, "schema_fingerprint")
    }

    pub fn decode_chunk_string_from_filename(filename: &str) -> String {
        let pairs = url::form_urlencoded::parse(filename.as_bytes());
        let mut chunks = HashMap::new();
        pairs.into_iter().for_each(|(key, value)| {
            chunks.insert(key, value);
        });
        form_urlencoded::Serializer::new(String::new())
            .extend_pairs(chunks)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::BufferChunker;

    #[test]
    fn encode_decode_sink_ref_roundtrip() {
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
        assert_eq!(BufferChunker::decode_file_time(&name), 100);
        assert_eq!(BufferChunker::decode_file_namespace(&name), "ns");
    }

    #[test]
    fn decode_file_time_valid_and_missing() {
        assert_eq!(
            BufferChunker::decode_file_time(
                "buffer=test_buffer&namespace=&partition=&time=1645296045"
            ),
            1645296045
        );
        assert_eq!(
            BufferChunker::decode_file_time("buffer=test_buffer&namespace=&partition="),
            -1
        );
        assert_eq!(
            BufferChunker::decode_file_time(
                "buffer=test_buffer&namespace=&partition=&time=invalid"
            ),
            0
        );
        assert_eq!(
            BufferChunker::decode_file_time(
                "buffer=test_buffer&namespace=&partition=&time=1645296045.part"
            ),
            1645296045
        );
        assert_eq!(
            BufferChunker::decode_file_time(
                "buffer=output&namespace=bike_hire&partition=&time=1645296045&schema_fingerprint=1.merged"
            ),
            1645296045
        );
        assert_eq!(
            BufferChunker::decode_file_time(
                "buffer=output&sink_type=data_outputs&sink_name=test_datalake&namespace=test&partition=&time=1700000280&schema_fingerprint=abc-c=def"
            ),
            1700000280
        );
    }

    #[test]
    fn decode_file_time_to_datetime_string_cases() {
        assert_eq!(
            BufferChunker::decode_file_time_to_datetime_string(
                "buffer=test_buffer&namespace=&partition=&time=1645296045"
            ),
            "2022-02-19T18:40:45+00:00"
        );
        assert_eq!(
            BufferChunker::decode_file_time_to_datetime_string(
                "buffer=test_buffer&namespace=&partition="
            ),
            ""
        );
        assert_eq!(
            BufferChunker::decode_file_time_to_datetime_string(
                "buffer=test_buffer&namespace=&partition=&time=0"
            ),
            "1970-01-01T00:00:00+00:00"
        );
        assert_eq!(
            BufferChunker::decode_file_time_to_datetime_string(
                "buffer=test_buffer&namespace=&partition=&time=invalid"
            ),
            "1970-01-01T00:00:00+00:00"
        );
        assert_eq!(
            BufferChunker::decode_file_time_to_datetime_string(
                "buffer=test_buffer&namespace=&partition=&time="
            ),
            ""
        );
        assert_eq!(
            BufferChunker::decode_file_time_to_datetime_string(
                "buffer=output&namespace=bike_hire&partition=&time=1645296045&schema_fingerprint=1.merged"
            ),
            "2022-02-19T18:40:45+00:00"
        );
    }

    #[test]
    fn encode_chunk_name_fields() {
        assert_eq!(
            BufferChunker::encode_chunk_name("test_buffer", None, None, None, None, None),
            "buffer=test_buffer&sink_type=&sink_name=&namespace=&partition=&time=&schema_fingerprint="
        );
        assert_eq!(
            BufferChunker::encode_chunk_name(
                "test_buffer",
                None,
                Some("test_namespace"),
                None,
                None,
                None
            ),
            "buffer=test_buffer&sink_type=&sink_name=&namespace=test_namespace&partition=&time=&schema_fingerprint="
        );
        assert_eq!(
            BufferChunker::encode_chunk_name(
                "test_buffer",
                None,
                None,
                Some("test_partition"),
                None,
                None
            ),
            "buffer=test_buffer&sink_type=&sink_name=&namespace=&partition=test_partition&time=&schema_fingerprint="
        );
        assert_eq!(
            BufferChunker::encode_chunk_name("test_buffer", None, None, None, Some(123), None),
            "buffer=test_buffer&sink_type=&sink_name=&namespace=&partition=&time=123&schema_fingerprint="
        );
        assert_eq!(
            BufferChunker::encode_chunk_name(
                "test_buffer",
                None,
                Some("test_namespace"),
                Some("test_partition"),
                Some(456),
                None
            ),
            "buffer=test_buffer&sink_type=&sink_name=&namespace=test_namespace&partition=test_partition&time=456&schema_fingerprint="
        );
    }
}

pub mod deadletter;
pub mod partition_time;
