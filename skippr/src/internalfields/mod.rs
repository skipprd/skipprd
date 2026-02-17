use std::collections::HashMap;
use std::sync::{Arc, Mutex};

// mod arr;

// mod helpers;

#[allow(dead_code)]
pub struct InternalFields {
    pub parse_namespace_cache: Arc<Mutex<HashMap<String, String>>>,
    pub skip_fields: Vec<String>,
    pub metadata_fields: Vec<String>,
    pub data_fields: Vec<String>,
    pub transforms: HashMap<String, String>,
}

#[allow(dead_code)]
impl InternalFields {
    // pub fn parse_partition_field<'a>(message: &mut HashMap<String, String>, partition: &'a str) -> &'a str {
    //     let mut partition = partition.to_string();
    //
    //     // let mut helpers = Helpers { clean_field_cache: Default::default() };
    //     let mut clean_field_cache_lock = *clean_field_cache.lock().unwrap();
    //
    //     // default to data source partition (table, topic, queue, file dir, etc)
    //     partition = clean_field_cache_lock(&mut partition).parse().unwrap();
    //
    //     // optional: partition by composite key
    //     if Config::partition_by_fields.len() > 0 {
    //         partition = "".to_string();
    //
    //         for entity_field_dot in Config::partition_by_fields.iter() {
    //             if let Some(entity_value) = message.get(entity_field_dot) {
    //                 let clean_entity_field_name = clean_field_cache_lock(entity_field_dot);
    //                 let clean_entity_field_value = clean_field_cache_lock(entity_value);
    //                 partition.push_str(&format!("-{}={}", clean_entity_field_name, clean_entity_field_value));
    //             }
    //         }
    //     }
    //
    //     let partition = partition.to_lowercase();
    //     let partition = partition.trim_start_matches('-');
    //     let partition = partition.trim_end_matches('-');
    //
    //     message.insert("skpr_partition".to_string(), partition.to_string());
    //
    //     partition
    // }

    pub fn parse_source_partition(partition: &str) -> &str {
        let end = partition.find('-');

        match end {
            Some(end) => &partition[..end],
            None => partition,
        }
    }

    // pub fn parse_namespace_field(&mut self,
    //     message: &mut HashMap<String, String>,
    //     namespace: &str,
    //     PARSE_NAMESPACE_CACHE: HashMap<String, String>
    // ) -> &str {
    //     let mut clean_namespace = namespace;
    //
    //     // let mut helpers = Helpers { clean_field_cache: Default::default() };
    //     let mut clean_field_cache_lock = PARSE_NAMESPACE_CACHE;
    //
    //     if !clean_field_cache_lock.contains_key(namespace)
    //         || *clean_field_cache_lock.get(namespace).unwrap() == "yes"
    //     {
    //         // default to data source partition (table, topic, queue, file dir, etc)
    //         clean_namespace = clean_field_cache_lock.get(namespace).unwrap();
    //
    //         // optional: partition by composite key
    //         if Config::getenv("TRANSFORM_NAMESPACE_FIELDS", "") != "" {
    //             let mut namespaces = vec![<String>];
    //
    //             for entity_field_dot in Config::getenv("TRANSFORM_NAMESPACE_FIELDS", "").split(",") {
    //                 if let Some(entity_value) = message.get(entity_field_dot) {
    //                     namespaces.push(clean_field_cache_lock.get(entity_value).unwrap());
    //                 }
    //             }
    //
    //             clean_namespace = &*namespaces.join("_");
    //
    //             clean_namespace = &*clean_namespace.trim_matches('-').to_lowercase();
    //         }
    //     }
    //
    //     if clean_namespace != namespace {
    //         // *clean_field_cache_lock.get_mut(namespace).unwrap() = "yes".to_string();
    //         clean_field_cache_lock.insert(namespace.to_string(), "yes".to_string());
    //     }
    //     else {
    //         clean_field_cache_lock.insert(namespace.to_string(), "no".to_string());
    //     }
    //
    //     message.insert("skpr_namespace".to_string(), clean_namespace.to_string());
    //
    //     clean_namespace
    // }

    pub fn parse_source_namespace(namespace: &str) -> &str {
        let end = namespace.find('-');

        match end {
            Some(end) => &namespace[..end],
            None => namespace,
        }
    }

    // pub fn parse_time_field(message: &mut HashMap<String, String>) -> i64 {
    //     // default to beginning of epoch.
    //     message.insert("skpr_event_ts".to_string(), "0".to_string());
    //
    //     if let Some(time_fields) = Config::time_fields() {
    //         // Support nested time fields via array dot notation
    //         // For user confirmed event time fields, use the first one that matches
    //         for field_dot in time_fields {
    //             if let Some(time_value) = message.get(&field_dot) {
    //                 message.insert("skpr_event_ts".to_string(), time_value.to_string());
    //                 break;
    //             }
    //         }
    //
    //         // Handle millisecond timestamps
    //         if message.get("skpr_event_ts").unwrap().len() == 13 {
    //             let time_value = message.get("skpr_event_ts").unwrap().parse::<i64>().unwrap();
    //             message.insert("skpr_event_ts".to_string(), (time_value / 1000).to_string());
    //         }
    //
    //         // Handle datetime strings
    //         if message.get("skpr_event_ts").unwrap().len() > 13 {
    //             let time_value = message.get("skpr_event_ts").unwrap().parse::<i64>().unwrap();
    //             message.insert("skpr_event_ts".to_string(), time_value.to_string());
    //         }
    //     }
    //
    //     message.get("skpr_event_ts").unwrap().parse::<i64>().unwrap()
    // }
}
