#[allow(unused_imports)]
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use memory_stats::memory_stats;
use regex::Regex;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::{env, fs};

use std::str;

use rand::seq::SliceRandom;
use rand::Rng;
use serde_json::{Map, Value};
use std::error::Error;
use std::path::{Path, PathBuf};

pub mod configuration;
pub mod logger;
pub mod logging;
pub mod manifest;
pub mod offsets;
pub mod progress;
pub mod s3;
pub mod timed_rwlock;

// let CLEAN_FIELD_CACHE = Arc::new(Mutex::new(HashMap<String, bool> = HashMap::new()));

use crate::discover::date_formats::DateFormats;
use crate::discover::Metadata;
use crate::helpers::configuration::Config;
use dashmap::DashMap;
use once_cell::sync::Lazy;
use std::sync::Arc;
use walkdir::WalkDir;

// static CLEAN_FIELD_CACHE: Lazy<Mutex<i64>> = Lazy::new(|| Mutex::new(1));
// static CLEAN_FIELD_CACHE: Lazy<TimedRwLock<DashMap<String, String>>> =
//     Lazy::new(|| TimedRwLock::new("clean_field_cache".to_string(), DashMap::new()));
static CLEAN_FIELD_CACHE: Lazy<Arc<DashMap<String, String>>> =
    Lazy::new(|| Arc::new(DashMap::new()));

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
            if Config::get_transform_flatten_events() {
                clean = "".to_string() + &field;
            } else {
                clean = "item_".to_string() + &field;
            }
        }

        clean = clean.to_lowercase();

        let re = Regex::new(r"[^_0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ]")
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
            let mut cache = CLEAN_FIELD_CACHE
                .entry(field.clone())
                .or_insert_with(|| "no".to_string());
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
        let mut _rng = rand::thread_rng();
        let alphabet = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890";
        let mut pass = String::new();
        let alpha_length = alphabet.len() - 1;
        for _ in 0..length {
            let n = _rng.gen_range(0..alpha_length);
            pass.push(alphabet.chars().nth(n).unwrap());
        }
        pass
    }

    pub fn random_str(length: usize) -> String {
        let mut _rng = rand::thread_rng();
        let alphabet: Vec<char> = "abcdefghijklmnopqrstuvwxyz".chars().collect();
        let mut pass = String::with_capacity(length);

        for _ in 0..length {
            let c = alphabet.choose(&mut _rng).unwrap();
            pass.push(*c);
        }

        // add timestamp to the end of the string
        let timestamp = Utc::now().timestamp_millis();
        pass.push_str(&timestamp.to_string());
        pass
    }

    pub fn flatten(
        json: &Value,
        metadata: &HashMap<String, Metadata>,
    ) -> Result<Value, Box<dyn Error>> {
        let mut result = Map::new();
        Self::flatten_internal(json, metadata, &mut result, "".to_string())?;
        Ok(Value::Object(result))
    }

    fn flatten_internal(
        json: &Value,
        metadata: &HashMap<String, Metadata>,
        result: &mut Map<String, Value>,
        field_path: String,
    ) -> Result<(), Box<dyn Error>> {
        thread_local! {
            static FIELD_CACHE: RefCell<HashMap<String, (String, String)>> = RefCell::new(HashMap::new());
        }

        // Main flattening logic for different JSON value types
        match json {
            Value::Object(map) => {
                for (key, value) in map {
                    // Skip null values
                    if value.is_null() {
                        continue;
                    }

                    // Construct the field path and cache key - use underscores consistently
                    let path = if field_path.is_empty() {
                        key.clone()
                    } else {
                        // Change from dot notation to underscore notation
                        format!("{}_{}", field_path, key)
                    };
                    let cache_key = format!("{}:{}", field_path, key);

                    // Look up the field metadata
                    let field_metadata =
                        Self::lookup_metadata_with_caching(metadata, key, &cache_key);

                    // Get the output field name for this field
                    let output_field_name = match &field_metadata {
                        Some((_, meta)) => {
                            // Use the metadata's out_field_name
                            if field_path.is_empty() {
                                meta.out_field_name.clone()
                            } else {
                                // For nested fields, combine the path with the output field name
                                format!("{}_{}", field_path, meta.out_field_name)
                            }
                        }
                        None => {
                            // No metadata found, use the original path
                            path.clone()
                        }
                    };

                    match field_metadata {
                        Some((_, meta)) => {
                            // Handle scalar values directly (optimization)
                            if Self::is_scalar_value(value) {
                                result.insert(output_field_name, value.clone());
                            } else {
                                // Recursively flatten complex types
                                Self::flatten_internal(
                                    value,
                                    &meta.fields,
                                    result,
                                    output_field_name,
                                )?;
                            }
                        }
                        None => {
                            // Field not found in metadata - still include it
                            if Self::is_scalar_value(value) {
                                result.insert(output_field_name, value.clone());
                            } else {
                                Self::flatten_internal(
                                    value,
                                    &HashMap::new(),
                                    result,
                                    output_field_name,
                                )?;
                            }
                        }
                    }
                }
            }
            Value::Array(array) => {
                // Only flatten arrays of scalar values
                if array.iter().all(Self::is_scalar_value) {
                    result.insert(field_path.clone(), json.clone());
                } else {
                    // For arrays of complex types, process each element
                    for (i, item) in array.iter().enumerate() {
                        // Use underscore notation
                        let path = format!("{}_{}", field_path, i);
                        Self::flatten_internal(item, metadata, result, path)?;
                    }
                }
            }
            // Handle scalar values
            _ => {
                if !field_path.is_empty() {
                    result.insert(field_path.clone(), json.clone());
                }
            }
        }
        Ok(())
    }

    // Helper function to lookup metadata with caching
    fn lookup_metadata_with_caching<'a>(
        metadata: &'a HashMap<String, Metadata>,
        key: &str,
        cache_key: &str,
    ) -> Option<(String, &'a Metadata)> {
        thread_local! {
            static FIELD_CACHE: RefCell<HashMap<String, (String, String)>> = RefCell::new(HashMap::new());
        }

        // Check if we have a cached lookup for this field
        let cached_result = FIELD_CACHE.with(|cache| cache.borrow().get(cache_key).cloned());

        if let Some((metadata_key, _)) = cached_result {
            // Try direct lookup with cached metadata key
            if let Some(meta) = metadata.get(&metadata_key) {
                return Some((metadata_key, meta));
            }
        }

        // First try direct lookup with the original key (Strategy 1)
        if let Some(meta) = metadata.get(key) {
            let owned_key = key.to_string();
            FIELD_CACHE.with(|cache| {
                let mut cache_ref = cache.borrow_mut();
                cache_ref.insert(cache_key.to_string(), (owned_key.clone(), key.to_string()));
            });
            return Some((owned_key, meta));
        }

        // Fallback: try to find by output field name (Strategy 3)
        let result = Metadata::get_metadata_by_out_field_name(metadata, key);

        // If we found a match, cache it for future lookups
        if let Some((found_key, found_meta)) = &result {
            let owned_key = found_key.to_string();
            FIELD_CACHE.with(|cache| {
                let mut cache_ref = cache.borrow_mut();
                cache_ref.insert(cache_key.to_string(), (owned_key.clone(), key.to_string()));
            });
            Some((owned_key, *found_meta))
        } else {
            None
        }
    }

    // Helper function to check if a value is a scalar (not an object or array)
    fn is_scalar_value(value: &Value) -> bool {
        !value.is_object() && !value.is_array()
    }

    // Adding dead_code attribute to silence warnings for unused function
    #[allow(dead_code)]
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

            for entity_field_dot in Config::get_transform_batch_partition_fields().split(',') {
                // strip whitespace
                let entity_field_dot = entity_field_dot.trim();

                // Skip empty field names
                if entity_field_dot.is_empty() {
                    continue;
                }

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
                clean_allowed_values.len() > 0
                    && !clean_allowed_values.contains(&clean_entity_value)
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

                for entity_field_dot in Config::get_transform_namespace_fields().split(',') {
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
                        if let Ok(numeric) = s.parse::<i64>() {
                            // Determine if seconds or milliseconds by magnitude
                            if Helpers::is_millisecond_timestamp(numeric) {
                                if numeric <= 999999999999999 && numeric >= -999999999999999 {
                                    return Some(numeric / 1000);
                                } else {
                                    continue;
                                }
                            } else {
                                return Some(numeric);
                            }
                        }

                        // Try various date formats using parse_date_from_string
                        for format in DateFormats::iterator() {
                            if let Ok(dt) = Helpers::parse_date_from_string(s, format.as_str()) {
                                return Some(dt.timestamp());
                            }
                        }
                    }

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
                    }
                    _ => continue,
                }
            }
        }

        None
    }

    #[inline(always)]
    pub fn parse_date_from_string(date_str: &str, format: &str) -> Result<DateTime<Utc>, String> {
        // Data-driven normalization and parsing that does not rely on the provided format
        // - Handles: space vs 'T', lowercase 'z', UTC/GMT tokens, comma fractions,
        //   non-colon offsets (+HHMM, +HH), Unicode minus, extra whitespace/quotes/BOM
        let normalized = Self::normalize_datetime_input(date_str);
        // Fast path for Z without fractional seconds with either 'T' or space separator
        if normalized.len() == 20 && normalized.ends_with('Z') {
            let sep = normalized.as_bytes()[10] as char;
            if sep == 'T' || sep == ' ' {
                if let Some(dt) = Self::fast_parse_z_no_millis(&normalized, sep) {
                    return Ok(dt);
                }
            }
        }
        // RFC3339/ISO8601 attempt on normalized input (covers Z and offsets with/without space)
        if normalized.contains('Z')
            || normalized.rfind('+').map(|i| i > 10).unwrap_or(false)
            || normalized.rfind('-').map(|i| i > 10).unwrap_or(false)
        {
            if let Some(date) = Self::slow_parse_rfc3339_utc(&normalized) {
                return Ok(date);
            }
        }
        // Fast paths for common Z formats without fractional seconds
        // %Y-%m-%dT%H:%M:%SZ and %Y-%m-%d %H:%M:%SZ
        if (format == "%Y-%m-%dT%H:%M:%SZ" || format == "%Y-%m-%d %H:%M:%SZ")
            && (date_str.len() == 20)
        {
            if let Some(dt) = Self::fast_parse_z_no_millis(date_str, format.as_bytes()[10] as char)
            {
                return Ok(dt);
            }
        }
        // First try RFC3339 parsing if this looks like an ISO8601 format (original string)
        if date_str.contains('T') && (date_str.contains('Z') || date_str.contains('+')) {
            if let Some(date) = Self::slow_parse_rfc3339_utc(date_str) {
                return Ok(date);
            }
        }

        // Try direct DateTime parsing with timezone
        if let Some(date) = Self::slow_parse_format_utc(date_str, format) {
            return Ok(date);
        }
        // Try as NaiveDateTime (for formats without timezone)
        if let Some(date) = Self::slow_parse_naive_dt(date_str, format) {
            return Ok(DateTime::<Utc>::from_naive_utc_and_offset(date, Utc));
        }
        // Try as NaiveDate (for date-only formats)
        if format.contains("%Y")
            && format.contains("%m")
            && format.contains("%d")
            && !format.contains("%H")
        {
            if let Some(date) = Self::slow_parse_naive_date(date_str, format) {
                return Ok(DateTime::<Utc>::from_naive_utc_and_offset(
                    date.and_hms_opt(0, 0, 0).unwrap_or_default(),
                    Utc,
                ));
            }
        }

        // If all parsing attempts fail
        Err(format!(
            "Could not parse date {} with format {}",
            date_str, format
        ))
    }

    #[inline(always)]
    pub fn parse_date_from_string_with_tz(
        date_str: &str,
        format: &str,
    ) -> Result<chrono::DateTime<FixedOffset>, String> {
        // Data-driven normalization and parsing that does not rely on the provided format
        let normalized = Self::normalize_datetime_input(date_str);
        // Fast path for Z without fractional seconds; promote to +00:00
        if normalized.len() == 20 && normalized.ends_with('Z') {
            let sep = normalized.as_bytes()[10] as char;
            if sep == 'T' || sep == ' ' {
                if let Some(dt) = Self::fast_parse_z_no_millis(&normalized, sep) {
                    return Ok(dt.with_timezone(&FixedOffset::east_opt(0).unwrap()));
                }
            }
        }
        // Prefer RFC3339 on normalized input (covers offsets and Z)
        if normalized.contains('Z')
            || normalized.rfind('+').map(|i| i > 10).unwrap_or(false)
            || normalized.rfind('-').map(|i| i > 10).unwrap_or(false)
        {
            if let Some(date) = Self::slow_parse_rfc3339_fixed(&normalized) {
                return Ok(date);
            }
        }
        // Fast paths for common offset-bearing formats without fractional seconds
        // %Y-%m-%dT%H:%M:%S%z and %Y-%m-%d %H:%M:%S%z where %z is like +HH:MM
        if (format == "%Y-%m-%dT%H:%M:%S%z" || format == "%Y-%m-%d %H:%M:%S%z")
            && (date_str.len() == 25)
        {
            if let Some(dt) =
                Self::fast_parse_offset_no_millis(date_str, format.as_bytes()[10] as char)
            {
                return Ok(dt);
            }
        }
        // Also support Z variant fast-path here by promoting to +00:00
        if (format == "%Y-%m-%dT%H:%M:%SZ" || format == "%Y-%m-%d %H:%M:%SZ")
            && date_str.len() == 20
        {
            if let Some(dt) = Self::fast_parse_z_no_millis(date_str, format.as_bytes()[10] as char)
            {
                return Ok(dt.with_timezone(&FixedOffset::east_opt(0).unwrap()));
            }
        }
        // Prefer RFC3339 if string indicates ISO style with offset/Z
        if date_str.contains('T')
            && (date_str.contains('Z')
                || date_str.contains('+')
                || date_str.rfind('-').map(|i| i > 10).unwrap_or(false))
        {
            if let Some(date) = Self::slow_parse_rfc3339_fixed(date_str) {
                return Ok(date);
            }
        }

        // Try direct parse with explicit timezone in format
        if let Some(date) = Self::slow_parse_from_format_fixed(date_str, format) {
            return Ok(date);
        }

        // Fallback: parse naive and assume UTC offset
        if let Some(naive) = Self::slow_parse_naive_dt(date_str, format) {
            let offset =
                FixedOffset::east_opt(0).ok_or_else(|| "Invalid zero offset".to_string())?;
            let fixed = offset
                .from_local_datetime(&naive)
                .single()
                .ok_or_else(|| "Ambiguous or nonexistent local time".to_string())?;
            return Ok(fixed);
        }

        // Date-only
        if format.contains("%Y")
            && format.contains("%m")
            && format.contains("%d")
            && !format.contains("%H")
        {
            if let Some(date) = Self::slow_parse_naive_date(date_str, format) {
                let naive = date.and_hms_opt(0, 0, 0).unwrap_or_default();
                let offset =
                    FixedOffset::east_opt(0).ok_or_else(|| "Invalid zero offset".to_string())?;
                let fixed = offset
                    .from_local_datetime(&naive)
                    .single()
                    .ok_or_else(|| "Ambiguous or nonexistent local time".to_string())?;
                return Ok(fixed);
            }
        }

        Err(format!(
            "Could not parse date {} with format {}",
            date_str, format
        ))
    }

    #[inline(always)]
    fn normalize_datetime_input(input: &str) -> String {
        // Trim whitespace
        let mut s = input.trim().to_string();
        // Strip wrapping quotes/backticks if present
        if (s.starts_with('"') && s.ends_with('"'))
            || (s.starts_with('\'') && s.ends_with('\''))
            || (s.starts_with('`') && s.ends_with('`'))
        {
            s = s[1..s.len() - 1].to_string();
        }
        // Remove BOM and zero-width space
        s = s.replace('\u{FEFF}', "").replace('\u{200B}', "");
        // Normalize Unicode minus to ASCII hyphen in offsets
        s = s.replace('−', "-");
        // Collapse multiple spaces
        if s.contains("  ") {
            let mut collapsed = String::with_capacity(s.len());
            let mut last_space = false;
            for ch in s.chars() {
                if ch.is_whitespace() {
                    if !last_space {
                        collapsed.push(' ');
                    }
                    last_space = true;
                } else {
                    collapsed.push(ch);
                    last_space = false;
                }
            }
            s = collapsed.trim().to_string();
        }
        let lower = s.to_ascii_lowercase();
        // Convert trailing UTC/GMT token to Z
        if lower.ends_with(" utc") || lower.ends_with(" gmt") {
            s.truncate(s.len() - 4);
            s.push('Z');
        }
        // Normalize lowercase trailing 'z' to 'Z'
        if s.ends_with('z') {
            s.pop();
            s.push('Z');
        }
        // Replace comma fractional separator with dot
        if s.contains(',') {
            s = s.replace(',', ".");
        }
        // If space separator between date and time, use 'T' to satisfy RFC3339
        if s.len() > 10 {
            let bytes = s.as_bytes();
            if bytes.len() > 10 && bytes[10] == b' ' {
                let mut chars: Vec<char> = s.chars().collect();
                chars[10] = 'T';
                s = chars.into_iter().collect();
            }
        }
        // Normalize offsets: +HHMM -> +HH:MM, +HH -> +HH:00
        // Find last '+' or '-' after position 10 (to avoid date hyphens)
        let mut last_sign_idx: Option<usize> = None;
        for (i, ch) in s.char_indices() {
            if i > 10 && (ch == '+' || ch == '-') {
                last_sign_idx = Some(i);
            }
        }
        if let Some(idx) = last_sign_idx {
            if !s.ends_with('Z') {
                let (_head, tail) = s.split_at(idx + 1);
                let digits: String = tail
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == ':')
                    .collect();
                if digits.chars().all(|c| c.is_ascii_digit()) {
                    if digits.len() == 2 {
                        // +HH -> +HH:00
                        s = format!(
                            "{}{}:00{}",
                            &s[..idx + 1],
                            digits,
                            &s[idx + 1 + digits.len()..]
                        );
                    } else if digits.len() == 4 {
                        // +HHMM -> +HH:MM
                        s = format!(
                            "{}{}:{}{}",
                            &s[..idx + 1],
                            &digits[0..2],
                            &digits[2..4],
                            &s[idx + 1 + digits.len()..]
                        );
                    }
                }
            }
        }
        // Truncate excessive fractional seconds to 6 for chrono compatibility
        if let Some(t_idx) = s.find('T') {
            // Look for a '.' following seconds
            if let Some(dot_idx) = s[t_idx..].find('.') {
                let abs_dot = t_idx + dot_idx;
                // Find end of fraction (before 'Z', '+', or '-')
                let mut end = abs_dot + 1;
                while end < s.len() {
                    let ch = s.as_bytes()[end] as char;
                    if ch.is_ascii_digit() {
                        end += 1;
                    } else {
                        break;
                    }
                }
                let frac_len = end - (abs_dot + 1);
                if frac_len > 9 {
                    // Truncate to 6 to keep parsing fast and sufficient precision
                    let keep = 6usize;
                    let mut new_s = String::with_capacity(s.len());
                    new_s.push_str(&s[..abs_dot + 1]);
                    new_s.push_str(&s[abs_dot + 1..abs_dot + 1 + keep]);
                    new_s.push_str(&s[end..]);
                    s = new_s;
                }
            }
        }
        s
    }

    pub fn apply_timezone_to_naive(
        datetime: NaiveDateTime,
        timezone: &str,
    ) -> Result<DateTime<Utc>, String> {
        // Try fixed offset like +01:00 or -0500
        if let Some(offset) = Helpers::parse_fixed_offset(timezone) {
            let dt = offset
                .from_local_datetime(&datetime)
                .single()
                .ok_or_else(|| "Ambiguous or nonexistent local time".to_string())?;
            return Ok(dt.with_timezone(&Utc));
        }
        // Try named timezone via chrono-tz
        match timezone.parse::<Tz>() {
            Ok(tz) => Ok(tz
                .from_local_datetime(&datetime)
                .single()
                .ok_or_else(|| {
                    format!(
                        "Ambiguous or nonexistent local time for timezone {}",
                        timezone
                    )
                })?
                .with_timezone(&Utc)),
            Err(_e) => Err(format!("Unknown timezone: {}", timezone)),
        }
    }

    #[inline(always)]
    fn parse_fixed_offset(s: &str) -> Option<FixedOffset> {
        let b = s.as_bytes();
        if b.len() == 6 && (b[0] == b'+' || b[0] == b'-') && b[3] == b':' {
            let sign = if b[0] == b'+' { 1 } else { -1 };
            let hh = (b[1] - b'0') as i32 * 10 + (b[2] - b'0') as i32;
            let mm = (b[4] - b'0') as i32 * 10 + (b[5] - b'0') as i32;
            return FixedOffset::east_opt(sign * (hh * 3600 + mm * 60));
        }
        if b.len() == 5 && (b[0] == b'+' || b[0] == b'-') {
            let sign = if b[0] == b'+' { 1 } else { -1 };
            let hh = (b[1] - b'0') as i32 * 10 + (b[2] - b'0') as i32;
            let mm = (b[3] - b'0') as i32 * 10 + (b[4] - b'0') as i32;
            return FixedOffset::east_opt(sign * (hh * 3600 + mm * 60));
        }
        None
    }

    #[inline(always)]
    fn parse_2digits(bytes: &[u8]) -> Option<u32> {
        if bytes.len() != 2 {
            return None;
        }
        let d0 = bytes[0].wrapping_sub(b'0');
        let d1 = bytes[1].wrapping_sub(b'0');
        if d0 > 9 || d1 > 9 {
            return None;
        }
        Some((d0 as u32) * 10 + (d1 as u32))
    }

    #[inline(always)]
    fn parse_4digits(bytes: &[u8]) -> Option<i32> {
        if bytes.len() != 4 {
            return None;
        }
        let mut v: i32 = 0;
        for &b in bytes {
            let d = b.wrapping_sub(b'0');
            if d > 9 {
                return None;
            }
            v = v * 10 + (d as i32);
        }
        Some(v)
    }

    // Fast parse for YYYY-MM-DD{sep}HH:MM:SSZ (no millis)
    #[inline(always)]
    pub(crate) fn fast_parse_z_no_millis(s: &str, sep: char) -> Option<DateTime<Utc>> {
        let b = s.as_bytes();
        if b.len() != 20 {
            return None;
        }
        if b[4] != b'-' || b[7] != b'-' {
            return None;
        }
        if b[10] != sep as u8 {
            return None;
        }
        if b[13] != b':' || b[16] != b':' {
            return None;
        }
        if b[19] != b'Z' {
            return None;
        }
        let year = Self::parse_4digits(&b[0..4])?;
        let month = Self::parse_2digits(&b[5..7])? as u32;
        let day = Self::parse_2digits(&b[8..10])? as u32;
        let hour = Self::parse_2digits(&b[11..13])? as u32;
        let min = Self::parse_2digits(&b[14..16])? as u32;
        let sec = Self::parse_2digits(&b[17..19])? as u32;
        let date = chrono::NaiveDate::from_ymd_opt(year, month, day)?;
        let naive = date.and_hms_opt(hour, min, sec)?;
        Some(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
    }

    // Fast parse for YYYY-MM-DD{sep}HH:MM:SS±HH:MM (no millis)
    #[inline(always)]
    pub(crate) fn fast_parse_offset_no_millis(
        s: &str,
        sep: char,
    ) -> Option<chrono::DateTime<FixedOffset>> {
        let b = s.as_bytes();
        if b.len() != 25 {
            return None;
        }
        if b[4] != b'-' || b[7] != b'-' {
            return None;
        }
        if b[10] != sep as u8 {
            return None;
        }
        if b[13] != b':' || b[16] != b':' {
            return None;
        }
        let sign = match b[19] {
            b'+' => 1i32,
            b'-' => -1i32,
            _ => return None,
        };
        if b[22] != b':' {
            return None;
        }
        let year = Self::parse_4digits(&b[0..4])?;
        let month = Self::parse_2digits(&b[5..7])? as u32;
        let day = Self::parse_2digits(&b[8..10])? as u32;
        let hour = Self::parse_2digits(&b[11..13])? as u32;
        let min = Self::parse_2digits(&b[14..16])? as u32;
        let sec = Self::parse_2digits(&b[17..19])? as u32;
        let off_h = Self::parse_2digits(&b[20..22])? as i32;
        let off_m = Self::parse_2digits(&b[23..25])? as i32;
        let offset_secs = sign * (off_h * 3600 + off_m * 60);
        let date = chrono::NaiveDate::from_ymd_opt(year, month, day)?;
        let naive = date.and_hms_opt(hour, min, sec)?;
        let offset = FixedOffset::east_opt(offset_secs)?;
        let dt = offset.from_local_datetime(&naive).single()?;
        Some(dt)
    }

    #[cold]
    pub(crate) fn slow_parse_rfc3339_utc(s: &str) -> Option<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|d| DateTime::<Utc>::from(d))
    }

    #[cold]
    pub(crate) fn slow_parse_rfc3339_fixed(s: &str) -> Option<chrono::DateTime<FixedOffset>> {
        chrono::DateTime::parse_from_rfc3339(s).ok()
    }

    #[cold]
    pub(crate) fn slow_parse_format_utc(s: &str, fmt: &str) -> Option<DateTime<Utc>> {
        DateTime::parse_from_str(s, fmt)
            .ok()
            .map(|d| DateTime::<Utc>::from(d))
    }

    #[cold]
    pub(crate) fn slow_parse_from_format_fixed(
        s: &str,
        fmt: &str,
    ) -> Option<chrono::DateTime<FixedOffset>> {
        chrono::DateTime::parse_from_str(s, fmt).ok()
    }

    #[cold]
    pub(crate) fn slow_parse_naive_dt(s: &str, fmt: &str) -> Option<NaiveDateTime> {
        NaiveDateTime::parse_from_str(s, fmt).ok()
    }

    #[cold]
    pub(crate) fn slow_parse_naive_date(s: &str, fmt: &str) -> Option<NaiveDate> {
        NaiveDate::parse_from_str(s, fmt).ok()
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
        values
            .iter()
            .map(|value| {
                let current_value = Helpers::get_nested_value_from_dot_notation(value, field_str);
                match current_value {
                    // Some(Value::Array(_)) => current_value.clone(),
                    // Some(Value::Object(_)) => current_value.clone(),
                    _ => current_value.clone(),
                }
            })
            .collect()
    }

    // Adding dead_code attribute to silence warnings for unused function
    #[allow(dead_code)]
    fn list_dir_recursively_with_size(
        start_dir: &PathBuf,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Use a HashMap to keep track of directory sizes
        let mut dir_sizes: std::collections::HashMap<PathBuf, u64> =
            std::collections::HashMap::new();

        // Walk through the directory recursively
        for entry in WalkDir::new(start_dir).into_iter().filter_map(|e| e.ok()) {
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
            println!(
                "{}:\t{}",
                dir.display(),
                Helpers::human_readable_size(*size)
            );
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
    use super::*;
    #[allow(unused_imports)]
    use chrono::{DateTime, TimeZone, Utc};

    #[test]
    fn it_parses_datetime_with_positive_offset_to_utc() {
        let parsed =
            Helpers::parse_date_from_string("2022-02-22T22:22:22+01:00", "%Y-%m-%dT%H:%M:%S%z")
                .unwrap();
        let expected = Utc.with_ymd_and_hms(2022, 2, 22, 21, 22, 22).unwrap(); // Adjusted to UTC
        assert_eq!(parsed, expected);
    }

    #[test]
    fn it_parses_datetime_with_negative_offset_to_utc() {
        let parsed =
            Helpers::parse_date_from_string("2022-02-22T22:22:22-01:00", "%Y-%m-%dT%H:%M:%S%z")
                .unwrap();
        let expected = Utc.with_ymd_and_hms(2022, 2, 22, 23, 22, 22).unwrap(); // Adjusted to UTC
        assert_eq!(parsed, expected);
    }

    #[test]
    fn it_handles_incorrect_format_gracefully() {
        let result = Helpers::parse_date_from_string("2022-02-22T22:22:22", "%Y-%m-%dT%H:%M:%S%z");
        assert!(result.is_err());
    }

    #[test]
    fn it_parses_naive_datetime_to_utc() {
        let parsed =
            Helpers::parse_date_from_string("2022-02-22T22:22:22", "%Y-%m-%dT%H:%M:%S").unwrap();
        let expected = Utc.with_ymd_and_hms(2022, 2, 22, 22, 22, 22).unwrap(); // Assumed to already be in UTC
        assert_eq!(parsed, expected);
    }
}

#[cfg(test)]
mod parse_date_normalization_tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn parses_space_z_without_fraction() {
        let dt =
            Helpers::parse_date_from_string_with_tz("2025-09-25 14:31:23Z", "%Y-%m-%d %H:%M:%SZ")
                .unwrap();
        let expected = chrono::Utc
            .with_ymd_and_hms(2025, 9, 25, 14, 31, 23)
            .unwrap();
        assert_eq!(dt.with_timezone(&chrono::Utc), expected);
    }

    #[test]
    fn parses_space_z_with_fraction() {
        let dt = Helpers::parse_date_from_string_with_tz(
            "2025-09-25 14:31:23.123Z",
            "%Y-%m-%d %H:%M:%S.%fZ",
        )
        .unwrap();
        let expected = chrono::Utc
            .with_ymd_and_hms(2025, 9, 25, 14, 31, 23)
            .unwrap();
        assert_eq!(
            dt.with_timezone(&chrono::Utc).timestamp(),
            expected.timestamp()
        );
    }

    #[test]
    fn parses_offset_no_colon() {
        let dt = Helpers::parse_date_from_string_with_tz(
            "2025-09-25 14:31:23+0000",
            "%Y-%m-%d %H:%M:%S%z",
        )
        .unwrap();
        let expected = chrono::Utc
            .with_ymd_and_hms(2025, 9, 25, 14, 31, 23)
            .unwrap();
        assert_eq!(dt.with_timezone(&chrono::Utc), expected);
    }

    #[test]
    fn parses_offset_with_colon() {
        let dt = Helpers::parse_date_from_string_with_tz(
            "2025-09-25 14:31:23+00:00",
            "%Y-%m-%d %H:%M:%S%z",
        )
        .unwrap();
        let expected = chrono::Utc
            .with_ymd_and_hms(2025, 9, 25, 14, 31, 23)
            .unwrap();
        assert_eq!(dt.with_timezone(&chrono::Utc), expected);
    }

    #[test]
    fn parses_with_utc_token() {
        let dt =
            Helpers::parse_date_from_string_with_tz("2025-09-25 14:31:23 UTC", "%Y-%m-%d %H:%M:%S")
                .unwrap();
        let expected = chrono::Utc
            .with_ymd_and_hms(2025, 9, 25, 14, 31, 23)
            .unwrap();
        assert_eq!(dt.with_timezone(&chrono::Utc), expected);
    }

    #[test]
    fn parses_comma_fraction_z() {
        let dt = Helpers::parse_date_from_string_with_tz(
            "2025-09-25 14:31:23,123Z",
            "%Y-%m-%d %H:%M:%S.%fZ",
        )
        .unwrap();
        let expected = chrono::Utc
            .with_ymd_and_hms(2025, 9, 25, 14, 31, 23)
            .unwrap();
        assert_eq!(
            dt.with_timezone(&chrono::Utc).timestamp(),
            expected.timestamp()
        );
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
    use serial_test::serial;

    #[test]
    #[serial]
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
    #[serial]
    fn test_parse_time_field_with_valid_millisecond_timestamp() {
        Config::setenv("TRANSFORM_BATCH_TIME_FIELDS", "time1");

        let time = 1646901960000i64; // Valid millisecond timestamp
        let message = json!({ "time1": time });

        assert_eq!(Helpers::parse_time_field(&message), Some(1646901960));
    }

    #[test]
    #[serial]
    fn test_parse_time_field_with_valid_second_timestamp() {
        Config::setenv("TRANSFORM_BATCH_TIME_FIELDS", "time3");

        let time = 1646901960i64; // Valid second timestamp
        let message = json!({ "time3": time });

        assert_eq!(Helpers::parse_time_field(&message), Some(time));
    }

    #[test]
    #[serial]
    fn test_parse_time_field_with_empty_config() {
        Config::setenv("TRANSFORM_BATCH_TIME_FIELDS", "");

        let message = json!({
            "time": 1646901960
        });

        assert_eq!(Helpers::parse_time_field(&message), None);
    }

    #[test]
    #[serial]
    fn test_parse_time_field_with_string_timestamp() {
        Config::setenv("TRANSFORM_BATCH_TIME_FIELDS", "time");

        let message = json!({
            "time": "2022-03-10T12:00:00Z"
        });

        assert_eq!(Helpers::parse_time_field(&message), Some(1646913600));
    }

    #[test]
    #[serial]
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
    #[allow(unused_imports)]
    use crate::ingest_work::PARTITION_ALLOWED_VALUES_CACHE;
    use serde_json::json;
    use serial_test::serial;

    #[test]
    #[serial]
    fn test_parse_partition_field_no_config() {
        let message = json!({"foo": "bar", "abc1": "def"});
        Config::set_evncache("TRANSFORM_BATCH_PARTITION_FIELDS", "");
        Config::set_evncache("TRANSFORM_PARTITION_ALLOWED_VALUES", "bar");

        let allowed_values = Config::get_partition_allowed_values();
        let allowed_values_vec: HashSet<String> =
            allowed_values.split(',').map(|s| s.to_string()).collect();

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
        let allowed_values_vec: HashSet<String> =
            allowed_values.split(',').map(|s| s.to_string()).collect();

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
        let allowed_values_vec: HashSet<String> =
            allowed_values.split(',').map(|s| s.to_string()).collect();

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
        let allowed_values_vec: HashSet<String> =
            allowed_values.split(',').map(|s| s.to_string()).collect();

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
        let allowed_values_vec: HashSet<String> =
            allowed_values.split(',').map(|s| s.to_string()).collect();

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
        let allowed_values_vec: HashSet<String> = allowed_values
            .split(',')
            .map(|s| Helpers::clean_field_name(s.to_string()))
            .collect();

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
        let allowed_values_vec: HashSet<String> =
            allowed_values.split(',').map(|s| s.to_string()).collect();

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
        let allowed_values_vec: HashSet<String> =
            allowed_values.split(',').map(|s| s.to_string()).collect();

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
        let allowed_values_vec: HashSet<String> =
            allowed_values.split(',').map(|s| s.to_string()).collect();

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
        let allowed_values_vec: HashSet<String> =
            allowed_values.split(',').map(|s| s.to_string()).collect();

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
        let allowed_values_vec: HashSet<String> =
            allowed_values.split(',').map(|s| s.to_string()).collect();

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
        let allowed_values_vec: HashSet<String> =
            allowed_values.split(',').map(|s| s.to_string()).collect();

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

        let metadata = Metadata::new().unwrap();
        let mut metadata_hashmap = HashMap::new();
        metadata_hashmap.insert("field".to_string(), metadata);

        let flattened = Helpers::flatten(&json, &metadata_hashmap);
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
        let mut metadata: HashMap<String, Metadata> = HashMap::new();
        metadata.insert(
            "field".into(),
            Metadata {
                count: 1,
                types: HashMap::new(),
                parent_type: "".into(),
                fields: Box::new(HashMap::new()),
                date_candidate: None,
                date_parser_kind: None,
                timezone: false,
                evolution: Box::new(HashMap::new()),
                enabled: true,
                out_field_name: "field".into(),
                determined_type: "string".into(),
                determined_type_values: "".into(),
                repetition_count: 1,
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
                date_parser_kind: None,
                timezone: false,
                evolution: Box::new(HashMap::new()),
                enabled: true,
                out_field_name: "contact".into(),
                determined_type: "record".into(),
                determined_type_values: "".into(),
                repetition_count: 1,
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
                date_parser_kind: None,
                timezone: false,
                evolution: Box::new(HashMap::new()),
                enabled: true,
                out_field_name: "name".into(),
                determined_type: "string".into(),
                determined_type_values: "".into(),
                repetition_count: 1,
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
                date_parser_kind: None,
                timezone: false,
                evolution: Box::new(HashMap::new()),
                enabled: true,
                out_field_name: "tel".into(),
                determined_type: "int".into(),
                determined_type_values: "".into(),
                repetition_count: 1,
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
        let mut metadata: HashMap<String, Metadata> = HashMap::new();
        metadata.insert(
            "field".into(),
            Metadata {
                count: 1,
                types: HashMap::new(),
                parent_type: "".into(),
                fields: Box::new(HashMap::new()),
                date_candidate: None,
                date_parser_kind: None,
                timezone: false,
                evolution: Box::new(HashMap::new()),
                enabled: true,
                out_field_name: "field".into(),
                determined_type: "string".into(),
                determined_type_values: "".into(),
                repetition_count: 1,
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
                date_parser_kind: None,
                timezone: false,
                evolution: Box::new(HashMap::new()),
                enabled: true,
                out_field_name: "contacts".into(),
                determined_type: "array".into(),
                determined_type_values: "".into(),
                repetition_count: 1,
            },
        );
        metadata.get_mut("contacts").unwrap().fields.insert(
            "0".into(),
            Metadata {
                count: 2,
                types: HashMap::new(),
                parent_type: "".into(),
                fields: Box::new(HashMap::new()),
                date_candidate: None,
                date_parser_kind: None,
                timezone: false,
                evolution: Box::new(HashMap::new()),
                enabled: true,
                out_field_name: "0".into(),
                determined_type: "string".into(),
                determined_type_values: "".into(),
                repetition_count: 1,
            },
        );
        metadata
            .get_mut("contacts")
            .unwrap()
            .fields
            .get_mut("0")
            .unwrap()
            .fields
            .insert(
                "name".into(),
                Metadata {
                    count: 2,
                    types: HashMap::new(),
                    parent_type: "".into(),
                    fields: Box::new(HashMap::new()),
                    date_candidate: None,
                    date_parser_kind: None,
                    timezone: false,
                    evolution: Box::new(HashMap::new()),
                    enabled: true,
                    out_field_name: "name".into(),
                    determined_type: "string".into(),
                    determined_type_values: "".into(),
                    repetition_count: 1,
                },
            );
        metadata
            .get_mut("contacts")
            .unwrap()
            .fields
            .get_mut("0")
            .unwrap()
            .fields
            .insert(
                "tel".into(),
                Metadata {
                    count: 2,
                    types: HashMap::new(),
                    parent_type: "".into(),
                    fields: Box::new(HashMap::new()),
                    date_candidate: None,
                    date_parser_kind: None,
                    timezone: false,
                    evolution: Box::new(HashMap::new()),
                    enabled: true,
                    out_field_name: "tel".into(),
                    determined_type: "int".into(),
                    determined_type_values: "".into(),
                    repetition_count: 1,
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

#[cfg(test)]
mod tests_flatten_special_cases {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_flatten_special_chars_and_case() {
        // Setup metadata for test
        let mut metadata = HashMap::new();

        // Case for uppercase field (IMEI)
        let mut imei_metadata = Metadata::new().unwrap();
        imei_metadata.out_field_name = "imei".to_string();
        imei_metadata.determined_type = "long".to_string();
        metadata.insert("IMEI".to_string(), imei_metadata);

        // Case for special characters (preasure\bar)
        let mut pressure_metadata = Metadata::new().unwrap();
        pressure_metadata.out_field_name = "preasure_bar".to_string();
        pressure_metadata.determined_type = "double".to_string();
        metadata.insert("preasure\\bar".to_string(), pressure_metadata);

        // Create test JSON with both cases - using both original and transformed field names
        // to test both code paths
        let json_value = json!({
            "IMEI": 12345678901i64,             // Original case (using i64 for large numbers)
            "preasure_bar": 98.6                 // Already transformed name
        });

        // Test flattening
        let result = Helpers::flatten(&json_value, &metadata).unwrap();

        // Verify results - we should find both fields with their transformed names
        assert!(
            result.as_object().unwrap().contains_key("imei"),
            "Field 'imei' missing from flattened result"
        );
        assert!(
            result.as_object().unwrap().contains_key("preasure_bar"),
            "Field 'preasure_bar' missing from flattened result"
        );

        // Verify the values
        assert_eq!(result["imei"], json!(12345678901i64));
        assert_eq!(result["preasure_bar"], json!(98.6));

        // Test with lowercase field names to test case-insensitive matching
        let json_value2 = json!({
            "imei": 12345678901i64,             // Lowercase (using i64 for large numbers)
            "preasure\\bar": 98.6                // Original with backslash
        });

        let result2 = Helpers::flatten(&json_value2, &metadata).unwrap();

        // Verify both fields exist with correct values
        assert!(
            result2.as_object().unwrap().contains_key("imei"),
            "Field 'imei' missing from flattened result (lowercase test)"
        );
        assert!(
            result2.as_object().unwrap().contains_key("preasure_bar"),
            "Field 'preasure_bar' missing from flattened result (original name test)"
        );

        assert_eq!(result2["imei"], json!(12345678901i64));
        assert_eq!(result2["preasure_bar"], json!(98.6));
    }
}

#[cfg(test)]
mod tests_flatten_performance {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use std::time::Instant;

    #[test]
    fn test_flatten_performance_with_cache() {
        // Create metadata with test fields
        let mut metadata = HashMap::new();
        let mut field1 = Metadata::new().unwrap();
        field1.out_field_name = "imei".to_string();
        metadata.insert("IMEI".to_string(), field1);

        let mut field2 = Metadata::new().unwrap();
        field2.out_field_name = "pressure_bar".to_string();
        metadata.insert("preasure\\bar".to_string(), field2);

        // Create a complex test object that will be flattened
        let json = json!({
            "IMEI": 12345678901i64,
            "preasure\\bar": 32.5,
            "normal_field": "value",
            "nested": {
                "IMEI": 12345678901i64,
                "preasure\\bar": 45.2,
                "other_field": "other_value"
            }
        });

        // First flattening - should have cache misses
        let start = Instant::now();
        let flattened1 = Helpers::flatten(&json, &metadata).unwrap();
        let first_duration = start.elapsed();

        // Second flattening with the same data - should use cache
        let start = Instant::now();
        let flattened2 = Helpers::flatten(&json, &metadata).unwrap();
        let second_duration = start.elapsed();

        // Verify results are the same
        assert_eq!(flattened1, flattened2);

        // Print timing information for debugging
        println!("First flatten duration: {:?}", first_duration);
        println!("Second flatten duration: {:?}", second_duration);
        println!("Flattened result: {:#?}", flattened1);

        // Check for expected output field names
        // The original test was looking for "imei" and "pressure_bar"
        assert!(
            flattened1.as_object().unwrap().contains_key("imei"),
            "Field 'imei' missing from result"
        );
        assert!(
            flattened1.as_object().unwrap().contains_key("pressure_bar"),
            "Field 'pressure_bar' missing from result"
        );

        // We don't check for nested fields since metadata for them isn't provided
        // and they'll be flattened with default field paths

        // The second run should be faster due to caching, but don't make a hard assertion
        // since timing can vary based on system load, but typically it would be faster
        println!(
            "Speedup factor: {:.2}x",
            first_duration.as_nanos() as f64 / second_duration.as_nanos() as f64
        );
    }
}
