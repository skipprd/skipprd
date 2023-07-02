use std::collections::HashMap;
use std::io::{Read};
use serde_json::Value;
use std::error::Error;
use serde_json::Map;

use crate::discover::Metadata;
use crate::helpers::Helpers;
use crate::ingest::ingest::set_date;

#[derive(Default)]
pub struct IngestRecord {
    pub(crate) source_namespace: String,
    pub(crate) source_partition: String,
    pub(crate) skpr_event_ts: i64,
    pub(crate) skpr_namespace: String,
    pub(crate) skpr_partition: String,
    pub(crate) record: Value,
}


pub fn fast_path_ingest(
    unwrapped_message: &Value,
    metadata: &HashMap<String, Metadata>,
    flatten: bool,
) -> Result<Value, Box<dyn Error>> {
    let mut message: Value = Value::Null;
    for (field, value) in unwrapped_message.as_object().ok_or("Invalid JSON object")? {
        let meta_data = metadata.get(field).ok_or(format!("Field '{}' not found in metadata", field))?;
        let field_data_type = meta_data.determined_type.clone();
        let resolved_value = fast_set_value(
            &field_data_type,
            field,
            value,
            None,
            None,
            metadata,
        )?;
        if !resolved_value.is_null() {
            message[meta_data.out_field_name.clone()] = resolved_value;
        }
    }
    if flatten {
        message = Helpers::flatten(&message, &metadata);
    }
    Ok(message)
}

fn fast_set_value(
    data_type: &str,
    field: &str,
    value: &Value,
    parent_field: Option<&str>,
    parent_data_type: Option<&str>,
    metadata: &HashMap<String, Metadata>,
) -> Result<Value, Box<dyn Error>> {
    if value.is_string() && value.as_str().unwrap_or_default().is_empty() {
        return Ok(Value::Null);
    }
    if data_type.is_empty() {
        return Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, "No data type specified")));
    }

    match data_type {
        "record" => process_record_field(field, value, metadata),
        "map" => process_map_field(&field.to_string(), value, metadata),
        "array" => Ok(value.clone()),
        "date" => Ok(set_date(field, value, metadata)),
        _ => Ok(match_scalar_value(data_type, value)),
    }
}

fn process_record_field(
    field: &str,
    value: &Value,
    metadata: &HashMap<String, Metadata>,
) -> Result<Value, Box<dyn Error>> {
    let mut m = Map::new();
    if value.is_object() {
        for (sub_field, sub_value) in value.as_object().ok_or("Value is not an object")? {
            let meta_field = metadata.get(field).ok_or(format!("Field '{}' not found in metadata", field))?.fields.get(sub_field).ok_or(format!("Subfield '{}' not found in fields", sub_field))?;
            if meta_field.enabled {
                let newval = fast_set_value(
                    &meta_field.determined_type,
                    sub_field,
                    sub_value,
                    Some(field),
                    Some("record"),
                    &metadata.get(field).unwrap().fields
                )?;
                m.insert(meta_field.out_field_name.clone(), newval);
            }
        }
    }
    Ok(Value::Object(m))
}

fn process_map_field(
    field: &str,
    value: &Value,
    metadata: &HashMap<String, Metadata>,
) -> Result<Value, Box<dyn Error>> {
    let mut new_value: Value = Value::Null;
    if value.is_object() {
        for (key, val) in value.as_object().ok_or("Value is not an object")? {
            let meta_field = metadata.get(field).and_then(|f| f.fields.get(key)).ok_or(format!("Field '{}' not found in metadata or it's disabled", key))?;
            if meta_field.enabled {
                let new_val = fast_set_value(
                    &meta_field.determined_type,
                    key,
                    val,
                    Some(field),
                    Some("map"),
                    &metadata.get(field).unwrap().fields,
                )?;
                new_value[meta_field.out_field_name.clone()] = new_val;
            }
        }
    }
    Ok(new_value)
}


fn match_scalar_value(data_type: &str, value: &Value) -> Value {
    match data_type {
        "string" => value.as_str().map(|s| Value::String(s.to_string())).unwrap_or(Value::Null),
        "timestamp" | "timestamp_milli" | "int" | "integer" | "long" => value.as_i64().map(Value::from).unwrap_or(Value::Null),
        "double" => value.as_f64().map(Value::from).unwrap_or(Value::Null),
        "boolean" => value.as_bool().map(Value::from).unwrap_or(Value::Null),
        _ => Value::Null,
    }
}
