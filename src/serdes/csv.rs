extern crate csv;

use std::collections::HashMap;
use std::io::{self, Cursor, BufReader, BufRead};
use std::process::exit;
use std::string::ToString;
use once_cell::sync::Lazy;
use std::sync::{Mutex, RwLock};
use serde_json::Value;
use crate::helpers::timed_rwlock::TimedRwLock;

pub static CSV_HEADERS: Lazy<TimedRwLock<Vec<String>>> = Lazy::new(|| TimedRwLock::new("csv_headers".to_string(), Vec::new()));
pub static CHOSEN_DELIM: Lazy<TimedRwLock<String>> = Lazy::new(|| TimedRwLock::new("chosen_delim".to_string(), ",".to_string()));

pub struct SerderCsv;

impl SerderCsv {
    pub fn new() -> Self {
        SerderCsv
    }

    pub fn deserialize(record: &str) -> Vec<Value> {
        let record = record.replace("\r\n", "\n");

        let delimiter_byte;
        {
            let delim_guard = CHOSEN_DELIM.read().unwrap();
            if delim_guard.is_empty() {
                drop(delim_guard);
                let chosen_delim = Self::detect_delimiter(&record);
                let mut delim_write_guard = CHOSEN_DELIM.write().unwrap();
                *delim_write_guard = chosen_delim;
                delimiter_byte = delim_write_guard.as_bytes()[0];
            } else {
                delimiter_byte = delim_guard.as_bytes()[0];
            }
        }

        let mut rdr = csv::ReaderBuilder::new()
            .delimiter(delimiter_byte)
            .has_headers(false)
            .flexible(true)
            .from_reader(record.as_bytes());

        let mut messages: Vec<Value> = Vec::new();
        for result in rdr.records() {
            let record = match result {
                Ok(r) => r,
                Err(e) => {
                    println!("Error reading CSV record: {}", e);
                    continue;
                }
            };

            let mut is_header_row = false;

            let headers = {
                let header_guard = CSV_HEADERS.read().unwrap();
                if header_guard.is_empty() {
                    drop(header_guard);
                    is_header_row = record.iter().all(|item| item.parse::<i64>().is_err());
                    if is_header_row {
                        let headers_vec: Vec<String> = record.iter().map(|s| s.to_string()).collect();
                        let mut write_guard = CSV_HEADERS.write().unwrap();
                        write_guard.extend(headers_vec.clone());
                        headers_vec
                    } else {
                        vec![]
                    }
                } else {
                    header_guard.clone()
                }
            };

            let mut obj = serde_json::Map::new();
            if !is_header_row {
                if !headers.is_empty() {
                    for (key, value) in headers.iter().zip(record.iter()) {
                        obj.insert(key.clone(), Value::String(value.to_string()));
                    }
                } else {
                    for (idx, value) in record.iter().enumerate() {
                        obj.insert(idx.to_string(), Value::String(value.to_string()));
                    }
                }
                messages.push(Value::Object(obj));
            }
        }

        messages
    }

    fn detect_delimiter(record: &str) -> String {
        let delimiters = vec![";", ",", "\t", "|"];
        let mut chosen_delim = ",".to_string();  // Default to comma.
        let mut max_fields = 0;

        for delim in &delimiters {
            let lines: Vec<&str> = record.split('\n').collect();
            let first_line_fields = lines[0].split(*delim).count();

            println!("Delimiter: {} ({} fields)", delim, first_line_fields);

            if
            // lines.iter().all(|line| line.split(*delim).count() == first_line_fields) &&
                first_line_fields > max_fields {
                max_fields = first_line_fields;
                chosen_delim = delim.to_string();
            }
        }

        println!("Delimiter: {} ({} fields)", chosen_delim, max_fields);


        chosen_delim
    }

}

#[cfg(test)]
mod tests_csv {
    use super::*;
    use serde_json::json;
    use serial_test::serial;

    fn reset_globals() {
        let mut delim_guard = CHOSEN_DELIM.write().unwrap();
        *delim_guard = "".to_string();

        let mut headers_guard = CSV_HEADERS.write().unwrap();
        headers_guard.clear();
    }

    #[test]
    #[serial]
    fn test_comma_delimiter() {
        reset_globals();

        let input = "name,age\nJohn,30\nDoe,25\n";
        let output = SerderCsv::deserialize(input);

        assert_eq!(CHOSEN_DELIM.read().unwrap().to_string(), ",".to_string());

        let expected = vec![
            json!({"name": "John", "age": "30"}),
            json!({"name": "Doe", "age": "25"}),
        ];
        assert_eq!(output, expected);
    }

    #[test]
    #[serial]
    fn test_semicolon_delimiter() {
        reset_globals();

        let input = "name;age\nJane;20";
        let output = SerderCsv::deserialize(input);

        assert_eq!(CHOSEN_DELIM.read().unwrap().to_string(), ";".to_string());

        let expected = vec![json!({"name": "Jane", "age": "20"})];
        assert_eq!(output, expected);
    }

    #[test]
    #[serial]
    fn test_no_headers() {
        reset_globals();

        let input = "Alice,23\nBob,28";
        let output = SerderCsv::deserialize(input);
        // Since there are no headers, fields are indexed numerically.

        assert_eq!(CHOSEN_DELIM.read().unwrap().to_string(), ",".to_string());

        let expected = vec![
            json!({"0": "Alice", "1": "23"}),
            json!({"0": "Bob", "1": "28"}),
        ];
        assert_eq!(output, expected);
    }

    #[test]
    #[serial]
    fn test_inconsistent_field_count() {
        reset_globals();

        let input = "name,age\nJohn,30\nDoe";
        let output = SerderCsv::deserialize(input);
        // The second record (Doe) should be ignored as it has fewer fields than expected.

        assert_eq!(CHOSEN_DELIM.read().unwrap().to_string(), ",".to_string());

        let expected = vec![json!({"name": "John", "age": "30"})];
        assert_eq!(output, expected);
    }

    #[test]
    #[serial]
    fn test_tab_delimiter() {
        reset_globals();

        let input = "name\tage\nJohn\t30\nDoe\t25";
        let output = SerderCsv::deserialize(input);
        let expected = vec![
            json!({"name": "John", "age": "30"}),
            json!({"name": "Doe", "age": "25"}),
        ];
        assert_eq!(output, expected);
    }

    // ... Add more tests as needed.
}

