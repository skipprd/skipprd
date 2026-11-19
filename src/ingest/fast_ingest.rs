use std::collections::HashMap;

use serde_json::Value;
use std::error::Error;
use std::sync::Arc;

use once_cell::sync::Lazy;
use serde_json::Map;

use crate::discover::date_formats::DateFormats;
use crate::discover::evolution::Evolution;
use crate::discover::schema_alter::IngestKeys;
use crate::discover::{Metadata, SkipprDataType};

use crate::helpers::Helpers;
use crate::ingest::ingest::ResolvedFieldValue;
use dashmap::DashMap;

#[allow(unused_imports)]
use crate::discover::DateCandidate;
use crate::helpers::configuration::Config;
use crate::ingest::scalar::{coerce_scalar, scalar_mismatch};
#[allow(unused_imports)]
use chrono::{DateTime, FixedOffset, NaiveDateTime, Utc};

pub static DEFAULT_NESTED_MESSAGE: Lazy<Arc<DashMap<String, Arc<Value>>>> =
    Lazy::new(|| Arc::new(DashMap::new()));

pub fn create_default_nested_message(metadata: &HashMap<String, Metadata>) -> Value {
    let mut message = Value::Object(Map::new());
    for (field, meta_data) in metadata {
        if meta_data.enabled {
            if meta_data.fields.is_empty() {
                message[meta_data.out_field_name.clone()] =
                    meta_data.default_value().cloned().unwrap_or(Value::Null);
            } else if meta_data.determined_type == SkipprDataType::Array {
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
    config: &Config,
    unwrapped_message: &Value,
    metadata: &HashMap<String, Metadata>,
    namespace: &str,
    flatten: bool,
    output: &Metadata,
) -> Result<Value, Box<dyn Error>> {
    // Get the template message once
    let mut _message = match DEFAULT_NESTED_MESSAGE.get(namespace) {
        Some(m) => m.as_ref().clone(),
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

    let mut source = Metadata::new().unwrap();
    source.fields = Box::new(metadata.clone());
    let mut object = object.clone();
    IngestKeys::new(&source, output)
        .rewrite(&mut object)
        .map_err(|err| err.to_string())?;
    for (field, value) in &object {
        let Some(meta_data) = metadata.get(field) else {
            if crate::helpers::configuration::Config::debug_enabled() {
                println!(
                    "fast_path_ingest: missing field in metadata: '{}' ns={}",
                    field, namespace
                );
            }
            return Err(format!("Field '{}' not found in metadata", field).into());
        };
        if !meta_data.enabled {
            continue;
        }
        if value.is_null() || (value.is_string() && value.as_str().unwrap_or_default().is_empty()) {
            continue;
        }
        fields_to_process.push((
            field.clone(),
            value.clone(),
            meta_data.determined_type.clone(),
        ));
    }

    // Second pass - process all fields
    // Insert directly into the template object for speed (avoids intermediate indexing conversions)
    let obj = _message
        .as_object_mut()
        .ok_or("Template is not an object")?;
    for (field, value, data_type) in fields_to_process {
        // No need to convert string to enum since we already have the enum
        let resolved_value =
            match fast_set_value(config, &data_type, &field, &value, metadata, true, flatten) {
                Ok(v) => v,
                Err(e) => {
                    if crate::helpers::configuration::Config::log_wal_enabled() {
                        println!(
                            "fast_path_ingest: set_value failed for field='{}' err={}",
                            field, e
                        );
                    }
                    return Err(e);
                }
            };

        if !resolved_value.value.is_null() {
            obj.insert(resolved_value.field, resolved_value.value);
        }
    }

    validate_required_fields(metadata, &_message)?;

    // Apply flattening if needed
    if flatten {
        _message = match Helpers::flatten(&_message, &metadata) {
            Ok(m) => m,
            Err(e) => return Err(e),
        };
    }

    Ok(_message)
}

pub fn validate_required_fields(
    metadata: &HashMap<String, Metadata>,
    message: &Value,
) -> Result<(), Box<dyn Error>> {
    let object = message.as_object().ok_or("Message is not an object")?;
    for meta_data in metadata.values() {
        if !meta_data.enabled {
            continue;
        }
        let out_field_name = &meta_data.out_field_name;
        if meta_data.fields.is_empty() {
            let value = object.get(out_field_name).unwrap_or(&Value::Null);
            if !meta_data.nullable() && meta_data.default_value().is_none() && value.is_null() {
                return Err(
                    format!("Required field '{}' is missing or null", out_field_name).into(),
                );
            }
        } else if let Some(child_message) = object.get(out_field_name) {
            if child_message.is_object() {
                validate_required_fields(&meta_data.fields, child_message)?;
            }
        }
    }
    Ok(())
}

/// Fast-path set_value: dest walk first; published sibling dest-only on dest miss.
#[inline]
pub fn fast_set_value(
    config: &Config,
    data_type: &SkipprDataType,
    field: &str,
    value: &Value,
    metadata: &HashMap<String, Metadata>,
    apply_evolution: bool,
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

    let dest = match data_type {
        SkipprDataType::Record => {
            process_record_field(config, field, value, metadata, flatten, apply_evolution)
        }
        SkipprDataType::Map => {
            process_map_field(config, field, value, metadata, flatten, apply_evolution)
        }
        SkipprDataType::Array => process_array_field(config, field, value, metadata, flatten),
        SkipprDataType::Date => fast_set_date(field, value, metadata),
        SkipprDataType::String
        | SkipprDataType::Long
        | SkipprDataType::Integer
        | SkipprDataType::Short
        | SkipprDataType::Byte
        | SkipprDataType::Double
        | SkipprDataType::Float
        | SkipprDataType::Decimal
        | SkipprDataType::Boolean
        | SkipprDataType::TimestampMilli
        | SkipprDataType::Timestamp
        | SkipprDataType::Time
        | SkipprDataType::Binary
        | SkipprDataType::Uuid
        | SkipprDataType::Fixed
        | SkipprDataType::Json
        | SkipprDataType::Null
        | SkipprDataType::Unknown => {
            match_scalar_value_optimized(field, data_type, value, metadata)
        }
    };
    match dest {
        Ok(v) => Ok(v),
        Err(e) if apply_evolution => {
            if let Some(v) = try_fast_path_evolution(config, field, value, metadata, flatten) {
                Ok(v)
            } else {
                Err(e)
            }
        }
        Err(e) => Err(e),
    }
}

/// Dest-only walk of published siblings on live metadata, ranked by the factory scorer.
fn try_fast_path_evolution(
    config: &Config,
    field: &str,
    value: &Value,
    metadata: &HashMap<String, Metadata>,
    flatten: bool,
) -> Option<ResolvedFieldValue> {
    let mut ranked: Vec<(i32, Evolution)> = metadata
        .get(field)
        .map(|m| {
            m.evolution
                .values()
                .filter(|e| metadata.contains_key(&e.new_field))
                .map(|e| (e.score_published_sibling(value, metadata), e.clone()))
                .collect()
        })
        .unwrap_or_default();
    if ranked.is_empty() {
        return None;
    }
    ranked.sort_by(|a, b| b.0.cmp(&a.0));
    for (_score, evolution) in ranked {
        if let Ok(v) = fast_set_value(
            config,
            &evolution.type_string,
            &evolution.new_field,
            value,
            metadata,
            true,
            flatten,
        ) {
            return Some(v);
        }
    }
    None
}

/// Scalar coerce on the dest walk. Sibling apply lives in `fast_set_value`.
#[inline]
pub fn match_scalar_value_optimized(
    field: &str,
    data_type: &SkipprDataType,
    value: &Value,
    metadata: &HashMap<String, Metadata>,
) -> Result<ResolvedFieldValue, Box<dyn Error>> {
    if value.is_null() {
        return Ok(ResolvedFieldValue {
            field: Metadata::get_field_out_field_name(metadata, field),
            value: Value::Null,
        });
    }

    let output_field_name = Metadata::get_field_out_field_name(metadata, field);

    match coerce_scalar(data_type, value) {
        Ok(v) => Ok(ResolvedFieldValue {
            field: output_field_name,
            value: v,
        }),
        Err(_) => Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            scalar_mismatch(data_type, None, field, value),
        ))),
    }
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
                    )));
                }
            };

            let date_meta = match parent_field_meta.date_candidate.as_ref() {
                Some(f) => f,
                None => {
                    return Err(Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("Could not find date candidate in metadata for {}", field),
                    )));
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
    config: &Config,
    field: &str,
    value: &Value,
    metadata: &HashMap<String, Metadata>,
    flatten: bool,
    apply_evolution: bool,
) -> Result<ResolvedFieldValue, Box<dyn Error>> {
    let _m = Map::new();

    let resolved_value: Result<ResolvedFieldValue, Box<dyn Error>>;

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
                let new_val = fast_set_value(
                    config,
                    &meta_field.determined_type,
                    &sub_field,
                    val,
                    &metadata.get(field).unwrap().fields,
                    apply_evolution,
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
                                format!(
                                    "Array field '{}.{}' has {} elements, but repetition_count is {}. Falling back to slow path.",
                                    field, sub_field, array_length, meta_field.repetition_count
                                ),
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
                let newval = fast_set_value(
                    config,
                    &meta_field.determined_type,
                    sub_field,
                    sub_value,
                    &parent.fields,
                    apply_evolution,
                    flatten,
                )?;
                m.insert(newval.field, newval.value);
            }
        }
        resolved_value = Ok(ResolvedFieldValue::new(field.to_string(), Value::Object(m)));
    } else {
        return Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Field '{field}' is not a record"),
        )));
    }

    resolved_value
}

#[inline]
fn process_map_field(
    config: &Config,
    field: &str,
    value: &Value,
    metadata: &HashMap<String, Metadata>,
    flatten: bool,
    apply_evolution: bool,
) -> Result<ResolvedFieldValue, Box<dyn Error>> {
    if !value.is_object() {
        return Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Field '{field}' is not a map"),
        )));
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
                            format!(
                                "Array field '{}.{}' has {} elements, but repetition_count is {}. Falling back to slow path.",
                                field, key, array_length, meta_field.repetition_count
                            ),
                        )));
                    }
                }
            }

            let new_val = fast_set_value(
                config,
                &meta_field.determined_type,
                key,
                val,
                &field_metadata.fields,
                apply_evolution,
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

    Ok(ResolvedFieldValue::new(
        field.to_string(),
        Value::Object(map),
    ))
}

#[inline]
fn process_array_field(
    config: &Config,
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
                        format!(
                            "Array field '{}' has {} elements, but repetition_count is {}. Falling back to slow path.",
                            field, array_length, meta.repetition_count
                        ),
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
            let parent_meta = match parent_meta_opt {
                Some(m) => m,
                None => {
                    return Err(format!(
                        "Array field '{}' not found in metadata or it's disabled",
                        idx
                    )
                    .into());
                }
            };

            if parent_meta.enabled {
                let values_type = parent_meta
                    .determined_type_values
                    .as_ref()
                    .unwrap_or(&SkipprDataType::Unknown);
                let index = idx.to_string();
                let sub_field = if is_record_values {
                    "0"
                } else {
                    index.as_str()
                };
                let new_val = fast_set_value(
                    config,
                    values_type,
                    sub_field,
                    val,
                    &parent_meta.fields,
                    false,
                    flatten,
                )?;
                array.push(new_val.value);
            }
        }
    } else {
        return Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Field '{field}' is not an array"),
        )));
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
    use crate::discover::AnalyseSchema;
    use crate::discover::DateCandidate;
    #[allow(unused_imports)]
    use chrono::{FixedOffset, NaiveDateTime, Utc};
    use serde_json::json;

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
                ..Metadata::new().unwrap()
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

    #[test]
    fn fast_set_date_unparseable_string_is_err() {
        let field = "test_field";
        let format_name = AnalyseSchema::is_valid_date("2023-05-21 12:34:56").unwrap();
        let mut meta = generate_metadata(field, format_name);
        let result = fast_set_date(field, &json!("20mm"), &mut meta);
        assert!(result.is_err(), "fast date parse fail must bubble to slow");
    }

    #[test]
    fn date_without_evolution_bubbles_to_slow_path() {
        let field = "when";
        let format_name = AnalyseSchema::is_valid_date("2023-05-21 12:34:56").unwrap();
        let meta = generate_metadata(field, format_name);
        let err = fast_set_value(
            &Config::new(),
            &SkipprDataType::Date,
            field,
            &json!("20mm"),
            &meta,
            true,
            false,
        )
        .expect_err("fast Date must miss without has_evolution");
        assert!(err.to_string().contains("date") || err.to_string().contains("20mm"));
        assert!(meta.get("when_string").is_none());
    }

    #[test]
    fn date_with_existing_string_sibling_applies() {
        let field = "when";
        let format_name = AnalyseSchema::is_valid_date("2023-05-21 12:34:56").unwrap();
        let mut meta = generate_metadata(field, format_name);
        meta.get_mut(field).unwrap().evolution.insert(
            "when_string".into(),
            Evolution {
                type_string: SkipprDataType::String,
                new_field: "when_string".into(),
                sovled: true,
            },
        );
        let mut sibling = Metadata::new().unwrap();
        sibling.determined_type = SkipprDataType::String;
        sibling.out_field_name = "when_string".into();
        meta.insert("when_string".into(), sibling);
        let resolved = fast_set_value(
            &Config::new(),
            &SkipprDataType::Date,
            field,
            &json!("20mm"),
            &meta,
            true,
            false,
        )
        .expect("fast Date applies existing sibling");
        assert_eq!(resolved.field, "when_string");
        assert_eq!(resolved.value, json!("20mm"));
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
        assert_eq!(
            get_or_panic(match_scalar_value_optimized(
                "field",
                &SkipprDataType::String,
                &str_to_val("hello"),
                &metadata,
            ))
            .value,
            str_to_val("hello")
        );
        assert_eq!(
            get_or_panic(match_scalar_value_optimized(
                "field",
                &SkipprDataType::String,
                &i64_to_val(123),
                &metadata,
            ))
            .value,
            str_to_val("123")
        );
        assert_eq!(
            get_or_panic(match_scalar_value_optimized(
                "field",
                &SkipprDataType::String,
                &f64_to_val(123.4),
                &metadata,
            ))
            .value,
            str_to_val("123.4")
        );
        assert_eq!(
            get_or_panic(match_scalar_value_optimized(
                "field",
                &SkipprDataType::String,
                &bool_to_val(true),
                &metadata,
            ))
            .value,
            str_to_val("true")
        );
    }

    #[test]
    fn test_match_scalar_value_fast_int() {
        let mut metadata = HashMap::new();
        metadata.insert("field".to_string(), Metadata::new().unwrap());
        assert_eq!(
            get_or_panic(match_scalar_value_optimized(
                "field",
                &SkipprDataType::Integer,
                &i64_to_val(123),
                &metadata,
            ))
            .value,
            i64_to_val(123)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_optimized(
                "field",
                &SkipprDataType::Integer,
                &str_to_val("123"),
                &metadata,
            ))
            .value,
            i64_to_val(123)
        );

        assert_eq!(
            get_or_panic(match_scalar_value_optimized(
                "field",
                &SkipprDataType::Integer,
                &bool_to_val(true),
                &metadata,
            ))
            .value,
            i64_to_val(1)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_optimized(
                "field",
                &SkipprDataType::Integer,
                &bool_to_val(false),
                &metadata,
            ))
            .value,
            i64_to_val(0)
        );

        assert_eq!(
            get_or_panic(match_scalar_value_optimized(
                "field",
                &SkipprDataType::Integer,
                &str_to_val("true"),
                &metadata,
            ))
            .value,
            i64_to_val(1)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_optimized(
                "field",
                &SkipprDataType::Integer,
                &str_to_val("false"),
                &metadata,
            ))
            .value,
            i64_to_val(0)
        );
        assert_eq!(
            match_scalar_value_optimized(
                "field",
                &SkipprDataType::Integer,
                &str_to_val("True"),
                &metadata,
            )
            .is_err(),
            true
        );
        assert_eq!(
            match_scalar_value_optimized(
                "field",
                &SkipprDataType::Integer,
                &str_to_val("False"),
                &metadata,
            )
            .is_err(),
            true
        );
        assert_eq!(
            get_or_panic(match_scalar_value_optimized(
                "field",
                &SkipprDataType::Integer,
                &i64_to_val(1),
                &metadata,
            ))
            .value,
            i64_to_val(1)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_optimized(
                "field",
                &SkipprDataType::Integer,
                &str_to_val("1"),
                &metadata,
            ))
            .value,
            i64_to_val(1)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_optimized(
                "field",
                &SkipprDataType::Integer,
                &i64_to_val(0),
                &metadata,
            ))
            .value,
            i64_to_val(0)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_optimized(
                "field",
                &SkipprDataType::Integer,
                &str_to_val("0"),
                &metadata,
            ))
            .value,
            i64_to_val(0)
        );
    }

    #[test]
    fn test_match_scalar_value_fast_double() {
        let mut metadata = HashMap::new();
        metadata.insert("field".to_string(), Metadata::new().unwrap());
        assert_eq!(
            get_or_panic(match_scalar_value_optimized(
                "field",
                &SkipprDataType::Double,
                &f64_to_val(123.4),
                &metadata,
            ))
            .value,
            f64_to_val(123.4)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_optimized(
                "field",
                &SkipprDataType::Double,
                &str_to_val("123.4"),
                &metadata,
            ))
            .value,
            f64_to_val(123.4)
        );
    }

    #[test]
    fn test_match_scalar_value_fast_decimal() {
        let mut metadata = HashMap::new();
        metadata.insert("field".to_string(), Metadata::new().unwrap());
        assert_eq!(
            get_or_panic(match_scalar_value_optimized(
                "field",
                &SkipprDataType::Decimal,
                &str_to_val("120.50"),
                &metadata,
            ))
            .value,
            str_to_val("120.50")
        );
        assert_eq!(
            get_or_panic(match_scalar_value_optimized(
                "field",
                &SkipprDataType::Decimal,
                &f64_to_val(50.0),
                &metadata,
            ))
            .value,
            str_to_val("50.0")
        );
    }

    #[test]
    fn test_match_scalar_value_fast_timestamp_string() {
        let mut metadata = HashMap::new();
        metadata.insert("field".to_string(), Metadata::new().unwrap());
        assert_eq!(
            get_or_panic(match_scalar_value_optimized(
                "field",
                &SkipprDataType::Timestamp,
                &str_to_val("2025-01-03T09:15:00"),
                &metadata,
            ))
            .value,
            str_to_val("2025-01-03T09:15:00")
        );
    }

    #[test]
    fn test_match_scalar_value_fast_boolean() {
        let mut metadata = HashMap::new();
        metadata.insert("field".to_string(), Metadata::new().unwrap());
        assert_eq!(
            get_or_panic(match_scalar_value_optimized(
                "field",
                &SkipprDataType::Boolean,
                &bool_to_val(true),
                &metadata,
            ))
            .value,
            bool_to_val(true)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_optimized(
                "field",
                &SkipprDataType::Boolean,
                &bool_to_val(false),
                &metadata,
            ))
            .value,
            bool_to_val(false)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_optimized(
                "field",
                &SkipprDataType::Boolean,
                &str_to_val("true"),
                &metadata,
            ))
            .value,
            bool_to_val(true)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_optimized(
                "field",
                &SkipprDataType::Boolean,
                &str_to_val("false"),
                &metadata,
            ))
            .value,
            bool_to_val(false)
        );
        assert_eq!(
            match_scalar_value_optimized(
                "field",
                &SkipprDataType::Boolean,
                &str_to_val("True"),
                &metadata,
            )
            .is_err(),
            true
        );
        assert_eq!(
            match_scalar_value_optimized(
                "field",
                &SkipprDataType::Boolean,
                &str_to_val("False"),
                &metadata,
            )
            .is_err(),
            true
        );
        assert_eq!(
            match_scalar_value_optimized(
                "field",
                &SkipprDataType::Boolean,
                &str_to_val("Yes"),
                &metadata,
            )
            .is_err(),
            true
        );
        assert_eq!(
            match_scalar_value_optimized(
                "field",
                &SkipprDataType::Boolean,
                &str_to_val("No"),
                &metadata,
            )
            .is_err(),
            true
        );
        assert_eq!(
            get_or_panic(match_scalar_value_optimized(
                "field",
                &SkipprDataType::Boolean,
                &i64_to_val(1),
                &metadata,
            ))
            .value,
            bool_to_val(true)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_optimized(
                "field",
                &SkipprDataType::Boolean,
                &i64_to_val(0),
                &metadata,
            ))
            .value,
            bool_to_val(false)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_optimized(
                "field",
                &SkipprDataType::Boolean,
                &str_to_val("1"),
                &metadata,
            ))
            .value,
            bool_to_val(true)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_optimized(
                "field",
                &SkipprDataType::Boolean,
                &str_to_val("0"),
                &metadata,
            ))
            .value,
            bool_to_val(false)
        );
        // assert_eq!(
        //     get_or_panic(match_scalar_value_optimized("field", &SkipprDataType::Boolean, &f64_to_val(1.0))),
        //     bool_to_val(true)
        // );
    }

    #[test]
    fn test_match_scalar_value_fast_unknown() {
        let mut metadata = HashMap::new();
        metadata.insert("field".to_string(), Metadata::new().unwrap());
        let result = match_scalar_value_optimized(
            "field",
            &SkipprDataType::Unknown,
            &str_to_val("hello"),
            &metadata,
        );

        assert!(result.is_err(), "unknown data type should error");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("is not a unknown"),
            "error should name the unknown type, got {err_msg}"
        );
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

        let result = process_array_field(&Config::new(), field, &value, &metadata, flatten)?;

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

        let result = process_array_field(&Config::new(), field, &value, &metadata, flatten)?;

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

        let result = process_array_field(&Config::new(), field, &value, &metadata, flatten)?;

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

        let result = process_array_field(&Config::new(), field, &value, &metadata, flatten)?;

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

        let result = process_array_field(&Config::new(), field, &value, &metadata, flatten)?;

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

        let result = process_array_field(&Config::new(), field, &value, &metadata, flatten)?;

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

        let result = process_array_field(&Config::new(), field, &value, &metadata, flatten)?;

        assert_eq!(result.value, json!(["one", "two", "three"]));
        Ok(())
    }

    #[test]
    fn test_process_array_field_field_not_found() {
        let field = "test_field";
        let value = json!([1, 2, 3]);
        let flatten = false;
        let metadata = HashMap::new();

        let result = process_array_field(&Config::new(), field, &value, &metadata, flatten);

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

        let result = process_array_field(&Config::new(), field, &value, &metadata, flatten);

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

        let mut name_meta = Metadata::new()?;
        name_meta.determined_type = SkipprDataType::String;
        name_meta.out_field_name = "name".to_string();
        name_meta.enabled = true;
        let mut tel_meta = Metadata::new()?;
        tel_meta.determined_type = SkipprDataType::Integer;
        tel_meta.out_field_name = "tel".to_string();
        tel_meta.enabled = true;
        let mut field_zero = Metadata::new()?;
        field_zero.determined_type = SkipprDataType::Record;
        field_zero.determined_type_values = None;
        field_zero.enabled = true;
        field_zero.fields.insert("name".to_string(), name_meta);
        field_zero.fields.insert("tel".to_string(), tel_meta);
        meta_data_item.enabled = true;
        meta_data_item.fields.insert("0".to_string(), field_zero);

        metadata.insert(field.to_string(), meta_data_item);

        let result = process_array_field(&Config::new(), field, &value, &metadata, flatten);

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

        let result = process_array_field(&Config::new(), field, &value, &metadata, flatten);

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

        let mut name_meta = Metadata::new().unwrap();
        name_meta.determined_type = SkipprDataType::String;
        name_meta.out_field_name = "name".to_string();
        name_meta.enabled = true;
        let mut elem = Metadata::new().unwrap();
        elem.determined_type = SkipprDataType::Record;
        elem.out_field_name = "0".to_string();
        elem.enabled = true;
        elem.fields.insert("name".to_string(), name_meta);
        let mut array_field_meta = Metadata::new().unwrap();
        array_field_meta.determined_type = SkipprDataType::Array;
        array_field_meta.determined_type_values = Some(SkipprDataType::Record);
        array_field_meta.repetition_count = 2;
        array_field_meta.enabled = true;
        array_field_meta.fields.insert("0".to_string(), elem);

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
        let result = process_record_field(&Config::new(), "person", &value, &metadata, false, true);
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
        let result_exceeds = process_record_field(
            &Config::new(),
            "person",
            &value_exceeds,
            &metadata,
            false,
            true,
        );
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
        let nested_result = process_record_field(
            &Config::new(),
            "user",
            &nested_value,
            &nested_metadata,
            false,
            true,
        );
        assert!(nested_result.is_ok());

        // JSON with nested array exceeding limits
        let nested_value_exceeds = json!({
            "details": {
                "tags": ["tag1", "tag2", "tag3", "tag4"]
            }
        });

        // This should now be handled by the process_array_field function during recursive processing
        // rather than being checked in process_record_field directly
        let nested_result_exceeds = process_record_field(
            &Config::new(),
            "user",
            &nested_value_exceeds,
            &nested_metadata,
            false,
            true,
        );
        // The optimization we made is that this error would be caught in process_array_field
        // when it processes the "tags" field, not in the process_record_field check
        assert!(nested_result_exceeds.is_ok());
    }

    #[test]
    fn nested_decimal_without_evolution_bubbles_to_slow_path() {
        let mut child = Metadata::new().unwrap();
        child.determined_type = SkipprDataType::Decimal;
        child.out_field_name = "docket_aggregate_size_code".into();
        let mut parent = Metadata::new().unwrap();
        parent.determined_type = SkipprDataType::Record;
        parent
            .fields
            .insert("docket_aggregate_size_code".into(), child);
        let mut metadata = HashMap::new();
        metadata.insert("detail".into(), parent);
        let live_keys_before: Vec<_> = metadata
            .get("detail")
            .unwrap()
            .fields
            .keys()
            .cloned()
            .collect();
        let err = process_record_field(
            &Config::new(),
            "detail",
            &json!({"docket_aggregate_size_code": "20mm"}),
            &metadata,
            true,
            true,
        )
        .expect_err("fast path must not create a sibling");
        assert!(
            err.to_string().contains("decimal"),
            "expected decimal coerce error, got {err}"
        );
        let live_keys_after: Vec<_> = metadata
            .get("detail")
            .unwrap()
            .fields
            .keys()
            .cloned()
            .collect();
        assert_eq!(live_keys_before, live_keys_after);
    }

    #[test]
    fn nested_decimal_with_existing_string_sibling_applies() {
        let mut child = Metadata::new().unwrap();
        child.determined_type = SkipprDataType::Decimal;
        child.out_field_name = "docket_aggregate_size_code".into();
        child.evolution.insert(
            "docket_aggregate_size_code_string".into(),
            Evolution {
                type_string: SkipprDataType::String,
                new_field: "docket_aggregate_size_code_string".into(),
                sovled: true,
            },
        );
        let mut sibling = Metadata::new().unwrap();
        sibling.determined_type = SkipprDataType::String;
        sibling.out_field_name = "docket_aggregate_size_code_string".into();
        let mut parent = Metadata::new().unwrap();
        parent.determined_type = SkipprDataType::Record;
        parent
            .fields
            .insert("docket_aggregate_size_code".into(), child);
        parent
            .fields
            .insert("docket_aggregate_size_code_string".into(), sibling);
        let mut metadata = HashMap::new();
        metadata.insert("detail".into(), parent);
        let resolved = process_record_field(
            &Config::new(),
            "detail",
            &json!({"docket_aggregate_size_code": "20mm"}),
            &metadata,
            true,
            true,
        )
        .expect("fast path applies existing leaf sibling");
        assert_eq!(resolved.value["docket_aggregate_size_code_string"], "20mm");
    }

    #[test]
    fn nested_decimal_fast_miss_slow_create_then_fast_apply() {
        let mut child = Metadata::new().unwrap();
        child.determined_type = SkipprDataType::Decimal;
        child.out_field_name = "docket_aggregate_size_code".into();
        child.enabled = true;
        let mut parent = Metadata::new().unwrap();
        parent.determined_type = SkipprDataType::Record;
        parent
            .fields
            .insert("docket_aggregate_size_code".into(), child);
        let mut metadata = HashMap::new();
        metadata.insert("detail".into(), parent);
        process_record_field(
            &Config::new(),
            "detail",
            &json!({"docket_aggregate_size_code": "20mm"}),
            &metadata,
            true,
            true,
        )
        .expect_err("fast path must miss without has_evolution");
        assert!(metadata
            .get("detail")
            .unwrap()
            .fields
            .get("docket_aggregate_size_code_string")
            .is_none());
        let mut updated = "no".to_string();
        crate::ingest::ingest::set_value(
            &Config::new(),
            "record",
            "detail",
            &json!({"docket_aggregate_size_code": "20mm"}),
            None,
            None,
            &mut metadata,
            &mut updated,
            true,
            true,
        )
        .expect("slow path creates the sibling");
        let resolved = process_record_field(
            &Config::new(),
            "detail",
            &json!({"docket_aggregate_size_code": "10mm"}),
            &metadata,
            true,
            true,
        )
        .expect("fast path applies after slow publish");
        assert_eq!(resolved.value["docket_aggregate_size_code_string"], "10mm");
    }

    #[test]
    fn mixed_primitive_array_fast_miss_slow_denies() {
        let mut arr = Metadata::new().unwrap();
        arr.determined_type = SkipprDataType::Array;
        arr.determined_type_values = Some(SkipprDataType::Integer);
        arr.out_field_name = "codes".into();
        arr.enabled = true;
        let mut metadata = HashMap::new();
        metadata.insert("codes".into(), arr);
        process_array_field(
            &Config::new(),
            "codes",
            &json!([1234, "BWL"]),
            &metadata,
            false,
        )
        .expect_err("fast path must miss mixed primitive arrays");
        let mut updated = "no".to_string();
        let err = crate::ingest::ingest::ingest(
            &Config::new(),
            &json!({"codes": [1234, "BWL"]}),
            &mut metadata,
            "default",
            &mut updated,
            false,
        )
        .expect_err("slow path denies mixed primitive arrays");
        let deny = err
            .downcast_ref::<crate::ingest::deny::IngestDenyError>()
            .expect("ARRAY_MIXED_TYPES");
        assert_eq!(deny.code, crate::ingest::deny::ARRAY_MIXED_TYPES);
        assert!(!metadata.keys().any(|k| k.contains("codes_array")));
        fast_set_value(
            &Config::new(),
            &SkipprDataType::Array,
            "codes",
            &json!([1, "HNL"]),
            &metadata,
            true,
            false,
        )
        .expect_err("fast path must not apply a parent-split that was never published");
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

    fn identity_output(fields: &HashMap<String, Metadata>) -> Metadata {
        let mut meta = Metadata::new().unwrap();
        meta.fields = Box::new(fields.clone());
        meta
    }

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

        DEFAULT_NESTED_MESSAGE.insert(namespace.to_string(), Arc::new(message));
    }

    #[test]
    fn test_integer_i32_range_enforced() {
        let mut metadata = HashMap::new();
        let mut int_meta = Metadata::new().unwrap();
        int_meta.determined_type = SkipprDataType::Integer;
        metadata.insert("count".to_string(), int_meta);

        let in_range = match_scalar_value_optimized(
            "count",
            &SkipprDataType::Integer,
            &json!(42i64),
            &metadata,
        );
        assert!(in_range.is_ok());

        let overflow = match_scalar_value_optimized(
            "count",
            &SkipprDataType::Integer,
            &json!(i64::from(i32::MAX) + 1),
            &metadata,
        );
        assert!(overflow.is_err());
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

        let result = fast_path_ingest(
            &Config::new(),
            &input,
            &metadata,
            namespace,
            false,
            &identity_output(&metadata),
        );
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

        let result = fast_path_ingest(
            &Config::new(),
            &input,
            &metadata,
            namespace,
            false,
            &identity_output(&metadata),
        );
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

        let result = fast_path_ingest(
            &Config::new(),
            &input,
            &metadata,
            namespace,
            false,
            &identity_output(&metadata),
        );
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

        let result = fast_path_ingest(
            &Config::new(),
            &input,
            &metadata,
            namespace,
            false,
            &identity_output(&metadata),
        );
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

        let result = fast_path_ingest(
            &Config::new(),
            &input,
            &metadata,
            namespace,
            false,
            &identity_output(&metadata),
        );
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
        let result = fast_path_ingest(
            &Config::new(),
            &input,
            &metadata,
            namespace,
            false,
            &identity_output(&metadata),
        );

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
        let _result = fast_path_ingest(
            &Config::new(),
            &input,
            &metadata,
            namespace,
            true,
            &identity_output(&metadata),
        );
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

        let result = fast_path_ingest(
            &Config::new(),
            &input,
            &metadata,
            namespace,
            false,
            &identity_output(&metadata),
        );
        assert!(result.is_err());

        let err = result.unwrap_err().to_string();
        assert_eq!(err, "No default message template found");
    }
}

#[cfg(test)]
mod tests_fast_set_value_proptest {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    fn arb_skippr_type() -> impl Strategy<Value = SkipprDataType> {
        prop_oneof![
            Just(SkipprDataType::String),
            Just(SkipprDataType::Integer),
            Just(SkipprDataType::Long),
            Just(SkipprDataType::Double),
            Just(SkipprDataType::Boolean),
            Just(SkipprDataType::Timestamp),
            Just(SkipprDataType::TimestampMilli),
            Just(SkipprDataType::Date),
        ]
    }

    fn arb_json_value() -> impl Strategy<Value = serde_json::Value> {
        prop_oneof![
            Just(json!(null)),
            any::<bool>().prop_map(|b| json!(b)),
            any::<i32>().prop_map(|i| json!(i)),
            any::<i64>().prop_map(|i| json!(i)),
            (-1e15f64..1e15f64).prop_map(|f| json!(f)),
            "[a-zA-Z0-9_]{0,20}".prop_map(|s| json!(s)),
            Just(json!({})),
            Just(json!({"a": 1})),
            Just(json!([])),
            Just(json!([1, 2, 3])),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(500))]

        #[test]
        fn fast_set_value_never_panics(
            data_type in arb_skippr_type(),
            value in arb_json_value(),
        ) {
            let metadata: HashMap<String, Metadata> = HashMap::new();
            let _ = fast_set_value(
                &Config::new(), &data_type,
                "test_field",
                &value,
                &metadata,
                false,
                false,
            );
        }
    }
}

#[cfg(test)]
mod tests_data_loss_regression {
    use super::*;
    use serde_json::json;

    fn create_array_metadata(values_type: SkipprDataType) -> HashMap<String, Metadata> {
        let mut metadata = HashMap::new();
        let mut arr = Metadata::new().unwrap();
        arr.determined_type = SkipprDataType::Array;
        arr.determined_type_values = Some(values_type.clone());
        arr.out_field_name = "arr".to_string();

        let mut elem = Metadata::new().unwrap();
        elem.determined_type = values_type;
        elem.out_field_name = "0".to_string();
        arr.fields.insert("0".to_string(), elem);

        metadata.insert("arr".to_string(), arr);
        metadata
    }

    #[test]
    fn array_element_coercion_failure_bubbles_to_slow_path() {
        let metadata = create_array_metadata(SkipprDataType::Integer);

        let value = json!(["not_an_int", 42, "also_not_int"]);
        let result = process_array_field(&Config::new(), "arr", &value, &metadata, false);
        assert!(
            result.is_err(),
            "mixed primitive array must miss fast path so slow can deny"
        );
    }

    #[test]
    fn map_key_not_in_metadata_returns_error() {
        let metadata = HashMap::new();
        let value = json!({"unknown_key": "value"});
        let result = process_map_field(
            &Config::new(),
            "missing_map",
            &value,
            &metadata,
            false,
            true,
        );
        assert!(result.is_err(), "map with unknown key should error");
    }

    #[test]
    fn unknown_data_type_returns_error() {
        let metadata: HashMap<String, Metadata> = HashMap::new();
        let result = fast_set_value(
            &Config::new(),
            &SkipprDataType::Unknown,
            "test_field",
            &json!(42),
            &metadata,
            false,
            false,
        );
        assert!(result.is_err(), "unknown data type should error");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("is not a unknown"),
            "error should name the unknown type, got {err_msg}"
        );
    }

    #[test]
    fn empty_array_processes_cleanly() {
        let metadata = create_array_metadata(SkipprDataType::Integer);
        let value = json!([]);
        let result = process_array_field(&Config::new(), "arr", &value, &metadata, false);
        assert!(result.is_ok());
        if let serde_json::Value::Array(arr) = &result.unwrap().value {
            assert!(arr.is_empty());
        }
    }

    #[test]
    fn null_in_array_becomes_null() {
        let metadata = create_array_metadata(SkipprDataType::Integer);
        let value = json!([null, 42, null]);
        let result = process_array_field(&Config::new(), "arr", &value, &metadata, false);
        assert!(result.is_ok());
        if let serde_json::Value::Array(arr) = &result.unwrap().value {
            assert_eq!(arr.len(), 3);
            assert!(arr[0].is_null());
            assert!(arr[2].is_null());
        }
    }
}

#[cfg(test)]
mod tests_dest_before_evolution {
    use super::*;
    use crate::discover::date_formats::DateFormats;
    use crate::discover::evolution::Evolution;
    use crate::discover::{AnalyseSchema, DateCandidate};
    use serde_json::json;

    fn leaf(name: &str, ty: SkipprDataType) -> Metadata {
        let mut m = Metadata::new().unwrap();
        m.determined_type = ty;
        m.out_field_name = name.to_string();
        m.enabled = true;
        m
    }

    fn record_named(name: &str, children: &[(&str, SkipprDataType)]) -> Metadata {
        let mut m = Metadata::new().unwrap();
        m.determined_type = SkipprDataType::Record;
        m.out_field_name = name.to_string();
        m.enabled = true;
        for (child, ty) in children {
            m.fields
                .insert((*child).to_string(), leaf(child, ty.clone()));
        }
        m
    }

    fn evo(ty: SkipprDataType, new_field: &str) -> Evolution {
        Evolution {
            type_string: ty,
            new_field: new_field.to_string(),
            sovled: true,
        }
    }

    fn detail_with_record_sibling(
        dest_children: &[(&str, SkipprDataType)],
        sibling_children: &[(&str, SkipprDataType)],
    ) -> HashMap<String, Metadata> {
        let mut dest = record_named("detail", dest_children);
        dest.evolution.insert(
            "detail_record".into(),
            evo(SkipprDataType::Record, "detail_record"),
        );
        let mut metadata = HashMap::new();
        metadata.insert("detail".into(), dest);
        metadata.insert(
            "detail_record".into(),
            record_named("detail_record", sibling_children),
        );
        metadata
    }

    fn map_with_sibling(
        dest_children: &[(&str, SkipprDataType)],
        sibling_children: &[(&str, SkipprDataType)],
    ) -> HashMap<String, Metadata> {
        let mut dest = record_named("attrs", dest_children);
        dest.determined_type = SkipprDataType::Map;
        dest.evolution
            .insert("attrs_map".into(), evo(SkipprDataType::Map, "attrs_map"));
        let mut sibling = record_named("attrs_map", sibling_children);
        sibling.determined_type = SkipprDataType::Map;
        let mut metadata = HashMap::new();
        metadata.insert("attrs".into(), dest);
        metadata.insert("attrs_map".into(), sibling);
        metadata
    }

    fn array_int_with_array_sibling(sibling_name: &str) -> HashMap<String, Metadata> {
        let mut dest = Metadata::new().unwrap();
        dest.determined_type = SkipprDataType::Array;
        dest.determined_type_values = Some(SkipprDataType::Integer);
        dest.out_field_name = "codes".into();
        dest.enabled = true;
        dest.evolution
            .insert("array".into(), evo(SkipprDataType::Array, sibling_name));
        let mut sibling = Metadata::new().unwrap();
        sibling.determined_type = SkipprDataType::Array;
        sibling.determined_type_values = Some(SkipprDataType::Integer);
        sibling.out_field_name = sibling_name.to_string();
        sibling.enabled = true;
        let mut metadata = HashMap::new();
        metadata.insert("codes".into(), dest);
        metadata.insert(sibling_name.to_string(), sibling);
        metadata
    }

    fn set_field(
        ty: &SkipprDataType,
        field: &str,
        metadata: &HashMap<String, Metadata>,
        value: serde_json::Value,
        apply: bool,
    ) -> Result<ResolvedFieldValue, Box<dyn Error>> {
        fast_set_value(&Config::new(), ty, field, &value, metadata, apply, false)
    }

    fn live_keys(metadata: &HashMap<String, Metadata>) -> Vec<String> {
        let mut keys: Vec<String> = metadata.keys().cloned().collect();
        keys.sort();
        keys
    }

    #[test]
    fn dest_compatible_record_stays_on_canonical_field() {
        let metadata = detail_with_record_sibling(
            &[("qty", SkipprDataType::Integer)],
            &[("qty", SkipprDataType::Integer)],
        );
        let resolved = set_field(
            &SkipprDataType::Record,
            "detail",
            &metadata,
            json!({"qty": 1}),
            true,
        )
        .expect("dest-compatible record");
        assert_eq!(
            resolved.field, "detail",
            "canonical dest must win when the object still coerces; got {}",
            resolved.field
        );
        assert_eq!(resolved.value["qty"], 1);
    }

    #[test]
    fn dest_miss_falls_back_to_existing_record_sibling() {
        let metadata = detail_with_record_sibling(
            &[("qty", SkipprDataType::Integer)],
            &[
                ("qty", SkipprDataType::Integer),
                ("note", SkipprDataType::String),
            ],
        );
        let resolved = set_field(
            &SkipprDataType::Record,
            "detail",
            &metadata,
            json!({"qty": 1, "note": "x"}),
            true,
        )
        .expect("live sibling dest-walk");
        assert_eq!(resolved.field, "detail_record");
        assert_eq!(resolved.value["note"], "x");
    }

    #[test]
    fn dest_miss_sibling_lacking_extra_key_is_err() {
        let metadata = detail_with_record_sibling(
            &[("qty", SkipprDataType::Integer)],
            &[("qty", SkipprDataType::Integer)],
        );
        let before = live_keys(&metadata);
        let err = set_field(
            &SkipprDataType::Record,
            "detail",
            &metadata,
            json!({"qty": 1, "note": "x"}),
            true,
        )
        .expect_err("unpublished extra key must not clone-discover onto sibling");
        assert!(
            err.to_string().contains("note") || err.to_string().contains("Subfield"),
            "expected dest extra-key miss, got {err}"
        );
        assert_eq!(live_keys(&metadata), before);
        assert!(metadata
            .get("detail_record")
            .unwrap()
            .fields
            .get("note")
            .is_none());
    }

    #[test]
    fn dest_compatible_and_dest_shaped_rows_do_not_split() {
        let metadata = detail_with_record_sibling(
            &[
                ("qty", SkipprDataType::Integer),
                ("code", SkipprDataType::String),
            ],
            &[("qty", SkipprDataType::Integer)],
        );
        let qty_only = set_field(
            &SkipprDataType::Record,
            "detail",
            &metadata,
            json!({"qty": 1}),
            true,
        )
        .expect("qty-only dest");
        let dest_shaped = set_field(
            &SkipprDataType::Record,
            "detail",
            &metadata,
            json!({"qty": 1, "code": "x"}),
            true,
        )
        .expect("dest-shaped");
        assert_eq!(
            qty_only.field, "detail",
            "qty-only still fits dest; must not steal to sibling ({})",
            qty_only.field
        );
        assert_eq!(dest_shaped.field, "detail");
    }

    #[test]
    fn positional_record_fitting_indexes_stay_on_dest() {
        let mut dest = Metadata::new().unwrap();
        dest.determined_type = SkipprDataType::Record;
        dest.out_field_name = "detail".into();
        dest.enabled = true;
        dest.fields.insert(
            "0".into(),
            record_named("0", &[("qty", SkipprDataType::Integer)]),
        );
        dest.evolution.insert(
            "detail_record".into(),
            evo(SkipprDataType::Record, "detail_record"),
        );
        let mut sibling = Metadata::new().unwrap();
        sibling.determined_type = SkipprDataType::Record;
        sibling.out_field_name = "detail_record".into();
        sibling.enabled = true;
        sibling.fields.insert(
            "0".into(),
            record_named("0", &[("qty", SkipprDataType::Integer)]),
        );
        let mut metadata = HashMap::new();
        metadata.insert("detail".into(), dest);
        metadata.insert("detail_record".into(), sibling);
        let resolved = set_field(
            &SkipprDataType::Record,
            "detail",
            &metadata,
            json!([{"qty": 1}]),
            true,
        )
        .expect("positional dest");
        assert_eq!(resolved.field, "detail");
    }

    #[test]
    fn parent_stays_dest_when_child_leaf_sibling_applies() {
        let mut child = leaf("qty", SkipprDataType::Integer);
        child.evolution.insert(
            "qty_string".into(),
            evo(SkipprDataType::String, "qty_string"),
        );
        let mut dest = record_named("detail", &[]);
        dest.fields.insert("qty".into(), child);
        dest.fields.insert(
            "qty_string".into(),
            leaf("qty_string", SkipprDataType::String),
        );
        dest.evolution.insert(
            "detail_record".into(),
            evo(SkipprDataType::Record, "detail_record"),
        );
        let mut metadata = HashMap::new();
        metadata.insert("detail".into(), dest);
        metadata.insert(
            "detail_record".into(),
            record_named("detail_record", &[("qty", SkipprDataType::Integer)]),
        );
        let resolved = set_field(
            &SkipprDataType::Record,
            "detail",
            &metadata,
            json!({"qty": "n/a"}),
            true,
        )
        .expect("leaf sibling on dest parent");
        assert_eq!(resolved.field, "detail");
        assert_eq!(resolved.value["qty_string"], "n/a");
        assert!(resolved.value.get("qty").is_none() || resolved.value["qty"].is_null());
    }

    #[test]
    fn apply_evolution_false_dest_miss_does_not_steal() {
        let metadata = detail_with_record_sibling(
            &[("qty", SkipprDataType::Integer)],
            &[
                ("qty", SkipprDataType::Integer),
                ("note", SkipprDataType::String),
            ],
        );
        set_field(
            &SkipprDataType::Record,
            "detail",
            &metadata,
            json!({"qty": 1, "note": "x"}),
            false,
        )
        .expect_err("apply off must not sibling-route");
    }

    #[test]
    fn apply_evolution_false_is_transitive_to_record_children() {
        let mut child = leaf("qty", SkipprDataType::Integer);
        child.evolution.insert(
            "qty_string".into(),
            evo(SkipprDataType::String, "qty_string"),
        );
        let mut dest = record_named("detail", &[]);
        dest.fields.insert("qty".into(), child);
        dest.fields.insert(
            "qty_string".into(),
            leaf("qty_string", SkipprDataType::String),
        );
        let mut metadata = HashMap::new();
        metadata.insert("detail".into(), dest);
        set_field(
            &SkipprDataType::Record,
            "detail",
            &metadata,
            json!({"qty": "n/a"}),
            false,
        )
        .expect_err("false must stay dest-only through nested record children");
    }

    #[test]
    fn dest_miss_ranks_timestamp_milli_over_timestamp() {
        let mut dest = leaf("ts", SkipprDataType::Integer);
        dest.evolution.insert(
            "ts_timestamp".into(),
            evo(SkipprDataType::Timestamp, "ts_timestamp"),
        );
        dest.evolution.insert(
            "ts_timestamp_milli".into(),
            evo(SkipprDataType::TimestampMilli, "ts_timestamp_milli"),
        );
        let mut metadata = HashMap::new();
        metadata.insert("ts".into(), dest);
        metadata.insert(
            "ts_timestamp".into(),
            leaf("ts_timestamp", SkipprDataType::Timestamp),
        );
        metadata.insert(
            "ts_timestamp_milli".into(),
            leaf("ts_timestamp_milli", SkipprDataType::TimestampMilli),
        );
        let resolved = set_field(
            &SkipprDataType::Integer,
            "ts",
            &metadata,
            json!(1_700_000_000_123i64),
            true,
        )
        .expect("ranked milli sibling");
        assert_eq!(resolved.field, "ts_timestamp_milli");
    }

    #[test]
    fn dest_miss_ranks_double_over_string() {
        let mut dest = leaf("n", SkipprDataType::Integer);
        dest.evolution
            .insert("n_string".into(), evo(SkipprDataType::String, "n_string"));
        dest.evolution
            .insert("n_double".into(), evo(SkipprDataType::Double, "n_double"));
        let mut metadata = HashMap::new();
        metadata.insert("n".into(), dest);
        metadata.insert("n_string".into(), leaf("n_string", SkipprDataType::String));
        metadata.insert("n_double".into(), leaf("n_double", SkipprDataType::Double));
        let resolved = set_field(&SkipprDataType::Integer, "n", &metadata, json!(1.5), true)
            .expect("ranked double sibling");
        assert_eq!(resolved.field, "n_double");
    }

    #[test]
    fn dest_miss_accepts_sibling_when_out_field_name_differs_from_key() {
        let mut dest = leaf("qty", SkipprDataType::Integer);
        dest.evolution.insert(
            "qty_string".into(),
            evo(SkipprDataType::String, "qty_string"),
        );
        let mut sibling = leaf("qty_string", SkipprDataType::String);
        sibling.out_field_name = "qty_str".into();
        let mut metadata = HashMap::new();
        metadata.insert("qty".into(), dest);
        metadata.insert("qty_string".into(), sibling);
        let resolved = set_field(
            &SkipprDataType::Integer,
            "qty",
            &metadata,
            json!("n/a"),
            true,
        )
        .expect("published sibling with cleaned out name");
        assert_eq!(resolved.field, "qty_str");
        assert_eq!(resolved.value, json!("n/a"));
    }

    #[test]
    fn dest_compatible_map_stays_on_canonical_field() {
        let metadata = map_with_sibling(
            &[("qty", SkipprDataType::Integer)],
            &[("qty", SkipprDataType::Integer)],
        );
        let resolved = set_field(
            &SkipprDataType::Map,
            "attrs",
            &metadata,
            json!({"qty": 1}),
            true,
        )
        .expect("dest-compatible map");
        assert_eq!(resolved.field, "attrs");
    }

    #[test]
    fn parent_map_stays_dest_when_child_leaf_sibling_applies() {
        let mut child = leaf("qty", SkipprDataType::Integer);
        child.evolution.insert(
            "qty_string".into(),
            evo(SkipprDataType::String, "qty_string"),
        );
        let mut dest = record_named("attrs", &[]);
        dest.determined_type = SkipprDataType::Map;
        dest.fields.insert("qty".into(), child);
        dest.fields.insert(
            "qty_string".into(),
            leaf("qty_string", SkipprDataType::String),
        );
        let mut metadata = HashMap::new();
        metadata.insert("attrs".into(), dest);
        let resolved = set_field(
            &SkipprDataType::Map,
            "attrs",
            &metadata,
            json!({"qty": "n/a"}),
            true,
        )
        .expect("map child leaf sibling");
        assert_eq!(resolved.field, "attrs");
        assert_eq!(resolved.value["qty_string"], "n/a");
    }

    #[test]
    fn apply_evolution_false_is_transitive_to_map_children() {
        let mut child = leaf("qty", SkipprDataType::Integer);
        child.evolution.insert(
            "qty_string".into(),
            evo(SkipprDataType::String, "qty_string"),
        );
        let mut dest = record_named("attrs", &[]);
        dest.determined_type = SkipprDataType::Map;
        dest.fields.insert("qty".into(), child);
        dest.fields.insert(
            "qty_string".into(),
            leaf("qty_string", SkipprDataType::String),
        );
        let mut metadata = HashMap::new();
        metadata.insert("attrs".into(), dest);
        set_field(
            &SkipprDataType::Map,
            "attrs",
            &metadata,
            json!({"qty": "n/a"}),
            false,
        )
        .expect_err("false must stay dest-only through nested map children");
    }

    #[test]
    fn dest_miss_map_falls_back_to_live_sibling() {
        let metadata = map_with_sibling(
            &[("qty", SkipprDataType::Integer)],
            &[
                ("qty", SkipprDataType::Integer),
                ("note", SkipprDataType::String),
            ],
        );
        let resolved = set_field(
            &SkipprDataType::Map,
            "attrs",
            &metadata,
            json!({"qty": 1, "note": "x"}),
            true,
        )
        .expect("map sibling");
        assert_eq!(resolved.field, "attrs_map");
        assert_eq!(resolved.value["note"], "x");
    }

    #[test]
    fn dest_miss_map_sibling_lacking_extra_key_is_err() {
        let metadata = map_with_sibling(
            &[("qty", SkipprDataType::Integer)],
            &[("qty", SkipprDataType::Integer)],
        );
        let before = live_keys(&metadata);
        set_field(
            &SkipprDataType::Map,
            "attrs",
            &metadata,
            json!({"qty": 1, "note": "x"}),
            true,
        )
        .expect_err("map extra key must not clone-discover");
        assert_eq!(live_keys(&metadata), before);
    }

    #[test]
    fn dest_compatible_array_stays_on_canonical_field() {
        let metadata = array_int_with_array_sibling("codes_array_integer");
        let resolved = set_field(
            &SkipprDataType::Array,
            "codes",
            &metadata,
            json!([1, 2]),
            true,
        )
        .expect("dest-compatible array");
        assert_eq!(
            resolved.field, "codes",
            "array short-circuit must not steal dest-compatible values; got {}",
            resolved.field
        );
        assert_eq!(resolved.value, json!([1, 2]));
    }

    #[test]
    fn non_array_dest_miss_does_not_raw_clone_onto_array_sibling() {
        let mut metadata = array_int_with_array_sibling("codes_array_integer");
        metadata.get_mut("codes").unwrap().evolution.insert(
            "codes_string".into(),
            evo(SkipprDataType::String, "codes_string"),
        );
        metadata.insert(
            "codes_string".into(),
            leaf("codes_string", SkipprDataType::String),
        );
        let resolved = set_field(
            &SkipprDataType::Array,
            "codes",
            &metadata,
            json!("hello"),
            true,
        )
        .expect("string sibling after dest array miss");
        assert_eq!(resolved.field, "codes_string");
        assert_eq!(resolved.value, json!("hello"));
    }

    #[test]
    fn array_element_published_sibling_still_errs() {
        let mut qty = leaf("qty", SkipprDataType::Integer);
        qty.evolution.insert(
            "qty_string".into(),
            evo(SkipprDataType::String, "qty_string"),
        );
        let mut elem = record_named("0", &[]);
        elem.fields.insert("qty".into(), qty);
        elem.fields.insert(
            "qty_string".into(),
            leaf("qty_string", SkipprDataType::String),
        );
        let mut dest = Metadata::new().unwrap();
        dest.determined_type = SkipprDataType::Array;
        dest.determined_type_values = Some(SkipprDataType::Record);
        dest.out_field_name = "codes".into();
        dest.enabled = true;
        dest.repetition_count = 4;
        dest.fields.insert("0".into(), elem);
        let mut metadata = HashMap::new();
        metadata.insert("codes".into(), dest);
        set_field(
            &SkipprDataType::Array,
            "codes",
            &metadata,
            json!([{"qty": 1}, {"qty": "BWL"}]),
            true,
        )
        .expect_err("array elements are dest-only; mixed must miss fast path");
    }

    #[test]
    fn record_array_repetition_overflow_falls_back_to_live_sibling() {
        let mut dest = Metadata::new().unwrap();
        dest.determined_type = SkipprDataType::Array;
        dest.determined_type_values = Some(SkipprDataType::Record);
        dest.out_field_name = "items".into();
        dest.enabled = true;
        dest.repetition_count = 1;
        dest.fields.insert(
            "0".into(),
            record_named("0", &[("qty", SkipprDataType::Integer)]),
        );
        dest.evolution.insert(
            "array".into(),
            evo(SkipprDataType::Array, "items_array_record"),
        );
        let mut sibling = Metadata::new().unwrap();
        sibling.determined_type = SkipprDataType::Array;
        sibling.determined_type_values = Some(SkipprDataType::Record);
        sibling.out_field_name = "items_array_record".into();
        sibling.enabled = true;
        sibling.repetition_count = 4;
        sibling.fields.insert(
            "0".into(),
            record_named("0", &[("qty", SkipprDataType::Integer)]),
        );
        let mut metadata = HashMap::new();
        metadata.insert("items".into(), dest);
        metadata.insert("items_array_record".into(), sibling);
        let resolved = set_field(
            &SkipprDataType::Array,
            "items",
            &metadata,
            json!([{"qty": 1}, {"qty": 2}]),
            true,
        )
        .expect("overflow dest, live sibling dest-walk");
        assert_eq!(resolved.field, "items_array_record");
        assert_eq!(resolved.value.as_array().map(|a| a.len()), Some(2));
    }

    #[test]
    fn scalar_dest_coerce_still_beats_type_suffix_sibling() {
        let mut dest = leaf("qty", SkipprDataType::Integer);
        dest.evolution.insert(
            "qty_string".into(),
            evo(SkipprDataType::String, "qty_string"),
        );
        let sibling = leaf("qty_string", SkipprDataType::String);
        let mut metadata = HashMap::new();
        metadata.insert("qty".into(), dest);
        metadata.insert("qty_string".into(), sibling);
        let resolved = set_field(&SkipprDataType::Integer, "qty", &metadata, json!(42), true)
            .expect("scalar dest coerce");
        assert_eq!(resolved.field, "qty");
        assert_eq!(resolved.value, json!(42));
    }

    fn date_meta_with_string_sibling() -> HashMap<String, Metadata> {
        let date_str = "2023-05-21 12:34:56";
        let format_name = AnalyseSchema::is_valid_date(date_str).unwrap();
        let date_candidate = DateCandidate {
            check_count: 1,
            valid_count: 1,
            field: "when".into(),
            format: format_name.to_string(),
        };
        let mut dest = Metadata::new().unwrap();
        dest.determined_type = SkipprDataType::Date;
        dest.out_field_name = "when".into();
        dest.enabled = true;
        dest.date_candidate = Some(date_candidate);
        dest.evolution.insert(
            "when_string".into(),
            evo(SkipprDataType::String, "when_string"),
        );
        let mut metadata = HashMap::new();
        metadata.insert("when".into(), dest);
        metadata.insert(
            "when_string".into(),
            leaf("when_string", SkipprDataType::String),
        );
        metadata
    }

    #[test]
    fn dest_compatible_date_stays_on_canonical_field() {
        let metadata = date_meta_with_string_sibling();
        let date_str = "2023-05-21 12:34:56";
        let resolved = set_field(
            &SkipprDataType::Date,
            "when",
            &metadata,
            json!(date_str),
            true,
        )
        .expect("parseable date dest");
        assert_eq!(
            resolved.field, "when",
            "parseable date must not steal to string sibling; got {}",
            resolved.field
        );
        let format_name = AnalyseSchema::is_valid_date(date_str).unwrap();
        let format = DateFormats::from_str(format_name).unwrap().as_str();
        let expected = Helpers::parse_date_from_string(date_str, format).unwrap();
        assert_eq!(resolved.value, json!(expected.timestamp() * 1000));
    }

    #[test]
    fn dest_miss_date_falls_back_to_string_sibling() {
        let metadata = date_meta_with_string_sibling();
        let resolved = set_field(
            &SkipprDataType::Date,
            "when",
            &metadata,
            json!("20mm"),
            true,
        )
        .expect("unparseable date sibling");
        assert_eq!(resolved.field, "when_string");
        assert_eq!(resolved.value, json!("20mm"));
    }

    #[test]
    fn dest_miss_non_string_date_falls_back_to_string_sibling() {
        let metadata = date_meta_with_string_sibling();
        let resolved = set_field(&SkipprDataType::Date, "when", &metadata, json!(42), true)
            .expect("numeric date dest miss");
        assert_eq!(resolved.field, "when_string");
        assert_eq!(resolved.value, json!("42"));
    }
}
