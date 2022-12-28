use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::File;
use std::path::Path;
use std::io::{Read, Seek, Write, BufReader, Lines, Result, BufRead};
use std::ops::Index;
use serde_json::Value::Null;
use crate::discover::AnalyseSchema;


#[derive(Serialize, Deserialize, Debug)]
pub struct SerderJson {
    pub supported_compression_types: Vec<String>,
    pub compression_type: String,
    pub fh: String,
    pub records: Vec<String>,
}

impl SerderJson {
    pub fn new() -> SerderJson {
        SerderJson {
            supported_compression_types: vec![String::from("VALUE_COMPRESSION"), String::from("NO_COMPRESSION")],
            compression_type: String::from("NO_COMPRESSION"),
            fh: String::from(""),
            records: vec![],
        }
    }

    pub fn deserialize(record: String) -> Vec<Value> {
        let mut message: Vec<Value> = vec![];
        let mut records: Vec<Value> = vec![];
        let mut messages: Vec<Value> = vec![];

        // deserialise handling multiline json
        let data = record;

        records = vec![];

        // let mut line: Value = match self.json_decode(data) {
        //     Ok(line) => line,
        //
        //     Err(err) => {
        //         if err.is_data() {
        //             println!("Data Error");
        //         }
        //         if err.is_eof() {
        //             println!("EOF Error");
        //         }
        //         if err.is_io() {
        //             println!("IO Error");
        //         }
        //         if err.is_syntax() {
        //             println!("Json Syntax Error");
        //
        //         }
        //     }
        // }

        // if array of json objects
        let mut analyise_schema = AnalyseSchema { i: 0 };

        let mut line: Vec<Value> = SerderJson::json_decode(data);

        if !line.is_empty() {

            // line.unwrap().as_array().unwrap()[0].as_str().unwrap()) == "integer"
            // if line.len() > 1
            // {
            //     records = line
            // } else {
            //     records.push(line.first().unwrap().clone());
            // }

            for item in line {
                if item.is_string() {
                    message = serde_json::from_str(&item.as_str().unwrap()).unwrap();

                    messages.append(&mut message);
                } else {
                    messages.push(item);
                }
            }
        }

        messages
    }

    pub fn open_writer(&mut self, filename: String, schema: Vec<Value>) {
        self.fh = filename;
    }

    pub fn close_writer(&mut self) {
        for data in self.records.iter() {
            if self.compression_type == "VALUE_COMPRESSION" {
                // fputs($this->fh, gzcompress($data, -6));
            } else if self.compression_type == "NO_COMPRESSION" {
                // fputs($this->fh, $data);
            }
        }

        // fclose($this->fh);
    }

    pub fn serialize(&mut self, record: Vec<Value>, schema: Vec<Value>) {
        self.records.push(serde_json::to_string(&record).unwrap());
    }

    // The output is wrapped in a Result to allow matching on errors
// Returns an Iterator to the Reader of the lines of the file.
    pub fn read_lines<P>(filename: P) -> Result<Lines<BufReader<File>>>
        where P: AsRef<Path>, {
        let file = File::open(filename)?;
        Ok(BufReader::new(file).lines())
    }

    pub fn json_decode(string: String) -> Vec<Value> {
        // let mut message: Vec<Value> = serde_json::from_str(&string).unwrap();

        let mut message: Vec<Value> = vec![];

        // let line: Value = serde_json::from_str(&string).unwrap_or_default();
        let line: Value = match serde_json::from_str(&string) {
            Ok(message) => message,
            Err(err) => Null
        };

        if line.is_array() {
            let lines: Vec<Value> = serde_json::from_str(&string).unwrap();
            for data in lines.iter() {
                message.push(data.clone());
            }
        }

        // @todo - match error above (trigger from last test)
        else if !line.is_null() {
            message.push(line);
        } else {
            message = vec![];

            // mocking a stream is best way to deal with new line chars

            let mut file = File::create("/tmp/foo").unwrap();
            file.write_all(string.as_bytes());
            file.rewind();


            // let mut fp = String::from("php://temp");
            // fputs($fp, $string);
            // rewind($fp);

            let lines = SerderJson::read_lines("/tmp/foo");

            if lines.is_ok() {
                for line in lines.unwrap() {
                    if let Ok(mut string) = line {

                        // Basic clean up
                        // handle escaped json
                        string = string.replace("\\", "");
                        // and sometimes double escaped
                        string = string.replace("\\", "");

                        // handle python unicode strings
                        // @todo - better way?
                        string = string.replace("u'", "\"");

                        // handle invalid single quotes
                        string = string.replace("'", "\"");

                        // This will remove unwanted control characters.
                        for d in 0..=31 {
                            string = string.replace(char::from_u32(d).unwrap(), "");
                        }
                        string = string.replace(char::from_u32(127).unwrap(), "");

                        // Some file begins with 'efbbbf' to mark the beginning of the file. (binary level)
                        // here we detect it and we remove it, basically it's the first 3 characters
                        // see https://en.wikipedia.org/wiki/Byte_order_mark
                        if string.starts_with("efbbbf") {
                            string = string.replace("efbbbf", "");
                        }

                        // Eagerly and perhaps over zealously glob any json we can find by stripping any
                        // remaining non-json from beginning of source data strings.
                        let json_start = string.find("[\"");
                        if json_start.is_none() {
                            let json_start = string.find("{\"");
                        }

                        if json_start.is_some() {
                            string = string.replace(string.get(0..json_start.unwrap()).unwrap(), "");
                        }

                        message.push(serde_json::from_str(&string).unwrap_or_default());

                        if message.is_empty() || message.first().unwrap() == &Null {
                            message = vec![];

                            // Check for object concatinated into single line with no delemiter
                            // e.g. as AWS Kinesis Firehose does
                            let records: Vec<&str> = string.split("}{").collect();

                            let count = records.len();
                            let mut i = 1;

                            for record in records {
                                if i == 1 {
                                    let mut record = record.to_string();
                                    record.push('}');
                                    message.push(serde_json::from_str(&record).unwrap());
                                }

                                if i > 1 && i < count {
                                    let mut record = record.to_string();
                                    record.insert(0, '{');
                                    record.push('}');
                                    message.push(serde_json::from_str(&record).unwrap());
                                }

                                if i == count {
                                    let mut record = record.to_string();
                                    record.insert(0, '{');
                                    message.push(serde_json::from_str(&record).unwrap());
                                }

                                i += 1;
                            }
                        }
                    }
                }
            }

            // fclose($fp);
        }

        message
    }
}

#[test]
fn test_basic_valid_json() {
    let record: String = r#"{"status": "200"}"#.to_string();
    let msg = SerderJson::deserialize(record);
    assert_eq!(msg.first().unwrap()["status"], "200");
}

#[test]
fn test_nested_valid_json() {
    let record: String = r#"{"status": "200", "items": {"foo": "bar"}}"#.to_string();
    let msg = SerderJson::deserialize(record);
    assert_eq!(msg.first().unwrap()["status"], "200");
    assert_eq!(msg.first().unwrap()["items"]["foo"], "bar");
}

#[test]
fn test_nested_array_valid_json() {
    let record: String = r#"{"status": "200", "items": [{"foo": "bar"}]}"#.to_string();
    let msg = SerderJson::deserialize(record);
    assert_eq!(msg.first().unwrap()["status"], "200");
    assert_eq!(msg.first().unwrap()["items"][0]["foo"], "bar");
}

#[test]
fn test_escaped_json() {
    let record: String = r#"{\"status\": \"200\"}"#.to_string();
    let msg = SerderJson::deserialize(record);
    assert_eq!( msg.first().unwrap()["status"], "200");
}

#[test]
fn test_double_escaped_json() {
    let record: String = r#"{\\\"time\\\":{\\\"start_time\\\":\\\"273.046328210292\\\",\\\"end_time\\\":\\\"16182\\\"},\\\"bike_id\\\":\\\"0.579087190592872\\\",\\\"location\\\":{\\\"start\\\":\\\"0.620131100002421\\\",\\\"end\\\":null}}"#.to_string();
    let msg = SerderJson::deserialize(record);
    assert_eq!( msg.first().unwrap()["bike_id"], "0.579087190592872");
    assert_eq!( msg.first().unwrap()["time"]["start_time"], "273.046328210292");
}

#[test]
fn test_null_value_valid_json() {
    let record: String = r#"{"start":"0.620131100002421","end":null}"#.to_string();
    let msg = SerderJson::deserialize(record);
    assert_eq!( msg.first().unwrap()["start"], "0.620131100002421");
    assert_eq!( msg.first().unwrap()["end"], Null);
}

#[test]
fn test_signle_quote_strings_json() {
    let record: String = r#"{'status': '200'}"#.to_string();
    let msg = SerderJson::deserialize(record);
    assert_eq!( msg.first().unwrap()["status"], "200");
}

#[test]
fn test_string_before_escaped_json() {
    let record: String = r#"some, string, that exists)/ 20080808115538 {\"status\":\"200\",\"length\":\"4742\",\"mime\":\"text/html\",\"offset\":\"16518203\"}"#.to_string();
    let msg = SerderJson::deserialize(record);
    assert_eq!( msg.first().unwrap()["status"], "200");
    // assert!(msg.first().unwrap().get("some, string").is_none());
}

#[test]
fn test_unicode_string_json() {
    let record: String = r#"{u'status': u'200'}"#.to_string();
    let msg = SerderJson::deserialize(record);
    assert_eq!( msg.first().unwrap()["status"], "200");
}

#[test]
fn test_unicode_string_value_json() {
    let record: String = r#"{"status": "\u0023"}"#.to_string();
    let msg = SerderJson::deserialize(record);
    assert_eq!( msg.first().unwrap()["status"], "#");
    assert_ne!( msg.first().unwrap()["status"], 2605);
}

#[test]
fn test_multi_record_array_json() {
    let record: String = r#"[{"status": "200"},{"status": "500"}]"#.to_string();
    let msg = SerderJson::deserialize(record);
    assert_eq!( msg.first().unwrap()["status"], "200");
    assert_eq!( msg.last().unwrap()["status"], "500");
}

#[test]
fn test_valid_multi_line_json() {
    let record: String = "{\"status\": \"200\"}\n{\"status\": \"500\"}".to_string();
    let msg = SerderJson::deserialize(record);
    assert_eq!( msg.first().unwrap()["status"], "200");
    assert_eq!( msg.last().unwrap()["status"], "500");
}

#[test]
fn test_valid_multi_line_with_multi_record_arrays_json() {
    let record: String = "[{\"status\": \"200\"},{\"status\": \"201\"}]\n[{\"status\": \"202\"},{\"status\": \"203\"}]".to_string();
    let msg = SerderJson::deserialize(record);
    assert_eq!( msg.first().unwrap()[0]["status"], "200");
    assert_eq!( msg.first().unwrap()[1]["status"], "201");
    assert_eq!( msg.last().unwrap()[0]["status"], "202");
    assert_eq!( msg.last().unwrap()[1]["status"], "203");
}

/**
 * This madness is common, for instance AWS Firehose S3 Destination produces this crap
 */
#[test]
fn test_single_line_objects_json() {
    let record: String = r#"{"foo": {"nest": "bar"}}{"foo": {"nest": "baz"}}{"foo": {"nest": "boo"}}"#.to_string();
    let msg = SerderJson::deserialize(record);
    // assert!( msg.first().unwrap().is_array());
    assert_eq!(msg[0]["foo"]["nest"], "bar");
    assert_eq!(msg[1]["foo"]["nest"], "baz");
    assert_eq!(msg[2]["foo"]["nest"], "boo");
}