use chrono::{DateTime, FixedOffset, NaiveDateTime, TimeZone, Utc};
use memory_stats::memory_stats;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::{env, fs};

use std::str;

use rand::seq::SliceRandom;
use rand::Rng;
use serde_json::{Map, Value};
use std::error::Error;
use std::path::{Path, PathBuf};

pub mod configuration;
pub mod license;
pub mod logger;
pub mod offsets;
pub mod timed_rwlock;

// let CLEAN_FIELD_CACHE = Arc::new(Mutex::new(HashMap<String, bool> = HashMap::new()));

use crate::discover::date_formats::DateFormats;
use crate::discover::Metadata;
use crate::helpers::configuration::Config;
use once_cell::sync::Lazy;
use std::sync::{Arc};
use dashmap::{DashMap, DashSet};
use crate::helpers::timed_rwlock::TimedRwLock;
use walkdir::WalkDir;

// static CLEAN_FIELD_CACHE: Lazy<Mutex<i64>> = Lazy::new(|| Mutex::new(1));
// static CLEAN_FIELD_CACHE: Lazy<TimedRwLock<DashMap<String, String>>> =
//     Lazy::new(|| TimedRwLock::new("clean_field_cache".to_string(), DashMap::new()));
static CLEAN_FIELD_CACHE: Lazy<Arc<DashMap<String, String>>> = Lazy::new(|| Arc::new(DashMap::new()));

pub struct Helpers {}

impl Helpers {
    // let CLEAN_FIELD_CACHE: HashMap<String, bool> = HashMap::new();
    // pub(crate) CLEAN_FIELD_CACHE: HashMap<String, bool> = HashMap::new();

    // pub fn explode_field(field: &str) -> Vec<&str> {
    //     let mut field_haystack: Vec<&str> = Vec::new();
    //
    //     let string = field.replace(".", "_");
    //     let string = string.to_lowercase();
    //
    //     // let string = string.replace("_", " ");
    //     // let string = string.replace("-", " ");
    //     // let string = string.replace(" ", "_");
    //
    //     field_haystack = string.split("_").collect();
    //
    //     field_haystack
    // }

    // pub fn is_sequential_array_keys(arr: &[i32]) -> bool {
    pub fn is_sequential_array_keys(arr: &Vec<Value>) -> bool {
        let arr = arr.to_vec();
        // arr.sort();
        // if arr.first() != Some(&0) && arr.is_empty() {
        //     return false;
        // }

        // for (k, v) in arr.iter().enumerate() {
        //     if k.parse::<i32>().is_ok() {
        //
        //     }
        // }

        arr.iter().enumerate().all(|(i, v)| &arr[i] == v)
        // arr.iter().enumerate().all(|(i, v)| i as i32 == v)
    }

    pub fn clean_field_name<'a>(field: String) -> String {
        {
            if let Some(cached) = CLEAN_FIELD_CACHE.get(&field) {
                if cached.value() != "no" {
                    return cached.value().to_string();
                }
            }
        }

        let mut clean = field.clone();
        if field.parse::<i32>().is_ok() {
            clean = "item_".to_string() + &field;
        }

        clean = clean.to_lowercase();

        let re =
            Regex::new(r"[^_0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ]")
                .unwrap();
        clean = re.replace_all(&clean, "_").to_string();
        clean = regex::Regex::new(r"_+")
            .unwrap()
            .replace_all(&clean, "_")
            .to_string();

        // let pattern = "/[^" + preg_quote(
        //     "_0123456789_abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ",
        //     "/"
        // ) + "]/";
        // clean = preg_replace(pattern, "_", field);

        let x: &[_] = &['1', '2', '3', '4', '5', '6', '7', '8', '9'];
        clean = clean.trim_start_matches(x).to_string();
        // clean = ltrim(clean, "0123456789");

        // '_' at the beginning is common and probably allowable
        clean = clean.trim_matches('_').to_string();
        // clean = trim(clean, '_');

        {
            let mut cache = CLEAN_FIELD_CACHE.entry(field.clone()).or_insert_with(|| "no".to_string());
            if clean != field {
                *cache = clean.clone();
            }
        }

        clean
    }

    // pub fn clean_array_field_names<T>(mut self, mut array: HashMap<String, T>) {
    //     for (field, value) in array.iter() {
    //         let field = self.clean_field_name(field);
    //         if TypeId::of::<T>() == TypeId::of::<HashMap<String, T>>() {
    //             self.clean_array_field_names(value);
    //         }
    //         array.insert(field, value);
    //     }
    // }

    pub fn random_password(length: usize) -> String {
        let mut rng = rand::thread_rng();
        let alphabet = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890";
        let mut pass = String::new();
        let alpha_length = alphabet.len() - 1;
        for _ in 0..length {
            let n = rng.gen_range(0..alpha_length);
            pass.push(alphabet.chars().nth(n).unwrap());
        }
        pass
    }

    pub fn random_str(length: usize) -> String {
        let mut rng = rand::thread_rng();
        let alphabet: Vec<char> = "abcdefghijklmnopqrstuvwxyz".chars().collect();
        let mut pass = String::with_capacity(length);

        for _ in 0..length {
            let c = alphabet.choose(&mut rng).unwrap();
            pass.push(*c);
        }

        // add timestamp to the end of the string
        let timestamp = Utc::now().timestamp_millis();
        pass.push_str(&timestamp.to_string());
        pass
    }

    fn flatten_internal(field: &str, json: &Value, result: &mut Map<String, Value>, metadata: &Metadata) {
        match json {
            Value::Object(map) => {
                if map.is_empty() {
                    result.insert(metadata.out_field_name.clone(), Value::Null);
                } else {
                    for (key, value) in map {
                        let new_key = format!("{}_{}", field, key);
                        if let Some(child_metadata) = metadata.fields.get(key) {
                            Helpers::flatten_internal(&new_key, value, result, child_metadata);
                        } else {
                            Helpers::flatten_internal(&new_key, value, result, metadata);
                        }
                    }
                }
            }
            Value::Array(arr) => {
                if arr.is_empty() {
                    result.insert(metadata.out_field_name.clone(), Value::Array(vec![]));
                } else {
                    for (index, value) in arr.iter().enumerate() {
                        // Ensure field name is correctly indexed
                        let indexed_field_name = format!("{}_{}", metadata.out_field_name, index);
                        Helpers::flatten_internal(&indexed_field_name, value, result, metadata);
                    }
                }
            }
            _ => {
                // Ensure correct naming for flattened fields
                let field_name = field.to_string(); // Preserve full key path
                result.insert(field_name, json.clone());
            }
        }
    }



    // deprecated - we now use the metadata to determine the field names
    pub fn flatten(json: &Value, _metadata: &HashMap<String, Metadata>) -> Result<Value, Box<dyn Error>> {
        let mut result = Map::new();
        for (key, value) in json.as_object().ok_or(format!("Invalid JSON object: {}", json))? {
            Helpers::flatten_internal(key, value, &mut result, _metadata.get(key).unwrap());
        }
        // Helpers::flatten_internal(json, &mut result, metadata);
        Ok(Value::Object(result))
    }

    pub fn mem_limit_reached() -> bool {
        let mem_limit = env::var("MEM_LIMIT")
            .unwrap_or("0".to_string())
            .parse::<u32>()
            .unwrap_or(0);
        let mut mem_usage = 0;
        if let Some(usage) = memory_stats() {
            mem_usage = usage.physical_mem as u32;
        }

        if mem_usage >= mem_limit {
            return true;
        }

        false
    }

    pub fn parse_partition_field(message: &Value, clean_allowed_values: HashSet<String>) -> String {
        let mut clean_partition: String = "".to_string();

        // optional: partition by composite key
        if !Config::get_transform_batch_partition_fields().is_empty() {
            let mut partitions = vec![];

            for entity_field_dot in
                Config::get_transform_batch_partition_fields().split(',')
            {
                // strip whitespace
                let entity_field_dot = entity_field_dot.trim();

                let mut clean_entity_value =
                    match Helpers::get_nested_value_from_dot_notation(message, entity_field_dot) {
                        Some(entity_value) => {
                            Helpers::clean_field_name(match entity_value.as_str() {
                                Some(val) => val.to_string(),
                                None => "".to_string(),
                            })
                            // let entity_name = match entity_field_dot.rfind('.') {
                            //     Some(index) => &entity_field_dot[index + 1..],
                            //     None => entity_field_dot,
                            // };
                            // let clean_entity_name = Helpers::clean_field_name(entity_name.to_string());
                            // partitions.push(format!("{}={}", clean_entity_name, clean_entity_value));
                        }
                        None => "".to_string(),
                    };

                // only partition by allowed values, is set
                if 
                // clean_entity_value != "" && // explicitly allow empty values to pass through
                    clean_allowed_values.len() > 0 &&
                    !clean_allowed_values.contains(&clean_entity_value)
                {
                    clean_entity_value = "".to_string();
                }

                let entity_name = match entity_field_dot.rfind('.') {
                    Some(index) => format!("p_{}", &entity_field_dot[index + 1..]),
                    None => format!("p_{}", entity_field_dot),
                };
                let clean_entity_name = Helpers::clean_field_name(entity_name.to_string());
                partitions.push(format!("{}={}", clean_entity_name, clean_entity_value));
            }

            clean_partition = partitions.join("/");
            clean_partition = clean_partition.trim_matches('-').to_lowercase();
            // clean_partition = Helpers::clean_field_name(clean_partition);
        }

        clean_partition
    }

    pub fn parse_namespace_field(
        message: &Value,
        namespace: String,
        parse_namespace_cache: &mut HashMap<String, String>,
    ) -> String {
        let mut clean_namespace = namespace.clone();

        if !parse_namespace_cache.contains_key(&namespace)
            || parse_namespace_cache.get(&namespace).unwrap() == "yes"
        {
            // default to data source partition (table, topic, queue, file dir, etc)
            clean_namespace = Helpers::clean_field_name(clean_namespace);

            // optional: partition by composite key
            if !Config::get_transform_namespace_fields().is_empty() {
                let mut namespaces = vec!["".to_string()];

                for entity_field_dot in Config::get_transform_namespace_fields().split(',')
                {
                    match Helpers::get_nested_value_from_dot_notation(message, entity_field_dot) {
                        Some(entity_value) => {
                            namespaces.push(entity_value.as_str().unwrap().to_string());
                        }
                        None => (),
                    }
                }

                let join = namespaces.join("_").trim_matches('_').to_lowercase();

                if join != "" {
                    clean_namespace = Helpers::clean_field_name(join);
                }
            }
        }

        if clean_namespace != namespace {
            parse_namespace_cache.insert(namespace, "yes".to_string());
        } else {
            parse_namespace_cache.insert(namespace, "no".to_string());
        }

        clean_namespace
    }

    fn is_millisecond_timestamp(time: i64) -> bool {
        let num_digits = ((time as f64).log10() + 1.0).floor() as i32;
        num_digits > 10
    }
    pub fn parse_time_field(message: &Value) -> Option<i64> {
        // Only process if time fields are configured
        if Config::get_transform_batch_time_fields().is_empty() {
            return None;
        }

        // Support nested time fields via array dot notation
        // For user confirmed event time fields, use the first one that matches
        for field_dot in Config::get_transform_batch_time_fields().split(',') {
            if let Some(value) = Helpers::get_nested_value_from_dot_notation(message, field_dot) {
                match value {
                    // Handle string timestamps
                    Value::String(ref s) => {
                        // Try parsing as RFC3339/ISO8601 first
                        if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
                            return Some(dt.timestamp());
                        }
                        
                        // Try parsing as millisecond timestamp string
                        if let Ok(ms) = s.parse::<i64>() {
                            // Validate millisecond timestamp range
                            if ms > 999999999999999 || ms < -999999999999999 {
                                continue; // Invalid millisecond range, try next field
                            }
                            return Some(ms / 1000); // Convert ms to seconds
                        }
                        
                        // Try various date formats using parse_date_from_string
                        for format in DateFormats::iterator() {
                            if let Ok(dt) = Helpers::parse_date_from_string(s, format.as_str()) {
                                return Some(dt.timestamp());
                            }
                        }
                    },
                    
                    // Handle numeric timestamps
                    Value::Number(n) => {
                        if let Some(ts) = n.as_i64() {
                            // Validate timestamp range
                            if ts > 999999999999999 || ts < -999999999999999 {
                                continue; // Invalid range, try next field
                            }
                            // Assume milliseconds if timestamp is too large for seconds
                            if Helpers::is_millisecond_timestamp(ts) {
                                return Some(ts / 1000);
                            }
                            return Some(ts);
                        }
                    },
                    _ => continue,
                }
            }
        }

        None
    }

    pub fn parse_date_from_string(date_str: &str, format: &str) -> Result<DateTime<Utc>, String> {

        return match DateTime::parse_from_str(date_str, format) {
            Ok(date) => {
                Ok(DateTime::<Utc>::from(date))
            },
            Err(_) => {
                match NaiveDateTime::parse_from_str(date_str, format) {
                    Ok(date) => Ok(DateTime::<Utc>::from_naive_utc_and_offset(date, Utc)),
                    Err(_) => {
                        Err(format!("Could not parse date {} with format {}", date_str, format))
                    }
                }
            }
        };

    }

    pub fn get_nested_value_from_dot_notation(
        json_value: &Value,
        field_str: &str,
    ) -> Option<Value> {
        // Parse the JSON string into a serde_json Value object
        // let json_value: Value = serde_json::from_str(json_str).ok()?;

        // Split the dot notation string into individual field names
        let fields: Vec<&str> = field_str.split('.').collect();

        // Traverse the JSON object, following each field name in turn
        let mut current_value: &Value = json_value;
        for field in fields {
            if let Value::Object(map) = current_value {
                if let Some(next_value) = map.get(field) {
                    current_value = next_value;
                } else {
                    return None;
                }
            } else {
                return None;
            }
        }

        // Return the final value found at the end of the traversal
        Some(current_value.clone())
    }

    pub fn process_values(values: &Vec<Value>, field_str: &str) -> Option<Vec<Value>> {
        values.iter().map(|value| {
            let current_value = Helpers::get_nested_value_from_dot_notation(value, field_str);
            match current_value {
                // Some(Value::Array(_)) => current_value.clone(),
                // Some(Value::Object(_)) => current_value.clone(),
                _ => current_value.clone(),
            }
        }).collect()
    }

    pub fn get_nested_metadata_with_flat_name<'a>(metadata: &'a mut Metadata, field_str: &str) -> Option<&'a mut Metadata> {
        // Check if the current metadata's out_field_name matches the field_str
        if metadata.out_field_name == field_str {
            return Some(metadata);
        }

        // Recursively search in nested fields
        for (_, nested_metadata) in metadata.fields.iter_mut() {
            if let Some(found_metadata) = Helpers::get_nested_metadata_with_flat_name(nested_metadata, field_str) {
                return Some(found_metadata);
            }
        }

        // If no matching Metadata is found in this branch, return None
        None
    }

    fn list_dir_recursively_with_size(start_dir: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
        // Use a HashMap to keep track of directory sizes
        let mut dir_sizes: std::collections::HashMap<PathBuf, u64> = std::collections::HashMap::new();

        // Walk through the directory recursively
        for entry in WalkDir::new(start_dir)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if path.is_file() {
                // Get file metadata
                let metadata = fs::metadata(path)?;
                let file_size = metadata.len();

                // Add file size to its parent directory total
                let parent_dir = path.parent().unwrap_or_else(|| Path::new("/"));
                *dir_sizes.entry(PathBuf::from(parent_dir)).or_insert(0) += file_size;
            }
        }

        // To mimic `du -h`, sort directories by their path
        let mut sorted_dirs: Vec<_> = dir_sizes.iter().collect();
        sorted_dirs.sort_by_key(|&(dir, _)| dir);

        // Print sizes in a human-readable format
        for (dir, size) in sorted_dirs {
            println!("{}:\t{}", dir.display(), Helpers::human_readable_size(*size));
        }

        Ok(())
    }

    pub fn human_readable_size(bytes: u64) -> String {
        let units = ["B", "KB", "MB", "GB", "TB", "PB", "EB"];
        let mut size = bytes as f64;
        let mut unit = 0;

        while size >= 1024.0 && unit < units.len() - 1 {
            size /= 1024.0;
            unit += 1;
        }

        format!("{:.1} {}", size, units[unit])
    }

}

#[cfg(test)]
mod date_timezones {
    use chrono::{DateTime, Utc};
    use crate::helpers::Helpers;
    use crate::discover::AnalyseSchema;
    use chrono::TimeZone;

    #[test]
    fn it_parses_datetime_with_positive_offset_to_utc() {
        let datetime_str = "2022-02-22T22:22:22+01:00";
        let format = "%Y-%m-%dT%H:%M:%S%:z";
        let expected = Utc.ymd(2022, 2, 22).and_hms(21, 22, 22); // Adjusted to UTC
        let result = Helpers::parse_date_from_string(datetime_str, format).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn it_parses_datetime_with_negative_offset_to_utc() {
        let datetime_str = "2022-02-22T22:22:22-01:00";
        let format = "%Y-%m-%dT%H:%M:%S%:z";
        let expected = Utc.ymd(2022, 2, 22).and_hms(23, 22, 22); // Adjusted to UTC
        let result = Helpers::parse_date_from_string(datetime_str, format).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn it_handles_incorrect_format_gracefully() {
        let datetime_str = "2022-02-22 22:22:22";
        let format = "%Y-%m-%dT%H:%M:%S%:z"; // Incorrect format for the input
        let result = Helpers::parse_date_from_string(datetime_str, format);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Could not parse date 2022-02-22 22:22:22 with format %Y-%m-%dT%H:%M:%S%:z");
    }

    #[test]
    fn it_parses_naive_datetime_to_utc() {
        let datetime_str = "2022-02-22T22:22:22";
        let format = "%Y-%m-%dT%H:%M:%S"; // No timezone information
        let expected = Utc.ymd(2022, 2, 22).and_hms(22, 22, 22); // Assumed to already be in UTC
        let result = Helpers::parse_date_from_string(datetime_str, format).unwrap();
        assert_eq!(result, expected);
    }
}

#[cfg(test)]
mod tests_get_nested_value_from_dot_notation {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_get_nested_value_from_dot_notation() {
        let data = json!({
            "name": "Alice",
            "info": {
                "age": 30,
                "address": {
                    "city": "Wonderland"
                }
            }
        });

        // Test case: Value exists
        let result = Helpers::get_nested_value_from_dot_notation(&data, "info.address.city");
        assert_eq!(result, Some(json!("Wonderland")));

        // Test case: Value does not exist
        let result = Helpers::get_nested_value_from_dot_notation(&data, "info.address.country");
        assert_eq!(result, None);

        // Test case: Final value is an object
        let result = Helpers::get_nested_value_from_dot_notation(&data, "info.address");
        assert_eq!(result, Some(json!({"city": "Wonderland"})));

        // Test case: Final value is an array
        let data = json!({
            "array_field": [{"a": 1}, {"b": 2}]
        });
        let result = Helpers::get_nested_value_from_dot_notation(&data, "array_field");
        assert_eq!(result, Some(json!([{"a": 1}, {"b": 2}])));

        // Test case: Traversal of non-object field
        let result = Helpers::get_nested_value_from_dot_notation(&data, "name.city");
        assert_eq!(result, None);
    }
}


#[cfg(test)]
mod clean_field_name_tests {
    use super::*;

    #[test]
    fn test_clean_field_name() {
        // Cache is empty, alphanumeric input
        {
            CLEAN_FIELD_CACHE.clear();
        }
        assert_eq!(
            Helpers::clean_field_name("testField".to_string()),
            "testfield".to_string()
        );

        // Cache is empty, input with special characters
        {
            CLEAN_FIELD_CACHE.clear();
        }
        assert_eq!(
            Helpers::clean_field_name("test!@#Field$%^&".to_string()),
            "test_field".to_string()
        );

        // Cache is empty, input starts with numbers
        {
            CLEAN_FIELD_CACHE.clear();
        }
        assert_eq!(
            Helpers::clean_field_name("123testField".to_string()),
            "testfield".to_string()
        );

        // Cache is empty, input starts with underscore and numbers
        {
            CLEAN_FIELD_CACHE.clear();
        }
        assert_eq!(
            Helpers::clean_field_name("_123testField".to_string()),
            "123testfield".to_string()
        );

        // Cache is empty, input is numbers
        {
            CLEAN_FIELD_CACHE.clear();
        }
        assert_eq!(
            Helpers::clean_field_name("1".to_string()),
            "item_1".to_string()
        );

        // Cache has a record
        {
            CLEAN_FIELD_CACHE.clear();
            CLEAN_FIELD_CACHE.insert("cachedField".to_string(), "cachedfield".to_string());
        }
        assert_eq!(
            Helpers::clean_field_name("cachedField".to_string()),
            "cachedfield".to_string()
        );

        // Cache has a record marked as "no"
        {
            CLEAN_FIELD_CACHE.clear();
            CLEAN_FIELD_CACHE.insert("no_change_field".to_string(), "no".to_string());
        }
        assert_eq!(
            Helpers::clean_field_name("no_change_field".to_string()),
            "no_change_field".to_string()
        );
    }
}

#[cfg(test)]
mod parse_time_field_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_time_field_with_invalid_millisecond_timestamp() {
        // Set up the environment
        Config::setenv("TRANSFORM_BATCH_TIME_FIELDS", "time2");

        // Create a message with the timestamp in a field matching TRANSFORM_BATCH_TIME_FIELDS
        let message = json!({
            "time2": 999999999999999999 as i64 // Invalid millisecond timestamp (too large)
        });

        assert_eq!(Helpers::parse_time_field(&message), None);
    }

    #[test]
    fn test_parse_time_field_with_valid_millisecond_timestamp() {
        Config::setenv("TRANSFORM_BATCH_TIME_FIELDS", "time1");
        
        let time = 1646901960000i64; // Valid millisecond timestamp
        let message = json!({ "time1": time });

        assert_eq!(Helpers::parse_time_field(&message), Some(1646901960));
    }

    #[test]
    fn test_parse_time_field_with_valid_second_timestamp() {
        Config::setenv("TRANSFORM_BATCH_TIME_FIELDS", "time3");

        let time = 1646901960i64; // Valid second timestamp
        let message = json!({ "time3": time });

        assert_eq!(Helpers::parse_time_field(&message), Some(time));
    }

    #[test]
    fn test_parse_time_field_with_empty_config() {
        Config::setenv("TRANSFORM_BATCH_TIME_FIELDS", "");

        let message = json!({
            "time": 1646901960
        });

        assert_eq!(Helpers::parse_time_field(&message), None);
    }

    #[test]
    fn test_parse_time_field_with_string_timestamp() {
        Config::setenv("TRANSFORM_BATCH_TIME_FIELDS", "time");

        let message = json!({
            "time": "2022-03-10T12:00:00Z"
        });

        assert_eq!(Helpers::parse_time_field(&message), Some(1646913600));
    }

    #[test]
    fn test_parse_time_field_with_invalid_field() {
        Config::setenv("TRANSFORM_BATCH_TIME_FIELDS", "nonexistent");

        let message = json!({
            "time": 1646901960
        });

        assert_eq!(Helpers::parse_time_field(&message), None);
    }
}

#[cfg(test)]
mod parse_partition_tests {
    use super::*;
    use serde_json::json;
    use serial_test::serial;

    #[test]
    #[serial]
    fn test_parse_partition_field_no_config() {
        let message = json!({"foo": "bar", "abc1": "def"});
        Config::setenv("TRANSFORM_BATCH_PARTITION_FIELDS", "");
        let partition = Helpers::parse_partition_field(&message, HashSet::new());
        assert_eq!(partition, "");
    }

    #[test]
    #[serial]
    fn test_parse_partition_field_empty_field() {
        let message = json!({"foo": "", "abc1": "def"});
        Config::set_evncache("TRANSFORM_BATCH_PARTITION_FIELDS", "foo");
        let partition = Helpers::parse_partition_field(&message, HashSet::new());
        assert_eq!(partition, "p_foo=");
    }

    // #[test]
    // #[serial]
    // fn test_parse_partition_field_single_field() {
    //     let message = json!({"foo": "bar", "abc1": "def"});
    //     Config::set_evncache("TRANSFORM_BATCH_PARTITION_FIELDS", "foo");
    //     let partition = Helpers::parse_partition_field(&message);
    //     assert_eq!(partition, "p_foo=bar");
    // }

    #[test]
    #[serial]
    fn test_parse_partition_field_composite_key() {
        let message = json!({"foo": {"bar": "baz"}, "abc1": "def"});
        Config::set_evncache("TRANSFORM_BATCH_PARTITION_FIELDS", "foo.bar");
        let partition = Helpers::parse_partition_field(&message, HashSet::new());
        assert_eq!(partition, "p_bar=baz");
    }

    #[test]
    #[serial]
    fn test_parse_partition_field_several_composite_keys() {
        let message = json!({"foo": {"bar": "baz"}, "abc1": "def"});
        Config::set_evncache("TRANSFORM_BATCH_PARTITION_FIELDS", "foo.bar,abc1");
        let partition = Helpers::parse_partition_field(&message, HashSet::new());
        assert_eq!(partition, "p_bar=baz/p_abc1=def");
    }

    #[test]
    #[serial]
    fn test_parse_partition_field_several_composite_keys_with_spaces() {
        let message = json!({"foo": {"bar": "baz"}, "abc1": "def"});
        Config::set_evncache("TRANSFORM_BATCH_PARTITION_FIELDS", " foo.bar  , abc1  ");
        let partition = Helpers::parse_partition_field(&message, HashSet::new());
        assert_eq!(partition, "p_bar=baz/p_abc1=def");
    }
}

#[cfg(test)]
mod parse_partition_allowed_values_tests {
    use super::*;
    use serde_json::json;
    use serial_test::serial;
    use crate::ingest_work::PARTITION_ALLOWED_VALUES_CACHE;

    #[test]
    #[serial]
    fn test_parse_partition_field_no_config() {
        let message = json!({"foo": "bar", "abc1": "def"});
        Config::set_evncache("TRANSFORM_BATCH_PARTITION_FIELDS", "");
        Config::set_evncache("TRANSFORM_PARTITION_ALLOWED_VALUES", "bar");

        let allowed_values = Config::get_partition_allowed_values();
        let allowed_values_vec: HashSet<String> = allowed_values.split(',').map(|s| s.to_string()).collect();

        let partition = Helpers::parse_partition_field(&message, allowed_values_vec);
        assert_eq!(partition, "");
    }

    #[test]
    #[serial]
    fn test_parse_partition_field_empty_field() {
        let message = json!({"foo": "", "abc1": "def"});
        Config::set_evncache("TRANSFORM_BATCH_PARTITION_FIELDS", "foo");
        Config::set_evncache("TRANSFORM_PARTITION_ALLOWED_VALUES", "baz,def");

        let allowed_values = Config::get_partition_allowed_values();
        let allowed_values_vec: HashSet<String> = allowed_values.split(',').map(|s| s.to_string()).collect();

        let partition = Helpers::parse_partition_field(&message, allowed_values_vec);
        assert_eq!(partition, "p_foo=");
    }

    #[test]
    #[serial]
    fn test_parse_partition_field_single_field() {
        let message = json!({"foo": "bar", "abc1": "def"});
        Config::set_evncache("TRANSFORM_BATCH_PARTITION_FIELDS", "foo");
        Config::set_evncache("TRANSFORM_PARTITION_ALLOWED_VALUES", "bar,def");

        let allowed_values = Config::get_partition_allowed_values();
        let allowed_values_vec: HashSet<String> = allowed_values.split(',').map(|s| s.to_string()).collect();

        let partition = Helpers::parse_partition_field(&message, allowed_values_vec);
        assert_eq!(partition, "p_foo=bar");
    }

    #[test]
    #[serial]
    fn test_parse_partition_field_composite_key() {
        let message = json!({"foo": {"bar": "baz"}, "abc1": "def"});
        Config::set_evncache("TRANSFORM_BATCH_PARTITION_FIELDS", "foo.bar");
        Config::set_evncache("TRANSFORM_PARTITION_ALLOWED_VALUES", "baz,def");

        let allowed_values = Config::get_partition_allowed_values();
        let allowed_values_vec: HashSet<String> = allowed_values.split(',').map(|s| s.to_string()).collect();

        let partition = Helpers::parse_partition_field(&message, allowed_values_vec);
        assert_eq!(partition, "p_bar=baz");
    }

    #[test]
    #[serial]
    fn test_parse_partition_field_several_composite_keys() {
        let message = json!({"foo": {"bar": "baz"}, "abc1": "def"});
        Config::set_evncache("TRANSFORM_BATCH_PARTITION_FIELDS", "foo.bar,abc1");
        Config::set_evncache("TRANSFORM_PARTITION_ALLOWED_VALUES", "baz,def");

        let allowed_values = Config::get_partition_allowed_values();
        let allowed_values_vec: HashSet<String> = allowed_values.split(',').map(|s| s.to_string()).collect();

        let partition = Helpers::parse_partition_field(&message, allowed_values_vec);
        assert_eq!(partition, "p_bar=baz/p_abc1=def");
    }

    #[test]
    #[serial]
    fn test_parse_partition_field_several_composite_keys_with_spaces() {
        let message = json!({"foo": {"bar": "baz"}, "abc1": "def"});
        Config::set_evncache("TRANSFORM_BATCH_PARTITION_FIELDS", " foo.bar  , abc1  ");
        Config::set_evncache("TRANSFORM_PARTITION_ALLOWED_VALUES", "baz  , def  ");

        let allowed_values = Config::get_partition_allowed_values();
        let allowed_values_vec: HashSet<String> = allowed_values.split(',').map(|s| {
            Helpers::clean_field_name(s.to_string())
        }).collect();

        let partition = Helpers::parse_partition_field(&message, allowed_values_vec);
        assert_eq!(partition, "p_bar=baz/p_abc1=def");
    }
}

#[cfg(test)]
mod parse_partition_not_allowed_values_tests {
    use super::*;
    use serde_json::json;
    use serial_test::serial;

    #[test]
    #[serial]
    fn test_parse_partition_field_no_config() {
        let message = json!({"foo": "bar", "abc1": "def"});
        Config::set_evncache("TRANSFORM_BATCH_PARTITION_FIELDS", "");
        Config::set_evncache("TRANSFORM_PARTITION_ALLOWED_VALUES", "nah");

        let allowed_values = Config::get_partition_allowed_values();
        let allowed_values_vec: HashSet<String> = allowed_values.split(',').map(|s| s.to_string()).collect();

        let partition = Helpers::parse_partition_field(&message, allowed_values_vec);
        assert_eq!(partition, "");
    }

    #[test]
    #[serial]
    fn test_parse_partition_field_empty_field() {
        let message = json!({"foo": "", "abc1": "def"});
        Config::set_evncache("TRANSFORM_BATCH_PARTITION_FIELDS", "foo");
        Config::set_evncache("TRANSFORM_PARTITION_ALLOWED_VALUES", "baz,def");

        let allowed_values = Config::get_partition_allowed_values();
        let allowed_values_vec: HashSet<String> = allowed_values.split(',').map(|s| s.to_string()).collect();

        let partition = Helpers::parse_partition_field(&message, allowed_values_vec);
        assert_eq!(partition, "p_foo=");
    }

    #[test]
    #[serial]
    fn test_parse_partition_field_single_field() {
        let message = json!({"foo": "bar", "abc1": "def"});
        Config::set_evncache("TRANSFORM_BATCH_PARTITION_FIELDS", "foo");
        Config::set_evncache("TRANSFORM_PARTITION_ALLOWED_VALUES", "nah,def");

        let allowed_values = Config::get_partition_allowed_values();
        let allowed_values_vec: HashSet<String> = allowed_values.split(',').map(|s| s.to_string()).collect();

        let partition = Helpers::parse_partition_field(&message, allowed_values_vec);
        assert_eq!(partition, "p_foo=");
    }

    #[test]
    #[serial]
    fn test_parse_partition_field_composite_key() {
        let message = json!({"foo": {"bar": "baz"}, "abc1": "def"});
        Config::set_evncache("TRANSFORM_BATCH_PARTITION_FIELDS", "foo.bar");
        Config::set_evncache("TRANSFORM_PARTITION_ALLOWED_VALUES", "nah,def");

        let allowed_values = Config::get_partition_allowed_values();
        let allowed_values_vec: HashSet<String> = allowed_values.split(',').map(|s| s.to_string()).collect();

        let partition = Helpers::parse_partition_field(&message, allowed_values_vec);
        assert_eq!(partition, "p_bar=");
    }

    #[test]
    #[serial]
    fn test_parse_partition_field_several_composite_keys() {
        let message = json!({"foo": {"bar": "baz"}, "abc1": "def"});
        Config::set_evncache("TRANSFORM_BATCH_PARTITION_FIELDS", "foo.bar,abc1");
        Config::set_evncache("TRANSFORM_PARTITION_ALLOWED_VALUES", "nah,nope");

        let allowed_values = Config::get_partition_allowed_values();
        let allowed_values_vec: HashSet<String> = allowed_values.split(',').map(|s| s.to_string()).collect();

        let partition = Helpers::parse_partition_field(&message, allowed_values_vec);
        assert_eq!(partition, "p_bar=/p_abc1=");
    }

    #[test]
    #[serial]
    fn test_parse_partition_field_several_composite_keys_with_spaces() {
        let message = json!({"foo": {"bar": "baz"}, "abc1": "def"});
        Config::set_evncache("TRANSFORM_BATCH_PARTITION_FIELDS", " foo.bar  , abc1  ");
        Config::set_evncache("TRANSFORM_PARTITION_ALLOWED_VALUES", "nah  , nope  ");

        let allowed_values = Config::get_partition_allowed_values();
        let allowed_values_vec: HashSet<String> = allowed_values.split(',').map(|s| s.to_string()).collect();

        let partition = Helpers::parse_partition_field(&message, allowed_values_vec);
        assert_eq!(partition, "p_bar=/p_abc1=");
    }
}

#[cfg(test)]
mod parse_namespace_field_tests {
    use super::*;
    use serde_json::json;
    use serial_test::serial;

    #[test]
    #[serial]
    fn test_parse_namespace_field_with_empty_field() {
        let mut cache = HashMap::new();
        let message = json!({"my_field": ""});
        let namespace = "my_namespace".to_string();
        Config::setenv("TRANSFORM_NAMESPACE_FIELDS", "my_field");
        let result = Helpers::parse_namespace_field(&message, namespace, &mut cache);
        assert_eq!(result, "my_namespace");
        assert_eq!(cache.get("my_namespace"), Some(&"no".to_string()));
    }

    #[test]
    #[serial]
    fn test_parse_namespace_field_with_missing_field() {
        let mut cache = HashMap::new();
        let message = json!({"my_field": "blah"});
        let namespace = "my_namespace".to_string();
        Config::setenv("TRANSFORM_NAMESPACE_FIELDS", "abc");
        let result = Helpers::parse_namespace_field(&message, namespace, &mut cache);
        assert_eq!(result, "my_namespace");
        assert_eq!(cache.get("my_namespace"), Some(&"no".to_string()));
    }

    #[test]
    #[serial]
    fn test_parse_namespace_field_with_existing_namespace() {
        let mut cache = HashMap::new();
        cache.insert("my_namespace".to_string(), "yes".to_string());
        let message = json!({"my_field": "my_value"});
        let namespace = "my_namespace".to_string();
        Config::setenv("TRANSFORM_NAMESPACE_FIELDS", "");
        let result = Helpers::parse_namespace_field(&message, namespace, &mut cache);
        assert_eq!(result, "my_namespace");
        assert_eq!(cache.get("my_namespace"), Some(&"no".to_string()));
    }

    #[test]
    #[serial]
    fn test_parse_namespace_field_with_new_namespace() {
        let mut cache = HashMap::new();
        let message = json!({"my_field": "my_value"});
        let namespace = "my_namespace".to_string();
        Config::setenv("TRANSFORM_NAMESPACE_FIELDS", "");
        let result = Helpers::parse_namespace_field(&message, namespace, &mut cache);
        assert_eq!(result, "my_namespace");
        assert_eq!(cache.get("my_namespace"), Some(&"no".to_string()));
    }

    #[test]
    #[serial]
    fn test_parse_namespace_field_with_composite_key() {
        let mut cache = HashMap::new();
        let message =
            json!({"entity_field_1": "entity_value_1","entity_field_2": "entity_value_2"});
        let namespace = "my_namespace".to_string();
        Config::setenv(
            "TRANSFORM_NAMESPACE_FIELDS",
            "entity_field_1,entity_field_2",
        );
        let result = Helpers::parse_namespace_field(&message, namespace, &mut cache);
        assert_eq!(result, "entity_value_1_entity_value_2");
        assert_eq!(cache.get("my_namespace"), Some(&"yes".to_string()));
        // assert_eq!(cache.get("entity_field_1,entity_field_2"), Some(&"yes".to_string()));
    }

    #[test]
    #[serial]
    fn test_parse_namespace_field_with_neasted_composite_key() {
        let mut cache = HashMap::new();
        let message = json!({
            "entity_field_1": "entity_value_1",
            "entity_field_2": "entity_value_2",
            "entity_field_3": {
                "entity_field_3a": "entity_value_3a",
            }
        });
        let namespace = "my_namespace".to_string();
        Config::setenv(
            "TRANSFORM_NAMESPACE_FIELDS",
            "entity_field_1,entity_field_3.entity_field_3a",
        );
        let result = Helpers::parse_namespace_field(&message, namespace, &mut cache);
        assert_eq!(result, "entity_value_1_entity_value_3a");
        assert_eq!(cache.get("my_namespace"), Some(&"yes".to_string()));
        // assert_eq!(cache.get("entity_field_1,entity_field_2"), Some(&"yes".to_string()));
    }
}

#[cfg(test)]
mod flattern_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_metadata_default() {
        let default_metadata = Metadata::new().unwrap();
        assert_eq!(default_metadata.count, 0);
        assert!(default_metadata.types.is_empty());
        assert_eq!(default_metadata.parent_type, "");
        assert!(default_metadata.fields.is_empty());
        assert!(default_metadata.date_candidate.is_none());
        assert!(default_metadata.evolution.is_empty());
        assert!(default_metadata.enabled);
        assert_eq!(default_metadata.out_field_name, "");
        assert_eq!(default_metadata.determined_type, "");
        assert_eq!(default_metadata.determined_type_values, "");
    }

    #[test]
    fn test_flatten_empty_object() {
        let json = json!({});
        let metadata = HashMap::new();
        let flattened = Helpers::flatten(&json, &metadata);
        assert_eq!(flattened.unwrap(), json!({}));
    }

    #[test]
    fn test_flatten_simple_object() {
        let json = json!(
            {
                "field": "value",
                "contact": {
                    "name": "Dave",
                    "tel": "123"
                }
            }
        );
        let mut metadata = HashMap::new();
        metadata.insert(
            "field".into(),
            Metadata {
                count: 1,
                types: HashMap::new(),
                parent_type: "".into(),
                fields: Box::new(HashMap::new()),
                date_candidate: None,
                evolution: Box::new(HashMap::new()),
                enabled: true,
                out_field_name: "field".into(),
                determined_type: "string".into(),
                determined_type_values: "".into(),
            },
        );
        metadata.insert(
            "contact".into(),
            Metadata {
                count: 1,
                types: HashMap::new(),
                parent_type: "".into(),
                fields: Box::new(HashMap::new()),
                date_candidate: None,
                evolution: Box::new(HashMap::new()),
                enabled: true,
                out_field_name: "contact".into(),
                determined_type: "record".into(),
                determined_type_values: "".into(),
            },
        );
        metadata.get_mut("contact").unwrap().fields.insert(
            "name".into(),
            Metadata {
                count: 1,
                types: HashMap::new(),
                parent_type: "".into(),
                fields: Box::new(HashMap::new()),
                date_candidate: None,
                evolution: Box::new(HashMap::new()),
                enabled: true,
                out_field_name: "contact_name".into(),
                determined_type: "string".into(),
                determined_type_values: "".into(),
            },
        );
        metadata.get_mut("contact").unwrap().fields.insert(
            "tel".into(),
            Metadata {
                count: 1,
                types: HashMap::new(),
                parent_type: "".into(),
                fields: Box::new(HashMap::new()),
                date_candidate: None,
                evolution: Box::new(HashMap::new()),
                enabled: true,
                out_field_name: "contact_tel".into(),
                determined_type: "int".into(),
                determined_type_values: "".into(),
            },
        );
        let flattened = Helpers::flatten(&json, &metadata);
        assert_eq!(
            flattened.unwrap(),
            json!({ "field": "value", "contact_name": "Dave", "contact_tel": "123" })
        );
    }

    #[test]
    fn test_flatten_array() {
        let json = json!(
            {
                "field": "value",
                "contacts": [
                    {
                        "name": "Dave",
                        "tel": "123"
                    },
                    {
                        "name": "John",
                        "tel": "456"
                    }
                ]
            }
        );
        let mut metadata = HashMap::new();
        metadata.insert(
            "field".into(),
            Metadata {
                count: 1,
                types: HashMap::new(),
                parent_type: "".into(),
                fields: Box::new(HashMap::new()),
                date_candidate: None,
                evolution: Box::new(HashMap::new()),
                enabled: true,
                out_field_name: "field".into(),
                determined_type: "string".into(),
                determined_type_values: "".into(),
            },
        );
        metadata.insert(
            "contacts".into(),
            Metadata {
                count: 2,
                types: HashMap::new(),
                parent_type: "".into(),
                fields: Box::new(HashMap::new()),
                date_candidate: None,
                evolution: Box::new(HashMap::new()),
                enabled: true,
                out_field_name: "contacts".into(),
                determined_type: "array".into(),
                determined_type_values: "".into(),
            },
        );
        metadata.get_mut("contacts").unwrap().fields.insert(
            "name".into(),
            Metadata {
                count: 2,
                types: HashMap::new(),
                parent_type: "".into(),
                fields: Box::new(HashMap::new()),
                date_candidate: None,
                evolution: Box::new(HashMap::new()),
                enabled: true,
                out_field_name: "contacts_name".into(),
                determined_type: "string".into(),
                determined_type_values: "".into(),
            },
        );
        metadata.get_mut("contacts").unwrap().fields.insert(
            "tel".into(),
            Metadata {
                count: 2,
                types: HashMap::new(),
                parent_type: "".into(),
                fields: Box::new(HashMap::new()),
                date_candidate: None,
                evolution: Box::new(HashMap::new()),
                enabled: true,
                out_field_name: "contacts_tel".into(),
                determined_type: "int".into(),
                determined_type_values: "".into(),
            },
        );
        let flattened = Helpers::flatten(&json, &metadata);
        assert_eq!(
            flattened.unwrap(),
            json!({
                "field": "value",
                "contacts_0_name": "Dave",
                "contacts_1_name": "John",
                "contacts_0_tel": "123",
                "contacts_1_tel": "456"
            })
        );
    }

    // #[test]
    // fn test_flatten() {
    //     let input = json!({
    //         "key1": "value1",
    //         "key2": {
    //             "key3": "value3",
    //             "key4": {
    //                 "key5": "value5"
    //             }
    //         },
    //         "key6": ["value6", "value7"]
    //     });
    //     let expected_output = json!({
    //         "key1": "value1",
    //         "key2_key3": "value3",
    //         "key2_key4_key5": "value5",
    //         "key6": ["value6", "value7"]
    //     });
    //
    //     let flattened = Helpers::flatten(&input, &metadata);
    //     assert_eq!(flattened.unwrap(), json!({}));
    //     assert_eq!(Helpers::flatten(&input).unwrap(), json!(expected_output));
    //
    //
    //     let input = json!({
    //         "key1": "value1",
    //         "key2": {
    //             "key3": "value3",
    //             "key4": {
    //                 "key5": "value5"
    //             }
    //         },
    //         "key6": ["value6", "value7", {
    //             "key8": "value8"
    //         }]
    //     });
    //     let expected_output = json!({
    //         "key1": "value1",
    //         "key2_key3": "value3",
    //         "key2_key4_key5": "value5",
    //         "key6_0": "value6",
    //         "key6_1": "value7",
    //         "key6_2_key8": "value8"
    //     });
    //     assert_eq!(Helpers::flatten(&input), expected_output);
    //
    //     let input = json!({
    //         "empty_obj": {},
    //         "empty_arr": [],
    //         "empty_str": ""
    //     });
    //     let expected_output = json!({
    //         "empty_obj": {},
    //         "empty_arr": [],
    //         "empty_str": ""
    //     });
    //     assert_eq!(Helpers::flatten(&input), expected_output);
    // }

    // Additional tests can be written similarly...
}
