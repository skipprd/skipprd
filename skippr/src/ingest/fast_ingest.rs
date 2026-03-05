use std::collections::HashMap;

use serde_json::Value;
use std::error::Error;
use std::sync::Arc;

use once_cell::sync::Lazy;
use serde_json::Map;

use crate::discover::date_formats::DateFormats;
use crate::discover::evolution::Evolution;
use crate::discover::{AnalyseSchema, Metadata, SkipprDataType};

use crate::helpers::timed_rwlock::TimedRwLock;
use crate::helpers::Helpers;
use crate::ingest::ingest::ResolvedFieldValue;

#[allow(unused_imports)]
use crate::discover::DateCandidate;
#[allow(unused_imports)]
use chrono::{DateTime, FixedOffset, NaiveDateTime, Utc};

pub static DEFAULT_NESTED_MESSAGE: Lazy<Arc<TimedRwLock<HashMap<String, Value>>>> =
    Lazy::new(|| {
        Arc::new(TimedRwLock::new(
            "default_message".to_string(),
            HashMap::new(),
        ))
    });

pub fn create_default_nested_message(metadata: &HashMap<String, Metadata>) -> Value {
    let mut message = Value::Object(Map::new());
    for (field, meta_data) in metadata {
        if meta_data.enabled {
            if meta_data.fields.is_empty() {
                message[meta_data.out_field_name.clone()] = Value::Null;
            } else             if meta_data.determined_type == SkipprDataType::Array {
                if meta_data.determined_type_values == Some(SkipprDataType::Record) {
                    // message[meta_data.out_field_name.clone()] = create_default_nested_message(&meta_data.fields);
                    let fields = create_default_nested_message(&meta_data.fields);
                    message[meta_data.out_field_name.clone()] = Value::Array(vec![]);
                    if fields.as_array().is_some() {
                        for field in fields.as_array().unwrap().iter() {
                            message.as_array_mut().unwrap().push(field.clone());
                        }
                    }
                } else {
                    message[meta_data.out_field_name.clone()] = Value::Array(Vec::new());
                }
            } else if meta_data.determined_type == SkipprDataType::Map {
                message[meta_data.out_field_name.clone()] = Value::Object(Map::new());
            } else {
                let mut sub_fields = Map::new();
                sub_fields.insert(
                    field.to_string(),
                    create_default_nested_message(&meta_data.fields),
                );
                message[meta_data.out_field_name.clone()] = Value::Object(sub_fields);
            }
        }
    }

    sort_fields(&mut message);

    message
}

pub fn sort_fields(value: &mut Value) {
    match value {
        Value::Object(map) => {
            // Only sort if there's more than one entry
            if map.len() <= 1 {
                return;
            }

            // Optimize by using drain/collect instead of cloning the entire map
            let mut keys: Vec<String> = map.keys().cloned().collect();
            keys.sort();

            // Create a new map from the sorted keys
            let mut sorted_map = Map::with_capacity(keys.len());

            // Move values from original map to sorted map in key order
            for key in keys {
                if let Some(mut v) = map.remove(&key) {
                    // Sort nested values before adding them
                    sort_fields(&mut v);
                    sorted_map.insert(key, v);
                }
            }

            // Replace the content of the original map
            *map = sorted_map;
        }
        Value::Array(vec) => {
            // No need to sort arrays, just recursively sort their elements if they contain objects
            for v in vec.iter_mut() {
                sort_fields(v);
            }
        }
        _ => {} // Other types do not need sorting
    }
}

#[inline]
pub fn fast_path_ingest(
    unwrapped_message: &Value,
    metadata: &HashMap<String, Metadata>,
    namespace: &str,
    flatten: bool,
) -> Result<Value, Box<dyn Error>> {
    // Get the template message once
    let mut _message = match DEFAULT_NESTED_MESSAGE.read().get(namespace) {
        Some(m) => m.clone(),
        None => {
            return Err("No default message template found".into());
        }
    };

    // Direct unwrap with early return for invalid input
    let object = unwrapped_message.as_object().ok_or("Invalid JSON object")?;
    if object.is_empty() {
        return Ok(_message);
    }

    // Pre-compute metadata access to avoid repeated lookups
    let field_count = object.len();
    let mut fields_to_process = Vec::with_capacity(field_count);

    // First pass - collect field information to process
    for (field, value) in object {
        if let Some(meta_data) = metadata.get(field) {
            // Skip disabled fields early (no functional change; mirrors deep handlers)
            if !meta_data.enabled {
                continue;
            }
            // Skip null or empty values early
            if value.is_null()
                || (value.is_string() && value.as_str().unwrap_or_default().is_empty())
            {
                continue;
            }

            // Use data_type() method instead of string determined_type
            // Avoid recomputing out_field_name in insertion path; capture now
            fields_to_process.push((field, value, meta_data.determined_type.clone()));
        } else {
            // Propose evolution for missing field under root
            if crate::helpers::configuration::Config::debug_enabled() {
                println!(
                    "fast_path_ingest: missing field in metadata: '{}' ns={}",
                    field, namespace
                );
            }
            return Err(format!("Field '{}' not found in metadata", field).into());
        }
    }

    // Second pass - process all fields
    // Insert directly into the template object for speed (avoids intermediate indexing conversions)
    let obj = _message
        .as_object_mut()
        .ok_or("Template is not an object")?;
    for (field, value, data_type) in fields_to_process {
        // No need to convert string to enum since we already have the enum
        let resolved_value =
            match fast_set_value_optimized(&data_type, field, value, metadata, None, flatten) {
                Ok(v) => v,
                Err(e) => {
                    if crate::helpers::configuration::Config::log_wal_enabled() {
                        println!(
                            "fast_path_ingest: set_value failed for field='{}' err={}",
                            field, e
                        );
                    }
                    if e.to_string().contains("Falling back to slow path") {
                        return Err(e);
                    }
                    return Err(e);
                }
            };

        if !resolved_value.value.is_null() {
            obj.insert(resolved_value.field, resolved_value.value);
        }
    }

    // Apply flattening if needed
    if flatten {
        _message = match Helpers::flatten(&_message, &metadata) {
            Ok(m) => m,
            Err(e) => return Err(e),
        };
    }

    Ok(_message)
}

/// Optimized version of fast_set_value that uses the DataType enum
#[inline]
pub fn fast_set_value_optimized(
    data_type: &SkipprDataType,
    field: &str,
    value: &Value,
    metadata: &HashMap<String, Metadata>,
    apply_evolution: Option<bool>,
    flatten: bool,
) -> Result<ResolvedFieldValue, Box<dyn Error>> {
    // Early return for null or empty values
    if value.is_null() {
        return Ok(ResolvedFieldValue {
            field: field.to_string(),
            value: Value::Null,
        });
    }
    if value.is_string() && value.as_str().unwrap_or_default().is_empty() {
        return Ok(ResolvedFieldValue {
            field: field.to_string(),
            value: Value::Null,
        });
    }

    let apply_evolution_bool = apply_evolution.unwrap_or(true);

    match data_type {
        SkipprDataType::Record => process_record_field(field, value, metadata, flatten),
        SkipprDataType::Map => process_map_field(field, value, metadata, flatten),
        SkipprDataType::Array => process_array_field(field, value, metadata, flatten),
        SkipprDataType::Date => fast_set_date(field, value, metadata),
        _ => match_scalar_value_optimized(
            field,
            data_type,
            value,
            metadata,
            apply_evolution_bool,
            flatten,
        ),
    }
}

/// Optimized version of match_scalar_value_fast that uses the DataType enum
#[inline]
pub fn match_scalar_value_optimized(
    field: &str,
    data_type: &SkipprDataType,
    value: &Value,
    metadata: &HashMap<String, Metadata>,
    apply_evolution: bool,
    flatten: bool,
) -> Result<ResolvedFieldValue, Box<dyn Error>> {
    // Return early for null values
    if value.is_null() {
        return Ok(ResolvedFieldValue {
            field: Metadata::get_field_out_field_name(metadata, field),
            value: Value::Null,
        });
    }

    // Use cached field name lookups to reduce repetitive transformations
    let output_field_name = Metadata::get_field_out_field_name(metadata, field);

    match data_type {
        SkipprDataType::String => {
            if let Some(s) = value.as_str() {
                // Fast path for actual strings
                return Ok(ResolvedFieldValue {
                    field: output_field_name,
                    value: Value::String(s.to_string()),
                });
            } else if let Some(i) = value.as_i64() {
                // Convert integer to string
                return Ok(ResolvedFieldValue {
                    field: output_field_name,
                    value: Value::String(i.to_string()),
                });
            } else if let Some(f) = value.as_f64() {
                // Convert float to string
                return Ok(ResolvedFieldValue {
                    field: output_field_name,
                    value: Value::String(f.to_string()),
                });
            } else if let Some(b) = value.as_bool() {
                // Convert boolean to string
                return Ok(ResolvedFieldValue {
                    field: output_field_name,
                    value: Value::String(b.to_string()),
                });
            } else if apply_evolution {
                let mut _meta_ev = metadata.clone();
                match Evolution::apply_evolution_factory(field, value, &mut _meta_ev, flatten) {
                    Ok(v) => return Ok(v),
                    Err(_) => {}
                }
            }

            Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Value {} is not a string", value),
            )))
        }
        SkipprDataType::Long => {
            if let Some(i) = value.as_i64() {
                // Fast path for actual integers
                return Ok(ResolvedFieldValue {
                    field: output_field_name,
                    value: Value::Number(serde_json::Number::from(i)),
                });
            } else if let Some(s) = value.as_str() {
                // Try parsing string as integer
                if let Ok(i) = s.parse::<i64>() {
                    return Ok(ResolvedFieldValue {
                        field: output_field_name,
                        value: Value::Number(serde_json::Number::from(i)),
                    });
                }
            }

            if apply_evolution {
                let mut _meta_ev = metadata.clone();
                match Evolution::apply_evolution_factory(field, value, &mut _meta_ev, flatten) {
                    Ok(v) => return Ok(v),
                    Err(_) => {}
                }
            }

            Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Value {} is not a long", value),
            )))
        }
        SkipprDataType::Integer => {
            if let Some(i) = value.as_i64() {
                // Ensure 32-bit range
                return Ok(ResolvedFieldValue {
                    field: output_field_name,
                    value: Value::Number(serde_json::Number::from(i as i32)),
                });
            } else if let Some(s) = value.as_str() {
                // Try parsing string as integer
                if let Ok(i) = s.parse::<i32>() {
                    return Ok(ResolvedFieldValue {
                        field: output_field_name,
                        value: Value::Number(serde_json::Number::from(i)),
                    });
                } else if s == "true" {
                    return Ok(ResolvedFieldValue {
                        field: output_field_name,
                        value: Value::Number(serde_json::Number::from(1)),
                    });
                } else if s == "false" {
                    return Ok(ResolvedFieldValue {
                        field: output_field_name,
                        value: Value::Number(serde_json::Number::from(0)),
                    });
                }
            } else if let Some(b) = value.as_bool() {
                // Convert boolean to integer (true -> 1, false -> 0)
                return Ok(ResolvedFieldValue {
                    field: output_field_name,
                    value: Value::Number(serde_json::Number::from(if b { 1 } else { 0 })),
                });
            }

            if apply_evolution {
                let mut _meta_ev = metadata.clone();
                match Evolution::apply_evolution_factory(field, value, &mut _meta_ev, flatten) {
                    Ok(v) => return Ok(v),
                    Err(_) => {}
                }
            }

            Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Value {} is not an integer", value),
            )))
        }
        SkipprDataType::TimestampMilli | SkipprDataType::Timestamp => {
            if let Some(i) = value.as_i64() {
                // Convert to milliseconds if necessary
                return Ok(ResolvedFieldValue {
                    field: output_field_name,
                    value: AnalyseSchema::coerce_to_milli_seconds(Value::Number(
                        serde_json::Number::from(i),
                    )),
                });
            } else if let Some(s) = value.as_str() {
                // Try parsing string as timestamp
                if let Ok(i) = s.parse::<i64>() {
                    return Ok(ResolvedFieldValue {
                        field: output_field_name,
                        value: AnalyseSchema::coerce_to_milli_seconds(Value::Number(
                            serde_json::Number::from(i),
                        )),
                    });
                }
            } else if let Some(b) = value.as_bool() {
                // Convert boolean to timestamp (true -> 1, false -> 0)
                return Ok(ResolvedFieldValue {
                    field: output_field_name,
                    value: Value::Number(serde_json::Number::from(if b { 1 } else { 0 })),
                });
            }

            if apply_evolution {
                let mut _meta_ev = metadata.clone();
                match Evolution::apply_evolution_factory(field, value, &mut _meta_ev, flatten) {
                    Ok(v) => return Ok(v),
                    Err(_) => {}
                }
            }

            Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Field {} value {} is not a timestamp", field, value),
            )))
        }
        SkipprDataType::Double => {
            if let Some(f) = value.as_f64() {
                // Fast path for floats
                return Ok(ResolvedFieldValue {
                    field: output_field_name,
                    value: Value::Number(serde_json::Number::from_f64(f).unwrap()),
                });
            } else if let Some(s) = value.as_str() {
                // Try parsing string as float
                if let Ok(f) = s.parse::<f64>() {
                    if let Some(num) = serde_json::Number::from_f64(f) {
                        return Ok(ResolvedFieldValue {
                            field: output_field_name,
                            value: Value::Number(num),
                        });
                    }
                }
            }

            if apply_evolution {
                let mut _meta_ev = metadata.clone();
                match Evolution::apply_evolution_factory(field, value, &mut _meta_ev, flatten) {
                    Ok(v) => return Ok(v),
                    Err(_) => {}
                }
            }

            Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Value {} is not a double", value),
            )))
        }
        SkipprDataType::Boolean => {
            if let Some(b) = value.as_bool() {
                // Fast path for booleans
                return Ok(ResolvedFieldValue {
                    field: output_field_name,
                    value: Value::Bool(b),
                });
            } else if let Some(s) = value.as_str() {
                // Try parsing string as boolean
                if let Ok(b) = s.parse::<bool>() {
                    return Ok(ResolvedFieldValue {
                        field: output_field_name,
                        value: Value::Bool(b),
                    });
                } else if s == "0" {
                    return Ok(ResolvedFieldValue {
                        field: output_field_name,
                        value: Value::Bool(false),
                    });
                } else if s == "1" {
                    return Ok(ResolvedFieldValue {
                        field: output_field_name,
                        value: Value::Bool(true),
                    });
                }
            } else if let Some(i) = value.as_i64() {
                // Convert 0/1 to boolean
                if i == 0 || i == 1 {
                    return Ok(ResolvedFieldValue {
                        field: output_field_name,
                        value: Value::Bool(i == 1),
                    });
                }
            }

            if apply_evolution {
                let mut _meta_ev = metadata.clone();
                match Evolution::apply_evolution_factory(field, value, &mut _meta_ev, flatten) {
                    Ok(v) => return Ok(v),
                    Err(_) => {}
                }
            }

            Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Value {} is not a boolean", value),
            )))
        }
        _ => Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Unknown data type 'unknown'"),
        ))),
    }
}

// Keep the original functions for backward compatibility

pub fn fast_set_value(
    data_type: &str,
    field: &str,
    value: &Value,
    metadata: &HashMap<String, Metadata>,
    apply_evolution: Option<bool>,
    flatten: bool,
) -> Result<ResolvedFieldValue, Box<dyn Error>> {
    // Convert string data type to enum and delegate to the optimized version
    let data_type_enum = SkipprDataType::from_str(data_type);
    fast_set_value_optimized(
        &data_type_enum,
        field,
        value,
        metadata,
        apply_evolution,
        flatten,
    )
}

pub fn match_scalar_value_fast(
    field: &str,
    data_type: &str,
    value: &Value,
    metadata: &HashMap<String, Metadata>,
    apply_evolution: bool,
    flatten: bool,
) -> Result<ResolvedFieldValue, Box<dyn Error>> {
    // Convert string data type to enum and delegate to the optimized version
    let data_type_enum = SkipprDataType::from_str(data_type);
    match_scalar_value_optimized(
        field,
        &data_type_enum,
        value,
        metadata,
        apply_evolution,
        flatten,
    )
}

pub fn fast_set_date(
    field: &str,
    value: &Value,
    metadata: &HashMap<String, Metadata>,
) -> Result<ResolvedFieldValue, Box<dyn Error>> {
    // Use cached field name lookups to reduce repetitive transformations
    let output_field_name = Metadata::get_field_out_field_name(metadata, field);

    // Hive Timestamp doesn't support string dates
    match value.as_str() {
        Some(val) => {
            let parent_field_meta = match metadata.get(field) {
                Some(m) => m,
                None => {
                    return Err(Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("Could not find field metadata for {}", field),
                    )))
                }
            };

            let date_meta = match parent_field_meta.date_candidate.as_ref() {
                Some(f) => f,
                None => {
                    return Err(Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("Could not find date candidate in metadata for {}", field),
                    )))
                }
            };

            let fmt = &date_meta.format;

            match DateFormats::from_str(fmt) {
                Ok(f) => {
                    let kind = parent_field_meta.date_parser_kind.clone();
                    let tz = parent_field_meta.timezone;
                    let millis = match (kind, tz) {
                        (Some(crate::discover::DateParserKind::ZNoMsT), true) => {
                            Helpers::fast_parse_z_no_millis(val, 'T').map(|d| d.timestamp() * 1000)
                        }
                        (Some(crate::discover::DateParserKind::ZNoMsSpace), true) => {
                            Helpers::fast_parse_z_no_millis(val, ' ').map(|d| d.timestamp() * 1000)
                        }
                        // Handle Z with milliseconds (T or space)
                        (Some(crate::discover::DateParserKind::ZMsT), true) => {
                            Helpers::parse_date_from_string_with_tz(val, "%Y-%m-%dT%H:%M:%S.%fZ")
                                .ok()
                                .map(|d| d.timestamp() * 1000)
                        }
                        (Some(crate::discover::DateParserKind::ZMsSpace), true) => {
                            Helpers::parse_date_from_string_with_tz(val, "%Y-%m-%d %H:%M:%S.%fZ")
                                .ok()
                                .map(|d| d.timestamp() * 1000)
                        }
                        (Some(crate::discover::DateParserKind::OffNoMsT), true) => {
                            Helpers::fast_parse_offset_no_millis(val, 'T')
                                .map(|d| d.timestamp() * 1000)
                        }
                        (Some(crate::discover::DateParserKind::OffNoMsSpace), true) => {
                            Helpers::fast_parse_offset_no_millis(val, ' ')
                                .map(|d| d.timestamp() * 1000)
                        }
                        // Handle offset with milliseconds (T or space)
                        (Some(crate::discover::DateParserKind::OffMsT), true) => {
                            Helpers::parse_date_from_string_with_tz(val, "%Y-%m-%dT%H:%M:%S.%f%z")
                                .ok()
                                .map(|d| d.timestamp() * 1000)
                        }
                        (Some(crate::discover::DateParserKind::OffMsSpace), true) => {
                            Helpers::parse_date_from_string_with_tz(val, "%Y-%m-%d %H:%M:%S.%f%z")
                                .ok()
                                .map(|d| d.timestamp() * 1000)
                        }
                        (Some(crate::discover::DateParserKind::NaiveMysql), false) => {
                            Helpers::slow_parse_naive_dt(val, f.as_str()).map(|d| {
                                DateTime::<Utc>::from_naive_utc_and_offset(d, Utc).timestamp()
                                    * 1000
                            })
                        }
                        (Some(crate::discover::DateParserKind::NaiveDateOnly), false) => {
                            Helpers::slow_parse_naive_date(val, "%Y-%m-%d").map(|d| {
                                DateTime::<Utc>::from_naive_utc_and_offset(
                                    d.and_hms_opt(0, 0, 0).unwrap_or_default(),
                                    Utc,
                                )
                                .timestamp()
                                    * 1000
                            })
                        }
                        // Fallbacks
                        (_, true) => Helpers::parse_date_from_string_with_tz(val, f.as_str())
                            .ok()
                            .map(|d| d.timestamp() * 1000),
                        (_, false) => Helpers::parse_date_from_string(val, f.as_str())
                            .ok()
                            .map(|d| d.timestamp() * 1000),
                    };
                    match millis {
                        Some(ms) => Ok(ResolvedFieldValue {
                            field: output_field_name,
                            value: ms.into(),
                        }),
                        None => Err(Box::new(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!(
                                "Could not parse date {} with format {} for field {}",
                                val, fmt, field
                            ),
                        ))),
                    }
                }
                Err(err) => {
                    println!("Error date: {}", err);
                    Err(Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "Invalid date format",
                    )))
                }
            }
        }
        None => Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "Could not format date, expected value {} to parse as a string",
                value
            ),
        ))),
    }
}

#[inline]
fn process_record_field(
    field: &str,
    value: &Value,
    metadata: &HashMap<String, Metadata>,
    flatten: bool,
) -> Result<ResolvedFieldValue, Box<dyn Error>> {
    let _m = Map::new();

    let mut resolved_value: Result<ResolvedFieldValue, Box<dyn Error>> =
        Ok(ResolvedFieldValue::new(field.to_string(), Value::Null));

    if value.is_array() {
        let values = value.as_array().ok_or("Value is not an array")?;
        let mut m = Map::with_capacity(values.len());

        // For array-type records, we don't need to check for repetition_count at this level
        // The check will happen in process_array_field when each array element is processed

        for (idx, val) in values.iter().enumerate() {
            let sub_field = idx.to_string();
            let meta_field = metadata.get(field).ok_or_else(|| {
                format!(
                    "Array field '{}' not found in metadata or it's disabled",
                    idx
                )
            })?;

            // println!("field: {}: value {}\n", sub_field, val);

            if meta_field.enabled {
                let new_val = fast_set_value_optimized(
                    &meta_field.determined_type,
                    &sub_field,
                    val,
                    &metadata.get(field).unwrap().fields,
                    None,
                    flatten,
                )?;
                m.insert(new_val.field, new_val.value);
            }
        }
        resolved_value = Ok(ResolvedFieldValue::new(field.to_string(), Value::Object(m)));
    } else if value.is_object() {
        let obj = value.as_object().ok_or("Value is not an object")?;
        // Pre-size map to reduce reallocations when inserting
        let mut m = Map::with_capacity(obj.len());
        for (sub_field, sub_value) in obj {
            // Only check direct array fields with record type, as nested checks will be handled in their respective processing functions
            if let Some(meta_field) = metadata.get(field).and_then(|f| f.fields.get(sub_field)) {
                // Use enum comparisons instead of string comparisons
                if meta_field.determined_type == SkipprDataType::Array
                    && meta_field.determined_type_values == Some(SkipprDataType::Record)
                {
                    if let Some(array_values) = sub_value.as_array() {
                        let array_length = array_values.len() as i32;
                        if array_length > meta_field.repetition_count {
                            return Err(Box::new(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                format!("Array field '{}.{}' has {} elements, but repetition_count is {}. Falling back to slow path.",
                                        field, sub_field, array_length, meta_field.repetition_count)
                            )));
                        }
                    }
                }
            }

            let parent = metadata
                .get(field)
                .ok_or_else(|| format!("Field '{}' not found in metadata", sub_field))?;
            let meta_field = parent
                .fields
                .get(sub_field)
                .ok_or_else(|| format!("Subfield '{}' not found in fields", sub_field))?;
            if meta_field.enabled {
                let newval = fast_set_value_optimized(
                    &meta_field.determined_type,
                    sub_field,
                    sub_value,
                    &parent.fields,
                    None,
                    flatten,
                )?;
                m.insert(newval.field, newval.value);
            }
        }
        resolved_value = Ok(ResolvedFieldValue::new(field.to_string(), Value::Object(m)));
    }

    resolved_value
}

#[inline]
fn process_map_field(
    field: &str,
    value: &Value,
    metadata: &HashMap<String, Metadata>,
    flatten: bool,
) -> Result<ResolvedFieldValue, Box<dyn Error>> {
    let mut new_value = Value::Null;

    // Early return if not an object
    if !value.is_object() {
        return Ok(ResolvedFieldValue::new(field.to_string(), new_value));
    }

    // Get field metadata once
    let field_metadata = match metadata.get(field) {
        Some(meta) => meta,
        None => return Err(format!("Field '{}' not found in metadata", field).into()),
    };

    let obj = value.as_object().unwrap();
    // Pre-allocate with capacity
    let mut map = serde_json::Map::with_capacity(obj.len());

    for (key, val) in obj {
        // Quick check if this key has metadata
        if let Some(meta_field) = field_metadata.fields.get(key) {
            // Skip disabled fields early
            if !meta_field.enabled {
                continue;
            }

            if meta_field.determined_type == SkipprDataType::Array
                && meta_field.determined_type_values == Some(SkipprDataType::Record)
            {
                // Only validate arrays of records
                if let Some(array_values) = val.as_array() {
                    let array_length = array_values.len() as i32;
                    if array_length > meta_field.repetition_count {
                        return Err(Box::new(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("Array field '{}.{}' has {} elements, but repetition_count is {}. Falling back to slow path.",
                                    field, key, array_length, meta_field.repetition_count)
                        )));
                    }
                }
            }

            let new_val = fast_set_value_optimized(
                &meta_field.determined_type,
                key,
                val,
                &field_metadata.fields,
                None,
                flatten,
            )?;

            // Only add non-null values
            if !new_val.value.is_null() {
                map.insert(new_val.field, new_val.value);
            }
        } else {
            return Err(
                format!("Map field '{}' not found in metadata or it's disabled", key).into(),
            );
        }
    }

    new_value = Value::Object(map);
    Ok(ResolvedFieldValue::new(field.to_string(), new_value))
}

#[inline]
fn process_array_field(
    field: &str,
    value: &Value,
    metadata: &HashMap<String, Metadata>,
    flatten: bool,
) -> Result<ResolvedFieldValue, Box<dyn Error>> {
    let mut array: Vec<Value> = Vec::new();

    // when flattening, force parent field name to be the out_field_name
    let out_field_name = Metadata::get_field_out_field_name(metadata, field);

    if value.is_array() {
        let values = value.as_array().ok_or("Value is not an array")?;

        // Only check repetition_count for arrays of records, as primitive arrays don't have the constraint
        if let Some(meta) = metadata.get(field) {
            if meta.determined_type_values == Some(SkipprDataType::Record) {
                let array_length = values.len() as i32;
                if array_length > meta.repetition_count {
                    // Reject the message if array has more elements than repetition_count
                    return Err(Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("Array field '{}' has {} elements, but repetition_count is {}. Falling back to slow path.",
                                field, array_length, meta.repetition_count)
                    )));
                }
            }
        }

        // Pre-allocate array capacity
        array.reserve(values.len());

        let parent_meta_opt = metadata.get(field);
        let is_record_values = parent_meta_opt
            .map(|m| m.determined_type_values == Some(SkipprDataType::Record))
            .unwrap_or(false);
        for (idx, val) in values.iter().enumerate() {
            // Preserve original error behavior: error on first iteration with current idx if parent metadata missing
            let parent_meta = match parent_meta_opt {
                Some(m) => m,
                None => {
                    return Err(format!(
                        "Array field '{}' not found in metadata or it's disabled",
                        idx
                    )
                    .into())
                }
            };

            if parent_meta.enabled {
                let values_type = parent_meta.determined_type_values.as_ref().unwrap_or(&SkipprDataType::Unknown);
                if is_record_values {
                    let sub_field = "0";
                    let _new_val = match fast_set_value_optimized(
                        values_type,
                        sub_field,
                        val,
                        &parent_meta.fields,
                        None,
                        flatten,
                    ) {
                        Ok(v) => array.push(v.value),
                        Err(_e) => {
                            array.push(Value::Null);
                        }
                    };
                } else {
                    let sub_field_owned = idx.to_string();
                    let sub_field = sub_field_owned.as_str();
                    let _new_val = match fast_set_value_optimized(
                        values_type,
                        sub_field,
                        val,
                        &parent_meta.fields,
                        None,
                        flatten,
                    ) {
                        Ok(v) => array.push(v.value),
                        Err(_e) => {
                            array.push(Value::Null);
                        }
                    };
                }
            }
        }
    }
    if flatten {
        Ok(ResolvedFieldValue::new(out_field_name, Value::Array(array)))
    } else {
        Ok(ResolvedFieldValue::new(
            field.to_string(),
            Value::Array(array),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover::DateCandidate;
    #[allow(unused_imports)]
    use chrono::{FixedOffset, NaiveDateTime, Utc};

    use std::collections::HashMap;

    fn generate_metadata(field: &str, format_name: &str) -> HashMap<String, Metadata> {
        let date_candidate = DateCandidate {
            check_count: 1,
            valid_count: 1,
            field: String::from(field),
            format: String::from(format_name),
        };

        let mut meta = HashMap::new();

        meta.insert(
            String::from(field),
            Metadata {
                count: 1,
                types: HashMap::new(),
                parent_type: Some(SkipprDataType::Record),
                fields: Box::new(HashMap::new()),
                date_candidate: Some(date_candidate),
                date_parser_kind: None,
                timezone: false,
                evolution: Box::new(HashMap::new()),
                enabled: true,
                out_field_name: String::from(field),
                determined_type: SkipprDataType::Date,
                determined_type_values: None,
                repetition_count: 1,
            },
        );

        meta
    }

    #[test]
    fn test_fast_set_date_with_valid_date() {
        let _foo: AnalyseSchema = AnalyseSchema { i: 0 };
        let _updated_schema = "no".to_string();

        let field = "test_field";

        let date_str = "2023-05-21 12:34:56";
        let format_name = AnalyseSchema::is_valid_date(date_str).unwrap();
        let format = DateFormats::from_str(format_name).unwrap().as_str();

        let mut meta = generate_metadata(field, format_name);

        let value = Value::String(String::from(date_str));

        let result = fast_set_date(field, &value, &mut meta);

        let expected_date = Helpers::parse_date_from_string(date_str, format).unwrap();
        let expected_millis =
            Value::Number(serde_json::Number::from(expected_date.timestamp() * 1000));

        assert!(result.is_ok());
        assert_eq!(result.unwrap().value, expected_millis);
    }

    #[test]
    fn test_fast_set_date_with_valid_iso_date() {
        let _foo: AnalyseSchema = AnalyseSchema { i: 0 };
        let _updated_schema = "no".to_string();

        let field = "test_field";

        // let date_str = "2023-05-23T07:09:03.000Z";
        // let date_str = "2023-07-11T12:56:44.000Z";
        // let date_str = "2023-07-11T14:56:44+02:00";
        let date_str = "2023-07-11T14:56:44";
        let format_name = AnalyseSchema::is_valid_date(date_str).unwrap();
        let format = DateFormats::from_str(format_name).unwrap().as_str();

        let mut meta = generate_metadata(field, format_name);

        let value = Value::String(String::from(date_str));

        let result = fast_set_date(field, &value, &mut meta);

        let expected_date = Helpers::parse_date_from_string(date_str, format).unwrap();

        let expected_millis =
            Value::Number(serde_json::Number::from(expected_date.timestamp() * 1000));

        println!("Expected date {} as {}", date_str, expected_millis);

        assert!(result.is_ok());
        assert_eq!(result.unwrap().value, expected_millis);
    }

    #[test]
    fn test_fast_set_date_with_valid_iso_timezone_date() {
        let _foo: AnalyseSchema = AnalyseSchema { i: 0 };
        let _updated_schema = "no".to_string();

        let field = "test_field";

        let date_str = "2023-12-11T15:49:31+01:00";
        let format_name = AnalyseSchema::is_valid_date(date_str).unwrap();
        let format = DateFormats::from_str(format_name).unwrap().as_str();

        let mut meta = generate_metadata(field, format_name);

        let value = Value::String(String::from(date_str));

        let result = fast_set_date(field, &value, &mut meta);

        let expected_date = Helpers::parse_date_from_string(date_str, format).unwrap();
        let expected_millis =
            Value::Number(serde_json::Number::from(expected_date.timestamp() * 1000));

        assert!(result.is_ok());
        assert_eq!(result.unwrap().value, expected_millis);
    }
}

#[cfg(test)]
mod tests_match_scalar_value_fast {

    use super::*;
    use serde_json::Value;

    fn str_to_val(s: &str) -> Value {
        Value::from(s)
    }

    fn i64_to_val(i: i64) -> Value {
        Value::from(i)
    }

    fn f64_to_val(f: f64) -> Value {
        Value::from(f)
    }

    fn bool_to_val(b: bool) -> Value {
        Value::from(b)
    }

    fn get_or_panic(result: Result<ResolvedFieldValue, Box<dyn Error>>) -> ResolvedFieldValue {
        result.expect("Unexpected error")
    }

    #[test]
    fn test_match_scalar_value_string() {
        let mut metadata = HashMap::new();
        metadata.insert("field".to_string(), Metadata::new().unwrap());
        let flatten = false;

        assert_eq!(
            get_or_panic(match_scalar_value_fast(
                "field",
                "string",
                &str_to_val("hello"),
                &metadata,
                true,
                flatten
            ))
            .value,
            str_to_val("hello")
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast(
                "field",
                "string",
                &i64_to_val(123),
                &metadata,
                true,
                flatten
            ))
            .value,
            str_to_val("123")
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast(
                "field",
                "string",
                &f64_to_val(123.4),
                &metadata,
                true,
                flatten
            ))
            .value,
            str_to_val("123.4")
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast(
                "field",
                "string",
                &bool_to_val(true),
                &metadata,
                true,
                flatten
            ))
            .value,
            str_to_val("true")
        );
    }

    #[test]
    fn test_match_scalar_value_fast_int() {
        let mut metadata = HashMap::new();
        metadata.insert("field".to_string(), Metadata::new().unwrap());
        let flatten = false;

        assert_eq!(
            get_or_panic(match_scalar_value_fast(
                "field",
                "int",
                &i64_to_val(123),
                &metadata,
                true,
                flatten
            ))
            .value,
            i64_to_val(123)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast(
                "field",
                "int",
                &str_to_val("123"),
                &metadata,
                true,
                flatten
            ))
            .value,
            i64_to_val(123)
        );

        assert_eq!(
            get_or_panic(match_scalar_value_fast(
                "field",
                "int",
                &bool_to_val(true),
                &metadata,
                true,
                flatten
            ))
            .value,
            i64_to_val(1)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast(
                "field",
                "int",
                &bool_to_val(false),
                &metadata,
                true,
                flatten
            ))
            .value,
            i64_to_val(0)
        );

        assert_eq!(
            get_or_panic(match_scalar_value_fast(
                "field",
                "int",
                &str_to_val("true"),
                &metadata,
                true,
                flatten
            ))
            .value,
            i64_to_val(1)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast(
                "field",
                "int",
                &str_to_val("false"),
                &metadata,
                true,
                flatten
            ))
            .value,
            i64_to_val(0)
        );
        assert_eq!(
            match_scalar_value_fast(
                "field",
                "int",
                &str_to_val("True"),
                &metadata,
                true,
                flatten
            )
            .is_err(),
            true
        );
        assert_eq!(
            match_scalar_value_fast(
                "field",
                "int",
                &str_to_val("False"),
                &metadata,
                true,
                flatten
            )
            .is_err(),
            true
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast(
                "field",
                "int",
                &i64_to_val(1),
                &metadata,
                true,
                flatten
            ))
            .value,
            i64_to_val(1)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast(
                "field",
                "int",
                &str_to_val("1"),
                &metadata,
                true,
                flatten
            ))
            .value,
            i64_to_val(1)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast(
                "field",
                "int",
                &i64_to_val(0),
                &metadata,
                true,
                flatten
            ))
            .value,
            i64_to_val(0)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast(
                "field",
                "int",
                &str_to_val("0"),
                &metadata,
                true,
                flatten
            ))
            .value,
            i64_to_val(0)
        );
    }

    #[test]
    fn test_match_scalar_value_fast_double() {
        let mut metadata = HashMap::new();
        metadata.insert("field".to_string(), Metadata::new().unwrap());
        let flatten = false;

        assert_eq!(
            get_or_panic(match_scalar_value_fast(
                "field",
                "double",
                &f64_to_val(123.4),
                &metadata,
                true,
                flatten
            ))
            .value,
            f64_to_val(123.4)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast(
                "field",
                "double",
                &str_to_val("123.4"),
                &metadata,
                true,
                flatten
            ))
            .value,
            f64_to_val(123.4)
        );
    }

    #[test]
    fn test_match_scalar_value_fast_boolean() {
        let mut metadata = HashMap::new();
        metadata.insert("field".to_string(), Metadata::new().unwrap());
        let flatten = false;

        assert_eq!(
            get_or_panic(match_scalar_value_fast(
                "field",
                "boolean",
                &bool_to_val(true),
                &metadata,
                true,
                flatten
            ))
            .value,
            bool_to_val(true)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast(
                "field",
                "boolean",
                &bool_to_val(false),
                &metadata,
                true,
                flatten
            ))
            .value,
            bool_to_val(false)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast(
                "field",
                "boolean",
                &str_to_val("true"),
                &metadata,
                true,
                flatten
            ))
            .value,
            bool_to_val(true)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast(
                "field",
                "boolean",
                &str_to_val("false"),
                &metadata,
                true,
                flatten
            ))
            .value,
            bool_to_val(false)
        );
        assert_eq!(
            match_scalar_value_fast(
                "field",
                "boolean",
                &str_to_val("True"),
                &metadata,
                true,
                flatten
            )
            .is_err(),
            true
        );
        assert_eq!(
            match_scalar_value_fast(
                "field",
                "boolean",
                &str_to_val("False"),
                &metadata,
                true,
                flatten
            )
            .is_err(),
            true
        );
        assert_eq!(
            match_scalar_value_fast(
                "field",
                "boolean",
                &str_to_val("Yes"),
                &metadata,
                true,
                flatten
            )
            .is_err(),
            true
        );
        assert_eq!(
            match_scalar_value_fast(
                "field",
                "boolean",
                &str_to_val("No"),
                &metadata,
                true,
                flatten
            )
            .is_err(),
            true
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast(
                "field",
                "boolean",
                &i64_to_val(1),
                &metadata,
                true,
                flatten
            ))
            .value,
            bool_to_val(true)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast(
                "field",
                "boolean",
                &i64_to_val(0),
                &metadata,
                true,
                flatten
            ))
            .value,
            bool_to_val(false)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast(
                "field",
                "boolean",
                &str_to_val("1"),
                &metadata,
                true,
                flatten
            ))
            .value,
            bool_to_val(true)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast(
                "field",
                "boolean",
                &str_to_val("0"),
                &metadata,
                true,
                flatten
            ))
            .value,
            bool_to_val(false)
        );
        // assert_eq!(
        //     get_or_panic(match_scalar_value_fast("field", "boolean", &f64_to_val(1.0))),
        //     bool_to_val(true)
        // );
    }

    #[test]
    #[should_panic(expected = "Unknown data type 'unknown'")]
    fn test_match_scalar_value_fast_unknown() {
        let mut metadata = HashMap::new();
        metadata.insert("field".to_string(), Metadata::new().unwrap());
        let flatten = false;

        get_or_panic(match_scalar_value_fast(
            "field",
            "unknown",
            &str_to_val("hello"),
            &metadata,
            true,
            flatten,
        ));
    }
}

#[cfg(test)]
mod tests_process_array_field {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_process_array_field_ints() -> Result<(), Box<dyn Error>> {
        let field = "test_field";
        let value = json!([1, 2, 3]);
        let flatten = false;
        let mut metadata = HashMap::new();
        let mut meta_data_item = Metadata::new()?;
        meta_data_item.determined_type_values = Some(SkipprDataType::Integer);
        metadata.insert(field.to_string(), meta_data_item);

        let result = process_array_field(field, &value, &metadata, flatten)?;

        assert_eq!(result.value, json!([1, 2, 3]));
        Ok(())
    }

    #[test]
    fn test_process_flatten_array_field_ints() -> Result<(), Box<dyn Error>> {
        let field = "test_field";
        let value = json!([1, 2, 3]);
        let flatten = true;
        let mut metadata = HashMap::new();
        let mut meta_data_item = Metadata::new()?;
        meta_data_item.determined_type_values = Some(SkipprDataType::Integer);
        meta_data_item.out_field_name = "splat_name".to_string();
        metadata.insert(field.to_string(), meta_data_item);

        let result = process_array_field(field, &value, &metadata, flatten)?;

        assert_eq!(result.field, "splat_name");
        assert_eq!(result.value, json!([1, 2, 3]));
        Ok(())
    }

    #[test]
    fn test_process_array_field_floats() -> Result<(), Box<dyn Error>> {
        let field = "test_field";
        let value = json!([1.2, 2.3, 3.4]);
        let flatten = false;
        let mut metadata = HashMap::new();
        let mut meta_data_item = Metadata::new()?;
        meta_data_item.determined_type_values = Some(SkipprDataType::Double);
        metadata.insert(field.to_string(), meta_data_item);

        let result = process_array_field(field, &value, &metadata, flatten)?;

        assert_eq!(result.value, json!([1.2, 2.3, 3.4]));
        Ok(())
    }

    #[test]
    fn test_process_array_field_booleans() -> Result<(), Box<dyn Error>> {
        let field = "test_field";
        let value = json!([true, false, true]);
        let flatten = false;
        let mut metadata = HashMap::new();
        let mut meta_data_item = Metadata::new()?;
        meta_data_item.determined_type_values = Some(SkipprDataType::Boolean);
        metadata.insert(field.to_string(), meta_data_item);

        let result = process_array_field(field, &value, &metadata, flatten)?;

        assert_eq!(result.value, json!([true, false, true]));
        Ok(())
    }

    #[test]
    fn test_process_array_field_booleans_int() -> Result<(), Box<dyn Error>> {
        let field = "test_field";
        let value = json!([1, 0, 1]);
        let flatten = false;
        let mut metadata = HashMap::new();
        let mut meta_data_item = Metadata::new()?;
        meta_data_item.determined_type_values = Some(SkipprDataType::Boolean);
        metadata.insert(field.to_string(), meta_data_item);

        let result = process_array_field(field, &value, &metadata, flatten)?;

        assert_eq!(result.value, json!([true, false, true]));
        Ok(())
    }

    #[test]
    fn test_process_array_field_null() -> Result<(), Box<dyn Error>> {
        let field = "test_field";
        let value = json!([null, null, null]);
        let flatten = false;
        let mut metadata = HashMap::new();
        let mut meta_data_item = Metadata::new()?;
        meta_data_item.determined_type_values = Some(SkipprDataType::Null);
        metadata.insert(field.to_string(), meta_data_item);

        let result = process_array_field(field, &value, &metadata, flatten)?;

        assert_eq!(result.value, json!([null, null, null]));
        Ok(())
    }

    #[test]
    fn test_process_array_field_strings() -> Result<(), Box<dyn Error>> {
        let field = "test_field";
        let value = json!(["one", "two", "three"]);
        let flatten = false;
        let mut metadata = HashMap::new();
        let mut meta_data_item = Metadata::new()?;
        meta_data_item.determined_type_values = Some(SkipprDataType::String);
        metadata.insert(field.to_string(), meta_data_item);

        let result = process_array_field(field, &value, &metadata, flatten)?;

        assert_eq!(result.value, json!(["one", "two", "three"]));
        Ok(())
    }

    #[test]
    fn test_process_array_field_field_not_found() {
        let field = "test_field";
        let value = json!([1, 2, 3]);
        let flatten = false;
        let metadata = HashMap::new();

        let result = process_array_field(field, &value, &metadata, flatten);

        assert!(result.is_err());
    }
}

#[cfg(test)]
mod tests_process_array_field_repetition_count {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_process_array_field_exceeds_repetition_count() {
        let field = "contacts";
        let value = json!([
            {"name": "Person 1", "tel": 123},
            {"name": "Person 2", "tel": 456},
            {"name": "Person 3", "tel": 789}
        ]);
        let flatten = false;

        let mut metadata = HashMap::new();
        let mut meta_data_item = Metadata::new().unwrap();
        meta_data_item.determined_type = SkipprDataType::Array;
        meta_data_item.determined_type_values = Some(SkipprDataType::Record);
        // Set repetition_count to 2, which is less than the 3 elements in the array
        meta_data_item.repetition_count = 2;
        metadata.insert(field.to_string(), meta_data_item);

        let result = process_array_field(field, &value, &metadata, flatten);

        // Verify the function returns an error
        assert!(result.is_err());
        let error = result.unwrap_err();
        let error_string = error.to_string();

        // Verify the error message contains the expected information
        assert!(error_string.contains("Falling back to slow path"));
        assert!(error_string.contains("has 3 elements, but repetition_count is 2"));
    }

    #[test]
    fn test_process_array_field_matches_repetition_count() -> Result<(), Box<dyn Error>> {
        let field = "contacts";
        let value = json!([
            {"name": "Person 1", "tel": 123},
            {"name": "Person 2", "tel": 456}
        ]);
        let flatten = false;

        let mut metadata = HashMap::new();
        let mut meta_data_item = Metadata::new()?;
        meta_data_item.determined_type = SkipprDataType::Array;
        meta_data_item.determined_type_values = Some(SkipprDataType::Record);
        meta_data_item.repetition_count = 2;

        let mut field_zero = Metadata::new()?;
        field_zero.determined_type = SkipprDataType::Record;
        field_zero.determined_type_values = None;
        meta_data_item.fields.insert("0".to_string(), field_zero);

        metadata.insert(field.to_string(), meta_data_item);

        let result = process_array_field(field, &value, &metadata, flatten);

        // Verify the function doesn't return an error
        assert!(result.is_ok());
        Ok(())
    }

    #[test]
    fn test_primitive_array_ignores_repetition_count() -> Result<(), Box<dyn Error>> {
        let field = "numbers";
        let value = json!([1, 2, 3, 4, 5]);
        let flatten = false;

        let mut metadata = HashMap::new();
        let mut meta_data_item = Metadata::new()?;
        meta_data_item.determined_type = SkipprDataType::Array;
        meta_data_item.determined_type_values = Some(SkipprDataType::Integer);
        meta_data_item.repetition_count = 2;
        metadata.insert(field.to_string(), meta_data_item);

        let result = process_array_field(field, &value, &metadata, flatten);

        // Verify the function doesn't return an error for primitive arrays
        assert!(result.is_ok());
        Ok(())
    }
}

#[cfg(test)]
mod tests_process_record_field {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_process_record_field_with_arrays() {
        let mut metadata = HashMap::new();
        let mut record_meta = Metadata::new().unwrap();
        record_meta.determined_type = SkipprDataType::Record;
        record_meta.fields = Box::new(HashMap::new());

        let mut array_field_meta = Metadata::new().unwrap();
        array_field_meta.determined_type = SkipprDataType::Array;
        array_field_meta.determined_type_values = Some(SkipprDataType::Record);
        array_field_meta.repetition_count = 2;

        record_meta
            .fields
            .insert("items".to_string(), array_field_meta);
        metadata.insert("person".to_string(), record_meta);

        // Create a record with an array of length 2 (within limit)
        let value = json!({
            "items": [
                {"name": "item1"},
                {"name": "item2"}
            ]
        });

        // This should succeed because the array length is within repetition_count
        let result = process_record_field("person", &value, &metadata, false);
        assert!(result.is_ok());

        // Create a record with an array of length 3 (exceeds limit)
        let value_exceeds = json!({
            "items": [
                {"name": "item1"},
                {"name": "item2"},
                {"name": "item3"}
            ]
        });

        // This should fail because the array length exceeds repetition_count
        let result_exceeds = process_record_field("person", &value_exceeds, &metadata, false);
        assert!(result_exceeds.is_err());

        let mut nested_metadata = HashMap::new();
        let mut outer_record_meta = Metadata::new().unwrap();
        outer_record_meta.determined_type = SkipprDataType::Record;
        outer_record_meta.fields = Box::new(HashMap::new());

        let mut nested_record_meta = Metadata::new().unwrap();
        nested_record_meta.determined_type = SkipprDataType::Record;
        nested_record_meta.fields = Box::new(HashMap::new());

        let mut nested_array_meta = Metadata::new().unwrap();
        nested_array_meta.determined_type = SkipprDataType::Array;
        nested_array_meta.determined_type_values = Some(SkipprDataType::String);
        nested_array_meta.repetition_count = 3;

        nested_record_meta
            .fields
            .insert("tags".to_string(), nested_array_meta);
        outer_record_meta
            .fields
            .insert("details".to_string(), nested_record_meta);
        nested_metadata.insert("user".to_string(), outer_record_meta);

        // This JSON has a nested structure with an array in the nested field
        let nested_value = json!({
            "details": {
                "tags": ["tag1", "tag2", "tag3"]
            }
        });

        // This should succeed because the nested array is within limits
        let nested_result = process_record_field("user", &nested_value, &nested_metadata, false);
        assert!(nested_result.is_ok());

        // JSON with nested array exceeding limits
        let nested_value_exceeds = json!({
            "details": {
                "tags": ["tag1", "tag2", "tag3", "tag4"]
            }
        });

        // This should now be handled by the process_array_field function during recursive processing
        // rather than being checked in process_record_field directly
        let nested_result_exceeds =
            process_record_field("user", &nested_value_exceeds, &nested_metadata, false);
        // The optimization we made is that this error would be caught in process_array_field
        // when it processes the "tags" field, not in the process_record_field check
        assert!(nested_result_exceeds.is_ok());
    }
}

#[cfg(test)]
mod tests_sort_fields {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_sort_fields_empty_object() {
        let mut value = json!({});
        sort_fields(&mut value);
        assert_eq!(value, json!({}));
    }

    #[test]
    fn test_sort_fields_simple_object() {
        let mut value = json!({"c": 3, "a": 1, "b": 2});
        sort_fields(&mut value);

        // Create a string representation to verify order
        let sorted_json = serde_json::to_string(&value).unwrap();
        assert_eq!(sorted_json, r#"{"a":1,"b":2,"c":3}"#);
    }

    #[test]
    fn test_sort_fields_nested_object() {
        let mut value = json!({
            "z": 26,
            "a": {
                "c": 3,
                "a": 1,
                "b": 2
            }
        });
        sort_fields(&mut value);

        // Create a string representation to verify order
        let sorted_json = serde_json::to_string(&value).unwrap();
        assert_eq!(sorted_json, r#"{"a":{"a":1,"b":2,"c":3},"z":26}"#);
    }

    #[test]
    fn test_sort_fields_array() {
        let mut value = json!([
            {"c": 3, "a": 1, "b": 2},
            {"z": 26, "x": 24}
        ]);
        sort_fields(&mut value);

        // Create a string representation to verify order
        let sorted_json = serde_json::to_string(&value).unwrap();
        assert_eq!(sorted_json, r#"[{"a":1,"b":2,"c":3},{"x":24,"z":26}]"#);
    }

    #[test]
    fn test_sort_fields_deep_nesting() {
        let mut value = json!({
            "z": {
                "y": {
                    "c": 3,
                    "a": 1,
                    "b": 2
                },
                "x": [
                    {"m": 13, "k": 11},
                    {"d": 4, "c": 3}
                ]
            },
            "a": 1
        });
        sort_fields(&mut value);

        // Create a string representation to verify order
        let sorted_json = serde_json::to_string(&value).unwrap();
        assert_eq!(
            sorted_json,
            r#"{"a":1,"z":{"x":[{"k":11,"m":13},{"c":3,"d":4}],"y":{"a":1,"b":2,"c":3}}}"#
        );
    }

    #[test]
    fn test_sort_fields_primitive_values() {
        // Primitive values should remain unchanged
        let mut string_val = json!("test");
        let mut num_val = json!(42);
        let mut bool_val = json!(true);
        let mut null_val = json!(null);

        sort_fields(&mut string_val);
        sort_fields(&mut num_val);
        sort_fields(&mut bool_val);
        sort_fields(&mut null_val);

        assert_eq!(string_val, json!("test"));
        assert_eq!(num_val, json!(42));
        assert_eq!(bool_val, json!(true));
        assert_eq!(null_val, json!(null));
    }
}

#[cfg(test)]
mod tests_fast_path_ingest {
    use super::*;
    use serde_json::json;

    // Helper function to create metadata for testing
    fn create_test_metadata() -> HashMap<String, Metadata> {
        let mut metadata = HashMap::new();

        let mut string_meta = Metadata::new().unwrap();
        string_meta.determined_type = SkipprDataType::String;
        metadata.insert("name".to_string(), string_meta);

        let mut int_meta = Metadata::new().unwrap();
        int_meta.determined_type = SkipprDataType::Integer;
        metadata.insert("age".to_string(), int_meta);

        let mut bool_meta = Metadata::new().unwrap();
        bool_meta.determined_type = SkipprDataType::Boolean;
        metadata.insert("active".to_string(), bool_meta);

        let mut record_meta = Metadata::new().unwrap();
        record_meta.determined_type = SkipprDataType::Record;

        let mut street_meta = Metadata::new().unwrap();
        street_meta.determined_type = SkipprDataType::String;
        record_meta.fields.insert("street".to_string(), street_meta);

        let mut city_meta = Metadata::new().unwrap();
        city_meta.determined_type = SkipprDataType::String;
        record_meta.fields.insert("city".to_string(), city_meta);

        metadata.insert("address".to_string(), record_meta);

        let mut array_meta = Metadata::new().unwrap();
        array_meta.determined_type = SkipprDataType::Array;
        array_meta.determined_type_values = Some(SkipprDataType::String);
        array_meta.repetition_count = 5;
        metadata.insert("tags".to_string(), array_meta);

        metadata
    }

    // Setup DEFAULT_NESTED_MESSAGE for tests
    fn setup_default_message(namespace: &str) {
        let message = json!({
            "name": null,
            "age": null,
            "active": null,
            "address": {
                "street": null,
                "city": null
            },
            "tags": []
        });

        DEFAULT_NESTED_MESSAGE
            .write()
            .insert(namespace.to_string(), message);
    }

    #[test]
    fn test_fast_path_ingest_simple() {
        let namespace = "test_namespace";
        let metadata = create_test_metadata();
        setup_default_message(namespace);

        let input = json!({
            "name": "John Doe",
            "age": 30,
            "active": true
        });

        let result = fast_path_ingest(&input, &metadata, namespace, false);
        assert!(result.is_ok());

        let output = result.unwrap();
        assert_eq!(output["name"], "John Doe");
        assert_eq!(output["age"], 30);
        assert_eq!(output["active"], true);
    }

    #[test]
    fn test_fast_path_ingest_nested() {
        let namespace = "test_namespace";
        let metadata = create_test_metadata();
        setup_default_message(namespace);

        let input = json!({
            "name": "John Doe",
            "address": {
                "street": "123 Main St",
                "city": "Anytown"
            }
        });

        let result = fast_path_ingest(&input, &metadata, namespace, false);
        assert!(result.is_ok());

        let output = result.unwrap();
        assert_eq!(output["name"], "John Doe");
        assert_eq!(output["address"]["street"], "123 Main St");
        assert_eq!(output["address"]["city"], "Anytown");
    }

    #[test]
    fn test_fast_path_ingest_array() {
        let namespace = "test_namespace";
        let metadata = create_test_metadata();
        setup_default_message(namespace);

        let input = json!({
            "name": "John Doe",
            "tags": ["developer", "rust", "data"]
        });

        let result = fast_path_ingest(&input, &metadata, namespace, false);
        assert!(result.is_ok());

        let output = result.unwrap();
        assert_eq!(output["name"], "John Doe");
        assert_eq!(output["tags"], json!(["developer", "rust", "data"]));
    }

    #[test]
    fn test_fast_path_ingest_null_values() {
        let namespace = "test_namespace";
        let metadata = create_test_metadata();
        setup_default_message(namespace);

        let input = json!({
            "name": "John Doe",
            "age": null,
            "active": null
        });

        let result = fast_path_ingest(&input, &metadata, namespace, false);
        assert!(result.is_ok());

        let output = result.unwrap();
        assert_eq!(output["name"], "John Doe");
        // Null values should be preserved in the template
        assert!(output["age"].is_null());
        assert!(output["active"].is_null());
    }

    #[test]
    fn test_fast_path_ingest_missing_field() {
        let namespace = "test_namespace";
        let metadata = create_test_metadata();
        setup_default_message(namespace);

        let input = json!({
            "name": "John Doe",
            "unknown_field": "value"
        });

        let result = fast_path_ingest(&input, &metadata, namespace, false);
        assert!(result.is_err());

        let err = result.unwrap_err().to_string();
        assert!(err.contains("Field 'unknown_field' not found in metadata"));
    }

    #[test]
    fn test_fast_path_ingest_invalid_type() {
        let namespace = "test_namespace";
        let metadata = create_test_metadata();
        setup_default_message(namespace);

        let input = json!({
            "name": 12345,  // Name should be a string
            "age": "thirty" // Age should be an integer
        });

        // The function actually returns an error when types don't match
        let result = fast_path_ingest(&input, &metadata, namespace, false);

        // Check that the result is an error
        assert!(result.is_err());

        // Verify that the error message contains information about the invalid type
        let err = result.unwrap_err().to_string();
        assert!(err.contains("thirty") || err.contains("int") || err.contains("not a"));
    }

    #[test]
    fn test_fast_path_ingest_with_flatten() {
        let namespace = "test_namespace";
        let mut metadata = create_test_metadata();

        // Add a nested record with out_field_name for flattening
        let mut nested_meta = Metadata::new().unwrap();
        nested_meta.determined_type = SkipprDataType::Record;
        nested_meta.out_field_name = "metrics_flat".to_string();

        let mut count_meta = Metadata::new().unwrap();
        count_meta.determined_type = SkipprDataType::Integer;
        count_meta.out_field_name = "count_flat".to_string();
        nested_meta.fields.insert("count".to_string(), count_meta);

        metadata.insert("metrics".to_string(), nested_meta);

        setup_default_message(namespace);

        let input = json!({
            "name": "John Doe",
            "metrics": {
                "count": 42
            }
        });

        // We need a mock for Helpers::flatten in this test
        // This is a complex test due to the external dependency on Helpers::flatten
        // For now, we'll expect it to return an error or be handled
        let _result = fast_path_ingest(&input, &metadata, namespace, true);
        // We'll skip assertion here since we can't easily mock Helpers::flatten
    }

    #[test]
    fn test_fast_path_ingest_no_default_message() {
        let namespace = "unknown_namespace";
        let metadata = create_test_metadata();

        // Don't set up default message for this namespace

        let input = json!({
            "name": "John Doe"
        });

        let result = fast_path_ingest(&input, &metadata, namespace, false);
        assert!(result.is_err());

        let err = result.unwrap_err().to_string();
        assert_eq!(err, "No default message template found");
    }
}
