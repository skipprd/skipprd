use serde::{Deserialize, Serialize};
use serde_json::{Value};

use std::fs::{File, OpenOptions, remove_file};
use std::path::Path;
use std::io::{Seek, Write, BufReader, Lines, Result, BufRead};



use rand::{Rng};
use serde_json::Value::Null;
use crate::discover::AnalyseSchema;
use crate::helpers::configuration::Config;


#[derive(Serialize, Deserialize, Debug)]
pub struct SerdeJson {
    pub supported_compression_types: Vec<String>,
    pub compression_type: String,
    pub fh: String,
    pub records: Vec<String>,
}

impl SerdeJson {
    pub fn new() -> SerdeJson {
        SerdeJson {
            supported_compression_types: vec![String::from("VALUE_COMPRESSION"), String::from("NO_COMPRESSION")],
            compression_type: String::from("NO_COMPRESSION"),
            fh: String::from(""),
            records: vec![],
        }
    }

    pub fn deserialize(record: &str) -> Vec<Value> {
        let mut messages: Vec<Value> = Vec::new();

        let line: Vec<Value> = SerdeJson::json_decode(record);

        for item in line {
            if item.is_string() {
                match serde_json::from_str::<Value>(&item.as_str().unwrap_or_default()) {
                    Ok(message) => messages.push(message),
                    Err(e) => println!("Couldn't deserialize message: {}", e),
                }
            } else {
                messages.push(item);
            }
        }

        messages
    }

    pub fn open_writer(&mut self, filename: String, _schema: Vec<Value>) {
        self.fh = filename;
    }

    pub fn close_writer(&mut self) {
        for _data in self.records.iter() {
            if self.compression_type == "VALUE_COMPRESSION" {
            } else if self.compression_type == "NO_COMPRESSION" {
            }
        }
    }

    pub fn serialize(&mut self, record: Vec<Value>, _schema: Vec<Value>) {
        self.records.push(serde_json::to_string(&record).unwrap_or_default());
    }

    // The output is wrapped in a Result to allow matching on errors
    // Returns an Iterator to the Reader of the lines of the file.
    pub fn read_lines<P>(filename: P) -> Result<Lines<BufReader<File>>>
        where P: AsRef<Path>, {
        let file = File::open(filename)?;
        Ok(BufReader::new(file).lines())
    }


    pub fn json_decode(string: &str) -> Vec<Value> {
        let mut message: Vec<Value> = Vec::new();

        match serde_json::from_str::<Value>(string) {
            Ok(Value::Array(lines)) => {
                message.extend(lines);
            }
            Ok(line) => {
                message.push(line);
            }
            Err(_) => {
                let lines = string
                    .lines()
                    .map(|line| {
                        let mut cleaned_line = line
                            .replace("\\", "")
                            .replace("u'", "\"")
                            .replace("'", "\"");

                        let valid_chars: String = cleaned_line
                            .chars()
                            .filter(|c| !c.is_ascii_control())
                            .collect();

                        if valid_chars.starts_with("efbbbf") {
                            cleaned_line = valid_chars.replace("efbbbf", "");
                        }

                        if let Some(json_start) = cleaned_line.find(|c| c == '[' || c == '{') {
                            cleaned_line.drain(..json_start);
                        }

                        cleaned_line
                    })
                    .collect::<Vec<_>>();

                let mut deserialized_lines: Vec<Value> = lines
                    .iter()
                    .map(|line| serde_json::from_str(&line).unwrap_or_default())
                    .collect();

                if deserialized_lines.is_empty() || deserialized_lines.first().unwrap() == &Value::Null {
                    deserialized_lines.clear();

                    for line in lines {
                        let records: Vec<&str> = line.split("}{").collect();

                        for (i, record) in records.iter().enumerate() {
                            let mut record = record.to_string();

                            if i != 0 {
                                record.insert(0, '{');
                            }

                            if i != records.len() - 1 {
                                record.push('}');
                            }

                            deserialized_lines.push(serde_json::from_str(&record).unwrap_or_default());
                        }
                    }
                }

                message.extend(deserialized_lines);
            }
        }

        message
    }

}

#[test]
fn test_json_serde() {
    #[test]
    fn test_basic_valid_json() {
        let record: String = r#"{"status": "200"}"#.to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.first().unwrap()["status"], "200");
    }

    #[test]
    fn test_nested_valid_json() {
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

    #[test]
    fn test_escaped_json() {
        let record: String = r#"{\"status\": \"200\"}"#.to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.first().unwrap()["status"], "200");
    }

    #[test]
    fn test_double_escaped_json() {
        let record: String = r#"{\\\"time\\\":{\\\"start_time\\\":\\\"273.046328210292\\\",\\\"end_time\\\":\\\"16182\\\"},\\\"bike_id\\\":\\\"0.579087190592872\\\",\\\"location\\\":{\\\"start\\\":\\\"0.620131100002421\\\",\\\"end\\\":null}}"#.to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.first().unwrap()["bike_id"], "0.579087190592872");
        assert_eq!(msg.first().unwrap()["time"]["start_time"], "273.046328210292");
    }

    #[test]
    fn test_null_value_valid_json() {
        let record: String = r#"{"start":"0.620131100002421","end":null}"#.to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.first().unwrap()["start"], "0.620131100002421");
        assert_eq!(msg.first().unwrap()["end"], Null);
    }

    #[test]
    fn test_signle_quote_strings_json() {
        let record: String = r#"{'status': '200'}"#.to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.first().unwrap()["status"], "200");
    }

    #[test]
    fn test_string_before_escaped_json() {
        let record: String = r#"some, string, that exists)/ 20080808115538 {\"status\":\"200\",\"length\":\"4742\",\"mime\":\"text/html\",\"offset\":\"16518203\"}"#.to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.first().unwrap()["status"], "200");
        // assert!(msg.first().unwrap().get("some, string").is_none());
    }

    #[test]
    fn test_unicode_string_json() {
        let record: String = r#"{u'status': u'200'}"#.to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.first().unwrap()["status"], "200");
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
        let record: String = r#"{"foo": {"nest": "bar"}}{"foo": {"nest": "baz"}}{"foo": {"nest": "boo"}}"#.to_string();
        let msg = SerdeJson::deserialize(&record);
        // assert!( msg.first().unwrap().is_array());
        assert_eq!(msg[0]["foo"]["nest"], "bar");
        assert_eq!(msg[1]["foo"]["nest"], "baz");
        assert_eq!(msg[2]["foo"]["nest"], "boo");
    }
}