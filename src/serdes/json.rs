use serde::{Deserialize, Serialize};
use serde_json::Value;

use std::fs::File;
use std::io::{BufRead, BufReader, Lines, Result};
use std::path::Path;

use crate::helpers::configuration::Config;
use crate::serdes::optimized_json::OptimizedJsonParser;

#[derive(Serialize, Deserialize, Debug)]
pub struct SerdeJson {
    pub supported_compression_types: Vec<String>,
    pub compression_type: String,
    pub fh: String,
    pub records: Vec<String>,
}

impl SerdeJson {
    #[allow(dead_code)]
    pub fn new() -> SerdeJson {
        SerdeJson {
            supported_compression_types: vec![
                String::from("VALUE_COMPRESSION"),
                String::from("NO_COMPRESSION"),
            ],
            compression_type: String::from("NO_COMPRESSION"),
            fh: String::from(""),
            records: vec![],
        }
    }

    pub fn deserialize(record: &str) -> Vec<Value> {
        // Pre-allocate with a reasonable capacity
        let estimated_size = (record.lines().count() + 1).max(4);
        let mut messages: Vec<Value> = Vec::with_capacity(estimated_size);

        // Use the optimized parser
        let enable_sq = Self::is_single_quote_parsing_enabled();
        let enable_unicode = Self::is_unicode_parsing_enabled();
        let parser = OptimizedJsonParser::new(enable_sq, enable_unicode);

        // Fast path: Try using the optimized parser first
        let line = parser.parse(record);

        for item in line {
            if item.is_string() {
                // Only handle string items that need further parsing
                if let Some(s) = item.as_str() {
                    match serde_json::from_str::<Value>(s) {
                        Ok(message) => messages.push(message),
                        Err(_) => messages.push(item), // Keep the original string if parsing fails
                    }
                } else {
                    messages.push(item);
                }
            } else {
                messages.push(item);
            }
        }

        messages
    }

    // New method: Process multiple records in batch for better performance
    pub fn deserialize_batch(records: &[&str]) -> Vec<Value> {
        // Pre-allocate with a reasonable capacity
        let estimated_size = records.len() * 2;
        let mut messages: Vec<Value> = Vec::with_capacity(estimated_size);

        // Reuse parser for better performance
        let enable_sq = Self::is_single_quote_parsing_enabled();
        let enable_unicode = Self::is_unicode_parsing_enabled();
        let parser = OptimizedJsonParser::new(enable_sq, enable_unicode);

        for record in records {
            let line = parser.parse(record);
            messages.extend(line);
        }

        messages
    }

    #[allow(dead_code)]
    pub fn open_writer(&mut self, filename: String, _schema: Vec<Value>) {
        self.fh = filename;
    }

    #[allow(dead_code)]
    pub fn close_writer(&mut self) {
        for _data in self.records.iter() {
            if self.compression_type == "VALUE_COMPRESSION" {
            } else if self.compression_type == "NO_COMPRESSION" {
            }
        }
    }

    #[allow(dead_code)]
    pub fn serialize(&mut self, record: Vec<Value>, _schema: Vec<Value>) {
        self.records
            .push(serde_json::to_string(&record).unwrap_or_default());
    }

    // The output is wrapped in a Result to allow matching on errors
    // Returns an Iterator to the Reader of the lines of the file.
    #[allow(dead_code)]
    pub fn read_lines<P>(filename: P) -> Result<Lines<BufReader<File>>>
    where
        P: AsRef<Path>,
    {
        let file = File::open(filename)?;
        Ok(BufReader::new(file).lines())
    }

    // Check if single quote parsing is enabled
    fn is_single_quote_parsing_enabled() -> bool {
        Config::get_enable_single_quote_parsing()
    }

    // Check if unicode parsing is enabled
    fn is_unicode_parsing_enabled() -> bool {
        Config::get_enable_unicode_parsing()
    }

    pub fn json_decode(string: &str) -> Vec<Value> {
        // Create an instance of the optimized parser
        let enable_sq = Self::is_single_quote_parsing_enabled();
        let enable_unicode = Self::is_unicode_parsing_enabled();
        let parser = OptimizedJsonParser::new(enable_sq, enable_unicode);

        // Use the optimized parser
        parser.parse(string)
    }
}

#[cfg(test)]
mod json_serde_tests {
    use super::*;
    use serde_json::json;
    use serial_test::serial;
    use std::env;

    // Helper function to set up environment for tests
    fn setup_test_env(single_quotes: bool, unicode: bool) {
        env::set_var(
            "SKIPPR_ENABLE_SINGLE_QUOTE_PARSING",
            if single_quotes { "true" } else { "false" },
        );
        env::set_var(
            "SKIPPR_ENABLE_UNICODE_PARSING",
            if unicode { "true" } else { "false" },
        );
    }

    #[test]
    fn test_basic_valid_json_test() {
        let record: String = r#"{"status": "200"}"#.to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.first().unwrap()["status"], "200");
    }

    #[test]
    fn test_nested_valid_json_2() {
        let record: String = r#"{"status": "200", "items": {"foo": "bar"}}"#.to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.first().unwrap()["status"], "200");
        assert_eq!(msg.first().unwrap()["items"]["foo"], "bar");
    }

    #[test]
    fn test_nested_array_valid_json() {
        let record: String = r#"{"status": "200", "items": [{"foo": "bar"}]}"#.to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.first().unwrap()["status"], "200");
        assert_eq!(msg.first().unwrap()["items"][0]["foo"], "bar");
    }

    // #[test]
    // fn test_escaped_json() {
    //     let record: String = r#"{\"status\": \"200\"}"#.to_string();
    //     let msg = SerdeJson::deserialize(&record);
    //     assert_eq!(msg.first().unwrap()["status"], "200");
    // }

    // #[test]
    // fn test_double_escaped_json() {
    //     let record: String = r#"{\\\"time\\\":{\\\"start_time\\\":\\\"273.046328210292\\\",\\\"end_time\\\":\\\"16182\\\"},\\\"bike_id\\\":\\\"0.579087190592872\\\",\\\"location\\\":{\\\"start\\\":\\\"0.620131100002421\\\",\\\"end\\\":null}}"#.to_string();
    //     let msg = SerdeJson::deserialize(&record);
    //     assert_eq!(msg.first().unwrap()["bike_id"], "0.579087190592872");
    //     assert_eq!(
    //         msg.first().unwrap()["time"]["start_time"],
    //         "273.046328210292"
    //     );
    // }

    #[test]
    fn test_null_value_valid_json() {
        let record: String = r#"{"start":"0.620131100002421","end":null}"#.to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.first().unwrap()["start"], "0.620131100002421");
        assert_eq!(msg.first().unwrap()["end"], Value::Null);
    }

    #[test]
    fn test_single_quote_value_json() {
        let record: String = r#"{"binary": "b'H'"}"#.to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.first().unwrap()["binary"], "b'H'");
    }

    #[test]
    #[serial]
    fn test_single_quote_strings_json() {
        // Enable single quote parsing for this test
        setup_test_env(true, false);

        let record: String = r#"{'status': '200'}"#.to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.first().unwrap()["status"], "200");

        // Reset to default
        setup_test_env(false, false);
    }

    #[test]
    fn test_string_before_json() {
        let record: String = r#"some, string, that exists)/ 20080808115538 {"status":"200","length":"4742","mime":"text/html","offset":"16518203"}"#.to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.first().unwrap()["status"], "200");
        // assert!(msg.first().unwrap().get("some, string").is_none());
    }

    // #[test]
    // fn test_string_before_escaped_json() {
    //     let record: String = r#"some, string, that exists)/ 20080808115538 {\"status\":\"200\",\"length\":\"4742\",\"mime\":\"text/html\",\"offset\":\"16518203\"}"#.to_string();
    //     let msg = SerdeJson::deserialize(&record);
    //     assert_eq!(msg.first().unwrap()["status"], "200");
    //     // assert!(msg.first().unwrap().get("some, string").is_none());
    // }

    #[test]
    #[serial]
    fn test_unicode_string_json() {
        // Enable both unicode parsing and single quote parsing for this test
        setup_test_env(true, true);

        let record: String = r#"{u'status': u'200'}"#.to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.first().unwrap()["status"], "200");

        // Reset to default
        setup_test_env(false, false);
    }

    #[test]
    fn test_unicode_string_value_json() {
        let record: String = r#"{"status": "\u0023"}"#.to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.first().unwrap()["status"], "#");
        assert_ne!(msg.first().unwrap()["status"], 2605);
    }

    #[test]
    fn test_multi_record_array_json() {
        let record: String = r#"[{"status": "200"},{"status": "500"}]"#.to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.first().unwrap()["status"], "200");
        assert_eq!(msg.last().unwrap()["status"], "500");
    }

    #[test]
    fn test_valid_multi_line_json() {
        let record: String = "{\"status\": \"200\"}\n{\"status\": \"500\"}".to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.first().unwrap()["status"], "200");
        assert_eq!(msg.last().unwrap()["status"], "500");
    }

    #[test]
    fn test_valid_multi_line_with_multi_record_arrays_json() {
        let record: String = "[{\"status\": \"200\"},{\"status\": \"201\"}]\n[{\"status\": \"202\"},{\"status\": \"203\"}]".to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.first().unwrap()[0]["status"], "200");
        assert_eq!(msg.first().unwrap()[1]["status"], "201");
        assert_eq!(msg.last().unwrap()[0]["status"], "202");
        assert_eq!(msg.last().unwrap()[1]["status"], "203");
    }

    /**
     * This madness is common, for instance AWS Firehose S3 Destination produces this crap
     */
    #[test]
    fn test_single_line_objects_json() {
        let record: String =
            r#"{"foo": {"nest": "bar"}}{"foo": {"nest": "baz"}}{"foo": {"nest": "boo"}}"#
                .to_string();
        let msg = SerdeJson::deserialize(&record);
        // assert!( msg.first().unwrap().is_array());
        assert_eq!(msg[0]["foo"]["nest"], "bar");
        assert_eq!(msg[1]["foo"]["nest"], "baz");
        assert_eq!(msg[2]["foo"]["nest"], "boo");
    }

    /**
     * Test case for more complex concatenated JSON objects
     * This tests the handling of complex nested objects that are concatenated without separators
     */
    #[test]
    fn test_single_line_objects_json_cc() {
        let record: String =
            r#"{"id":"123","data":{"value":42,"metadata":{"source":"system"}}}{"id":"456","data":{"value":99,"metadata":{"source":"user"}}}"#
                .to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.len(), 2);
        assert_eq!(msg[0]["id"], "123");
        assert_eq!(msg[0]["data"]["value"], 42);
        assert_eq!(msg[0]["data"]["metadata"]["source"], "system");
        assert_eq!(msg[1]["id"], "456");
        assert_eq!(msg[1]["data"]["value"], 99);
        assert_eq!(msg[1]["data"]["metadata"]["source"], "user");
    }

    #[test]
    fn test_deserialize_reparses_stringified_json_payloads() {
        let record = r#""{\"status\":\"200\"}""#.to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg, vec![json!({"status": "200"})]);
    }

    #[test]
    fn test_deserialize_preserves_plain_string_when_reparse_fails() {
        let record = r#""plain string""#.to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg, vec![Value::String("plain string".to_string())]);
    }

    #[test]
    fn test_deserialize_batch_parses_multiple_records_and_arrays() {
        let records = [
            r#"{"status":"200"}{"status":"201"}"#,
            r#"[{"status":"202"}]"#,
        ];
        let msg = SerdeJson::deserialize_batch(&records);
        assert_eq!(msg.len(), 3);
        assert_eq!(msg[0]["status"], "200");
        assert_eq!(msg[1]["status"], "201");
        assert_eq!(msg[2]["status"], "202");
    }

    #[test]
    fn test_json_decode_respects_feature_flags() {
        setup_test_env(true, true);

        let msg = SerdeJson::json_decode("{u'status': u'200'}");
        assert_eq!(msg, vec![json!({"status": "200"})]);

        setup_test_env(false, false);
    }
}
