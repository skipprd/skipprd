
use std::collections::HashMap;
use regex::Regex;
use std::env;
use memory_stats::memory_stats;
use chrono::{DateTime, TimeZone, Utc};





use std::str;

use rand::Rng;
use serde_json::Value;

pub mod configuration;

// let clean_field_cache = Arc::new(Mutex::new(HashMap<String, bool> = HashMap::new()));

use once_cell::sync::Lazy;
use std::sync::Mutex;
use crate::helpers::configuration::Config;

// static clean_field_cache: Lazy<Mutex<i64>> = Lazy::new(|| Mutex::new(1));
static clean_field_cache: Lazy<Mutex<HashMap<String, bool>>> = Lazy::new(|| Mutex::new(HashMap::new()));


pub struct Helpers {

}

impl Helpers {

    // let clean_field_cache: HashMap<String, bool> = HashMap::new();
    // pub(crate) clean_field_cache: HashMap<String, bool> = HashMap::new();

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
        let mut clean = field.to_string();

        let clean_field_cache_lock = &mut *clean_field_cache.lock().unwrap();

        if !clean_field_cache_lock.contains_key(&field) || clean_field_cache_lock[&field] == true {
            if field.parse::<i32>().is_ok() {
                clean = "item_".to_string() + &field;
            }

            clean = clean.to_lowercase();

            let re = Regex::new(r"[^_0123456789_abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ]").unwrap();
            clean = re.replace_all(&clean, "_").to_string();

            // let pattern = "/[^" + preg_quote(
            //     "_0123456789_abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ",
            //     "/"
            // ) + "]/";
            // clean = preg_replace(pattern, "_", field);

            let x: &[_] = &['1', '2', '3', '4', '5', '6', '7', '8', '9'];
            clean = clean.trim_start_matches(x).to_string();
            // clean = ltrim(clean, "0123456789");

            // '_' at the beginning is common and probably allowable
            clean = clean.trim_start_matches("_").to_string();
            // clean = trim(clean, '_');

            if clean != field {
                // println!("Cleaned {} field to {}", field, clean);
                clean_field_cache_lock.insert(field, true);
            } else {
                // println!("Not cleaned {} field to {}", field, clean);
                clean_field_cache_lock.insert(field, false);
            }
        }

        clean.to_string()
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
        let alphabet = "abcdefghijklmnopqrstuvwxyz";
        let mut pass = String::new();
        let alpha_length = alphabet.len() - 1;
        for _ in 0..length {
            let n = rng.gen_range(0..alpha_length);
            pass.push(alphabet.chars().nth(n).unwrap());
        }
        pass
    }

    pub fn flatten(array: &HashMap<String, String>, delimiter: &str, prefix: &str) -> HashMap<String, String> {
        let mut result: HashMap<String, String> = HashMap::new();
        for (key, value) in array {
            if value.contains("{") {
                let mut new_prefix = prefix.to_string();
                new_prefix.push_str(delimiter);
                new_prefix.push_str(key);
                result.extend(Self::flatten(array, delimiter, &new_prefix));
            } else {
                let mut new_prefix = prefix.to_string();
                new_prefix.push_str(delimiter);
                new_prefix.push_str(key);
                result.insert(new_prefix, value.to_string());
            }
        }
        result
    }

    pub fn mem_limit_reached() -> bool {
        let mem_limit = env::var("MEM_LIMIT").unwrap_or("0".to_string()).parse::<u32>().unwrap_or(0);
        let mut mem_usage = 0;
        if let Some(usage) = memory_stats() {
            mem_usage = usage.physical_mem as u32;
        }

        if mem_usage >= mem_limit {
            return true;
        }

        return false;
    }


    pub fn parse_namespace_field(
        message: & Value,
        namespace: String,
        parse_namespace_cache: &mut HashMap<String, String>
    ) -> String {
        let mut clean_namespace = namespace.clone();

        if !parse_namespace_cache.contains_key(&namespace)
            || parse_namespace_cache.get(&namespace).unwrap() == "yes"
        {
            // default to data source partition (table, topic, queue, file dir, etc)
            clean_namespace = Helpers::clean_field_name(clean_namespace);

            // optional: partition by composite key
            if Config::getenv("DATA_SOURCE_EVENT_TYPE_FIELDS", "") != "" {

                let mut namespaces = vec!["".to_string()];

                for entity_field_dot in Config::getenv("DATA_SOURCE_EVENT_TYPE_FIELDS", "").split(",") {

                    match Helpers::get_nested_value_from_dot_notation(message, entity_field_dot) {
                        Some(entity_value) => {
                            namespaces.push(entity_value.as_str().unwrap().to_string());
                        },
                        None => ()
                    }

                }

                clean_namespace = namespaces.join("_");

                clean_namespace = clean_namespace.trim_matches('_').to_lowercase();

            }
        }

        if clean_namespace != namespace {

            parse_namespace_cache.insert(namespace.to_string(), "yes".to_string());
        }
        else {
            parse_namespace_cache.insert(namespace.to_string(), "no".to_string());
        }

        clean_namespace

    }


    pub fn parse_time_field(
        message: &Value,
    ) -> Option<i64> {
        // default to beginning of epoch.
        let mut time_field_value: Option<i64> = None;

        if !Config::getenv("DATA_OUTPUT_TIME_FIELDS", "").is_empty() {
            // Support nested time fields via array dot notation
            // For user confirmed event time fields, use the first one that matches
            for field_dot in Config::getenv("DATA_OUTPUT_TIME_FIELDS", "").split(",") {
                match Helpers::get_nested_value_from_dot_notation(message, field_dot) {
                    Some(value) => {
                        // Handle millisecond timestamps
                        match value.as_i64() {
                            Some(i64_val) => {
                                if i64_val > 1000000000000 {
                                    time_field_value = Some(i64_val / 1000)
                                } else {
                                    time_field_value = None
                                }
                            },
                            None => time_field_value = None
                        }

                        // Handle datetime strings
                        match value.as_str() {
                            Some(val) => {
                                if let Ok(dt) = DateTime::parse_from_rfc3339(val) {
                                    time_field_value = Some(dt.with_timezone(&Utc).timestamp());
                                }
                            },
                            None => { time_field_value = None; }
                        };
                    },
                    None => { time_field_value = None; },
                };
            }
        }

        time_field_value
    }


    fn get_nested_value_from_dot_notation(json_value: &Value, field_str: &str) -> Option<Value> {
        // Parse the JSON string into a serde_json Value object
        // let json_value: Value = serde_json::from_str(json_str).ok()?;

        // Split the dot notation string into individual field names
        let fields: Vec<&str> = field_str.split('.').collect();

        // Traverse the JSON object, following each field name in turn
        let mut current_value: &Value = &json_value;
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

}


#[cfg(test)]
mod parse_namespace_field_tests {
    use serial_test::serial;
    use serde_json::json;
    use super::*;

    #[test]
    #[serial]
    fn test_parse_namespace_field_with_existing_namespace() {
        let mut cache = HashMap::new();
        cache.insert("my_namespace".to_string(), "yes".to_string());
        let message = json!({"my_field": "my_value"});
        let namespace = "my_namespace".to_string();
        Config::setenv("DATA_SOURCE_EVENT_TYPE_FIELDS", "");
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
        Config::setenv("DATA_SOURCE_EVENT_TYPE_FIELDS", "");
        let result = Helpers::parse_namespace_field(&message, namespace, &mut cache);
        assert_eq!(result, "my_namespace");
        assert_eq!(cache.get("my_namespace"), Some(&"no".to_string()));
    }


    #[test]
    #[serial]
    fn test_parse_namespace_field_with_composite_key() {
        let mut cache = HashMap::new();
        let message = json!({"entity_field_1": "entity_value_1","entity_field_2": "entity_value_2"});
        let namespace = "my_namespace".to_string();
        Config::setenv("DATA_SOURCE_EVENT_TYPE_FIELDS", "entity_field_1,entity_field_2");
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
        Config::setenv("DATA_SOURCE_EVENT_TYPE_FIELDS", "entity_field_1,entity_field_3.entity_field_3a");
        let result = Helpers::parse_namespace_field(&message, namespace, &mut cache);
        assert_eq!(result, "entity_value_1_entity_value_3a");
        assert_eq!(cache.get("my_namespace"), Some(&"yes".to_string()));
        // assert_eq!(cache.get("entity_field_1,entity_field_2"), Some(&"yes".to_string()));
    }


}