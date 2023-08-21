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

        // Reset static values for testing purposes
        // *CHOSEN_DELIM.lock().unwrap() = "".to_string();
        // *CSV_HEADERS.lock().unwrap() = Vec::new();

        let record = record.replace("\r\n", "\n")
            .replace("\n\r", "\n")
            .replace("\r", "\n");

        {
            let chosen_delim_guard = CHOSEN_DELIM.read().unwrap();

            if chosen_delim_guard.is_empty() {
                drop(chosen_delim_guard);
                // Mock a stream with Cursor to deal with newline chars.
                let cursor = Cursor::new(record.clone());
                let mut buf_reader = BufReader::new(cursor);

                // Determine delimiter by reading the first line.
                let mut first_line = String::new();
                match buf_reader.read_line(&mut first_line) {
                    Ok(_) => (),
                    Err(e) => println!("Error reading first line of CSV: {}", e),
                }

                let delimiters = vec![";", ",", "\t", "|"];
                let mut max_delim_count = 0;
                let mut chosen_delim = ",".to_string();  // Default to comma.

                for delim in &delimiters {
                    let count = first_line.split(*delim).count();
                    if count > max_delim_count {
                        max_delim_count = count;
                        chosen_delim = delim.to_string();
                    }
                }

                CHOSEN_DELIM.write().unwrap().push_str(&chosen_delim);
            }
        }

        // println!("Using delimiter: {}", CHOSEN_DELIM.read().unwrap().as_bytes()[0].to_string());

        // println!("Record: {}", record);
        // exit(0);

        let delimiter_byte = CHOSEN_DELIM.read().unwrap().as_bytes()[0];

        // @todo - don't cut off the header row
        let mut rdr = csv::ReaderBuilder::new()
            .delimiter(delimiter_byte)
            .has_headers(false)
            .flexible(true)
            .from_reader(Cursor::new(&record));

        let mut messages: Vec<Value> = Vec::new();

        for result in rdr.records() {
            let record = match result {
                Ok(record) => {
                    // println!("Record: {:?}", record);
                    // exit(0);
                    record
                },
                Err(e) => {
                    println!("Error reading CSV record: {}", e);
                    continue;
                }
            };
            let mut field_count = 0;
            let mut is_header_row = false;

            {

                if CSV_HEADERS.read().unwrap().is_empty() {

                    // hold write lock till we know if this is a header row
                    // in order to prevent ingestion of any other threads batches
                    let mut write_guard = CSV_HEADERS.write().unwrap();

                    is_header_row = record.iter().all(|item| match item.parse::<i64>().is_err() {
                        true => true,
                        false => {
                            // println!("Header row contains numeric value: {}", item);
                            false
                        }
                    });

                    if is_header_row {
                        // println!("Headers record: {:?}", record);
                        let headers: Vec<String> = record.iter().map(|s| s.to_string()).collect();
                        // println!("Headers set: {:?}", record);
                        {
                            // CSV_HEADERS.write().unwrap().extend(headers);
                            write_guard.extend(headers);
                        }
                        drop(write_guard);
                        // println!("New Headers: {:?}", CSV_HEADERS.read().unwrap());

                    }
                }
            }


            field_count = record.len().max(field_count);

            if !is_header_row && field_count > 1 {
                let headers = CSV_HEADERS.read().unwrap();
                let mut obj = serde_json::Map::new();

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


        // let json_messages: Vec<Value> = messages.into_iter()
        //     .map(|msg| serde_json::to_value(msg).expect("Failed to convert HashMap to JSON Value"))
        //     .collect();


        messages
    }
}

#[cfg(test)]
mod tests_csv {
    use super::*;
    use serde_json::json;
    use serial_test::serial;

    #[test]
    #[serial]
    fn test_comma_delimiter() {
        let input = "name,age\nJohn,30\nDoe,25\n";
        let output = SerderCsv::deserialize(input);
        let expected = vec![
            json!({"name": "John", "age": "30"}),
            json!({"name": "Doe", "age": "25"}),
        ];
        assert_eq!(output, expected);
    }

    #[test]
    #[serial]
    fn test_semicolon_delimiter() {
        let input = "name;age\nJane;20";
        let output = SerderCsv::deserialize(input);
        let expected = vec![json!({"name": "Jane", "age": "20"})];
        assert_eq!(output, expected);
    }

    #[test]
    #[serial]
    fn test_no_headers() {
        let input = "Alice,23\nBob,28";
        let output = SerderCsv::deserialize(input);
        // Since there are no headers, fields are indexed numerically.
        let expected = vec![
            json!({"0": "Alice", "1": "23"}),
            json!({"0": "Bob", "1": "28"}),
        ];
        assert_eq!(output, expected);
    }

    #[test]
    #[serial]
    fn test_inconsistent_field_count() {
        let input = "name,age\nJohn,30\nDoe";
        let output = SerderCsv::deserialize(input);
        // The second record (Doe) should be ignored as it has fewer fields than expected.
        let expected = vec![json!({"name": "John", "age": "30"})];
        assert_eq!(output, expected);
    }

    #[test]
    #[serial]
    fn test_tab_delimiter() {
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

