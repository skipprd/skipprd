use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use memory_stats::memory_stats;
use regex::Regex;
use std::collections::HashMap;
use std::env;

use std::str;

use rand::Rng;
use serde_json::{Map, Value};
use std::error::Error;

pub mod configuration;
pub mod license;
pub mod logger;
pub mod offsets;

// let clean_field_cache = Arc::new(Mutex::new(HashMap<String, bool> = HashMap::new()));

use crate::discover::date_formats::DateFormats;
use crate::discover::Metadata;
use crate::helpers::configuration::Config;
use once_cell::sync::Lazy;
use std::sync::Mutex;

// static clean_field_cache: Lazy<Mutex<i64>> = Lazy::new(|| Mutex::new(1));
static clean_field_cache: Lazy<Mutex<HashMap<String, String>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub struct Helpers {}

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

        if clean_field_cache_lock.contains_key(&field)
            && clean_field_cache_lock[&field] != "no".to_string()
        {
            return clean_field_cache_lock[&field].to_string();
        } else if !clean_field_cache_lock.contains_key(&field) {
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

            if clean != field {
                // println!("Cleaned {} field to {}", field, clean);
                clean_field_cache_lock.insert(field, clean.clone());
            } else {
                // println!("Not cleaned {} field to {}", field, clean);
                clean_field_cache_lock.insert(field, "no".to_string());
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
        let alphabet = "abcdefghijklmnopqrstuvwxyz";
        let mut pass = String::new();
        let alpha_length = alphabet.len() - 1;
        for _ in 0..length {
            let n = rng.gen_range(0..alpha_length);
            pass.push(alphabet.chars().nth(n).unwrap());
        }
        pass
    }

    // fn flatten_internal(json: &Value, result: &mut Map<String, Value>, metadata: &Metadata) {
    fn flatten_internal(field: &str, json: &Value, result: &mut Map<String, Value>) {
        match json {
            Value::Object(map) => {
                if map.is_empty() {
                    // result.insert(metadata.get(), json.clone());
                } else {
                    for (key, value) in map {
                        // let new_key = if prefix.is_empty() {
                        //     key.clone()
                        // } else {
                        //     format!("{}_{}", prefix, key)
                        // };
                        // Helpers::flatten_internal(value, result, metadata.fields.get(&key.clone()).unwrap());
                        Helpers::flatten_internal(key, value, result);
                    }
                }
            }
            Value::Array(arr) => {
                if arr.is_empty() {
                    // result.insert(prefix.to_string(), json.clone());
                } else {
                    // for (index, value) in arr.iter().enumerate() {
                    //     // let new_key = format!("{}_{}", prefix, index);
                    //     // Helpers::flatten_internal(value, result, metadata.fields.get(&index.to_string()).unwrap());
                    //     Helpers::flatten_internal(&index.to_string(), value, result);
                    // }
                    result.insert(field.to_string(), json.clone());
                }
            }
            _ => {
                // if metadata.determined_type != "record" {
                //     result.insert(metadata.out_field_name.clone(), json.clone());
                result.insert(field.to_string(), json.clone());
                // }
            }
        }
    }

    pub fn flatten(json: &Value, _metadata: &HashMap<String, Metadata>) -> Result<Value, Box<dyn Error>> {
        let mut result = Map::new();
        for (key, value) in json.as_object().ok_or("Invalid JSON object")? {
            Helpers::flatten_internal(key, value, &mut result);
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

    pub fn parse_partition_field(message: &Value) -> String {
        let mut clean_partition: String = "".to_string();

        // optional: partition by composite key
        if !Config::getenv("TRANSFORM_BATCH_PARTITION_FIELDS", "").is_empty() {
            let mut partitions = vec![];

            for entity_field_dot in
                Config::getenv("TRANSFORM_BATCH_PARTITION_FIELDS", "").split(',')
            {
                let clean_entity_value =
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
            if !Config::getenv("TRANSFORM_NAMESPACE_FIELDS", "").is_empty() {
                let mut namespaces = vec!["".to_string()];

                for entity_field_dot in Config::getenv("TRANSFORM_NAMESPACE_FIELDS", "").split(',')
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
        // default to beginning of epoch.
        let mut time_field_value: Option<i64> = None;

        if !Config::getenv("TRANSFORM_BATCH_TIME_FIELDS", "").is_empty() {
            // Support nested time fields via array dot notation
            // For user confirmed event time fields, use the first one that matches
            for field_dot in Config::getenv("TRANSFORM_BATCH_TIME_FIELDS", "").split(',') {
                match Helpers::get_nested_value_from_dot_notation(message, field_dot) {
                    Some(value) => {
                        // Handle millisecond timestamps
                        match value.as_i64() {
                            Some(i64_val) => {
                                if Helpers::is_millisecond_timestamp(i64_val) {
                                    time_field_value = Some(i64_val / 1000);
                                } else {
                                    // Handle second timestamps
                                    time_field_value = Some(i64_val);
                                }
                            }
                            None => time_field_value = None,
                        }

                        // println!("1");
                        if time_field_value.is_none() {
                            // Handle datetime strings
                            match value.as_str() {
                                Some(val) => {
                                    // println!("2");
                                    for format in DateFormats::iterator() {
                                        // println!("3: {} ? {}", val, format.as_str());
                                        time_field_value = match NaiveDateTime::parse_from_str(
                                            val,
                                            format.as_str(),
                                        ) {
                                            Ok(dt) => {
                                                // println!("3.1: FOUND {}", format.as_str());
                                                Some(DateTime::<Utc>::from_utc(dt, Utc).timestamp())
                                            }
                                            Err(_err) => {
                                                // println!("{:?}", err);
                                                None
                                            }
                                        };

                                        if time_field_value.is_some() {
                                            // println!("4: {}", format.as_str());
                                            return time_field_value;
                                        }
                                    }
                                }
                                None => {
                                    time_field_value = None;
                                }
                            };
                        }
                    }
                    None => {
                        time_field_value = None;
                    }
                };
            }
        }

        time_field_value
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
}

#[cfg(test)]
mod clean_field_name_tests {
    use super::*;

    #[test]
    fn test_clean_field_name() {
        // Cache is empty, alphanumeric input
        {
            let mut clean_field_cache_lock = clean_field_cache.lock().unwrap();
            clean_field_cache_lock.clear();
        }
        assert_eq!(
            Helpers::clean_field_name("testField".to_string()),
            "testfield".to_string()
        );

        // Cache is empty, input with special characters
        {
            let mut clean_field_cache_lock = clean_field_cache.lock().unwrap();
            clean_field_cache_lock.clear();
        }
        assert_eq!(
            Helpers::clean_field_name("test!@#Field$%^&".to_string()),
            "test_field".to_string()
        );

        // Cache is empty, input starts with numbers
        {
            let mut clean_field_cache_lock = clean_field_cache.lock().unwrap();
            clean_field_cache_lock.clear();
        }
        assert_eq!(
            Helpers::clean_field_name("123testField".to_string()),
            "testfield".to_string()
        );

        // Cache is empty, input starts with underscore and numbers
        {
            let mut clean_field_cache_lock = clean_field_cache.lock().unwrap();
            clean_field_cache_lock.clear();
        }
        assert_eq!(
            Helpers::clean_field_name("_123testField".to_string()),
            "123testfield".to_string()
        );

        // Cache is empty, input is numbers
        {
            let mut clean_field_cache_lock = clean_field_cache.lock().unwrap();
            clean_field_cache_lock.clear();
        }
        assert_eq!(
            Helpers::clean_field_name("1".to_string()),
            "item_1".to_string()
        );

        // Cache has a record
        {
            let mut clean_field_cache_lock = clean_field_cache.lock().unwrap();
            clean_field_cache_lock.insert("cachedField".to_string(), "cachedfield".to_string());
        }
        assert_eq!(
            Helpers::clean_field_name("cachedField".to_string()),
            "cachedfield".to_string()
        );

        // Cache has a record marked as "no"
        {
            let mut clean_field_cache_lock = clean_field_cache.lock().unwrap();
            clean_field_cache_lock.insert("no_change_field".to_string(), "no".to_string());
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
    use std::env;

    #[test]
    fn test_parse_time_field_no_time_fields() {
        // Mocking the environment variable.
        env::set_var("TRANSFORM_BATCH_TIME_FIELDS", "");

        let message = json!({
            "key": "value"
        });

        assert_eq!(Helpers::parse_time_field(&message), None);
    }

    #[test]
    fn test_parse_time_field_with_millisecond_timestamp() {
        // Mocking the environment variable.
        env::set_var("TRANSFORM_BATCH_TIME_FIELDS", "time1");

        let time = Utc::now().timestamp_millis();
        println!("{}", time);
        let message = json!({ "time1": time });

        assert_eq!(Helpers::parse_time_field(&message), Some(time / 1000));
    }

    #[test]
    fn test_parse_time_field_with_invalid_millisecond_timestamp() {
        // Mocking the environment variable.
        env::set_var("TRANSFORM_BATCH_TIME_FIELDS", "time2");

        let time: i64 = 999999999; // Invalid timestamp, less than 1000000000000
        let message = json!({ "time2": time });

        assert_eq!(Helpers::parse_time_field(&message), None);
    }

    #[test]
    fn test_parse_time_field_with_valid_second_timestamp() {
        // Mocking the environment variable.
        env::set_var("TRANSFORM_BATCH_TIME_FIELDS", "time3");

        let time: i64 = 1646901960;
        let message = json!({ "time3": time });

        assert_eq!(Helpers::parse_time_field(&message), Some(time));
    }

    #[test]
    fn test_parse_time_field_with_datetime_string() {
        // Mocking the environment variable.
        env::set_var("TRANSFORM_BATCH_TIME_FIELDS", "time4");

        let dt = Utc::now();
        let time = dt.to_rfc3339();
        let message = json!({ "time4": time });

        assert_eq!(Helpers::parse_time_field(&message), Some(dt.timestamp()));
    }

    #[test]
    fn test_parse_time_field_with_invalid_datetime_string() {
        // Mocking the environment variable.
        env::set_var("TRANSFORM_BATCH_TIME_FIELDS", "time5");

        let time = "invalid datetime string";
        let _message = json!({ "time5": time });
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
        std::env::set_var("TRANSFORM_BATCH_PARTITION_FIELDS", "");
        let partition = Helpers::parse_partition_field(&message);
        assert_eq!(partition, "");
    }

    #[test]
    #[serial]
    fn test_parse_partition_field_empty_field() {
        let message = json!({"foo": "", "abc1": "def"});
        std::env::set_var("TRANSFORM_BATCH_PARTITION_FIELDS", "foo");
        let partition = Helpers::parse_partition_field(&message);
        assert_eq!(partition, "p_foo=");
    }

    #[test]
    #[serial]
    fn test_parse_partition_field_single_field() {
        let message = json!({"foo": "bar", "abc1": "def"});
        std::env::set_var("TRANSFORM_BATCH_PARTITION_FIELDS", "foo");
        let partition = Helpers::parse_partition_field(&message);
        assert_eq!(partition, "p_foo=bar");
    }

    #[test]
    #[serial]
    fn test_parse_partition_field_composite_key() {
        let message = json!({"foo": {"bar": "baz"}, "abc1": "def"});
        std::env::set_var("TRANSFORM_BATCH_PARTITION_FIELDS", "foo.bar");
        let partition = Helpers::parse_partition_field(&message);
        assert_eq!(partition, "p_bar=baz");
    }

    #[test]
    #[serial]
    fn test_parse_partition_field_several_composite_keys() {
        let message = json!({"foo": {"bar": "baz"}, "abc1": "def"});
        std::env::set_var("TRANSFORM_BATCH_PARTITION_FIELDS", "foo.bar,abc1");
        let partition = Helpers::parse_partition_field(&message);
        assert_eq!(partition, "p_bar=baz/p_abc1=def");
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
        assert!(!default_metadata.enabled);
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
                determined_type: "".into(),
                determined_type_values: "".into(),
            },
        );
        metadata.insert(
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
                determined_type: "".into(),
                determined_type_values: "".into(),
            },
        );
        metadata.insert(
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
                determined_type: "".into(),
                determined_type_values: "".into(),
            },
        );
        let flattened = Helpers::flatten(&json, &metadata);
        assert_eq!(
            flattened.unwrap(),
            json!({ "field": "value", "contact_name": "Dave", "contact_tel": "123" })
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
    //     assert_eq!(Helpers::flatten(&input), expected_output);
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
