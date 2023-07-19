use std::collections::HashMap;

use serde_json::Value;
use std::error::Error;
use std::ops::Deref;
use chrono::NaiveDateTime;
use serde_json::Map;
use crate::discover::date_formats::DateFormats;

use crate::discover::{Evolution, Metadata};
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
        message = match Helpers::flatten(&message, &metadata) {
            Ok(m) => m,
            Err(e) => {
                return Err(e);
            }
        };
    }
    Ok(message)
}

fn fast_set_value(
    data_type: &str,
    field: &str,
    value: &Value,
    _parent_field: Option<&str>,
    _parent_data_type: Option<&str>,
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
        "array" => process_array_field(&field.to_string(), value, metadata),
        "date" => fast_set_date(field, value, metadata),
        _ => match_scalar_value_fast(field, data_type, value, metadata, true),
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
            let meta_field = metadata.get(field).ok_or(format!("Field '{}' not found in metadata", sub_field))?.fields.get(sub_field).ok_or(format!("Subfield '{}' not found in fields", sub_field))?;
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

fn process_array_field(
    field: &str,
    value: &Value,
    metadata: &HashMap<String, Metadata>,
) -> Result<Value, Box<dyn Error>> {
    let mut array: Vec<Value> = Vec::new();
    if value.is_array() {
        let values = value.as_array().ok_or("Value is not an array")?;
        for (idx, val) in values.iter().enumerate() {
            let meta_field = metadata.get(field).ok_or(format!("Field '{}' not found in metadata or it's disabled", idx))?;
            if meta_field.enabled {
                let new_val = fast_set_value(
                    &meta_field.determined_type_values,
                    &idx.to_string(),
                    val,
                    Some(field),
                    Some("array"),
                    &metadata.get(field).unwrap().fields,
                )?;
                array.push(new_val);
            }
        }
    }
    Ok(Value::Array(array))
}


pub fn match_scalar_value_fast(
    field: &str,
    data_type: &str,
    value: &Value,
    metadata: &HashMap<String, Metadata>,
    apply_evolution: bool,
) -> Result<Value, Box<dyn Error>> {

    if value.is_null() {
        return Ok(Value::Null);
    }

    match data_type {
        "string" => match value.as_str().map(Value::from) {
            Some(v) => Ok(v),
            None => match value.as_i64().map(|v| v.to_string()).map(Value::from) {
                Some(v) => Ok(v),
                None => match value.as_f64().map(|v| v.to_string()).map(Value::from) {
                    Some(v) => Ok(v),
                    None => match value.as_bool().map(|v| v.to_string()).map(Value::from) {
                        Some(v) => Ok(v),
                        None => {
                            if apply_evolution {
                                match Evolution::apply_evolution_factory(field, value, metadata) {
                                    Ok(v) => Ok(v),
                                    Err(e) => Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Value {} is not a string", value)))),
                                }
                            } else {
                                Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Value {} is not a string", value))))
                            }
                        }
                    }
                }
            }
        }
        "timestamp" | "timestamp_milli" | "int" | "integer" | "long" => match value.as_i64().map(Value::from) {
            Some(v) => Ok(v),
            None => match value.as_str().and_then(|v| v.parse::<i64>().ok()).map(Value::from) {
                Some(v) => Ok(v),
                None => {
                    if apply_evolution {
                        match Evolution::apply_evolution_factory(field, value, metadata) {
                            Ok(v) => Ok(v),
                            Err(e) => Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Value {} is not an integer", value)))),
                        }
                    } else {
                        Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Value {} is not a string", value))))
                    }
                }
            }
        }
        "double" => match value.as_f64().map(Value::from) {
            Some(v) => Ok(v),
            None => match value.as_str().and_then(|v| v.parse::<f64>().ok()).map(Value::from) {
                Some(v) => Ok(v),
                None => {
                    if apply_evolution {
                        match Evolution::apply_evolution_factory(field, value, metadata) {
                            Ok(v) => Ok(v),
                            Err(e) => Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Value {} is not a double", value)))),
                        }
                    } else {
                        Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Value {} is not a string", value))))
                    }
                }
            }
            // Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData,  format!("Value {} is not a double", value)))),
        }
        "boolean" => match value.as_bool().map(Value::from) {
            Some(v) => Ok(v),
            None => match value.as_str().and_then(|v| v.parse::<bool>().ok()).map(Value::from) {
                Some(v) => Ok(v),
                None => match value.as_i64().and_then(|v| {
                    if v == 0 || v == 1 {
                        Some((v == 1).to_string().parse::<bool>().ok()).map(Value::from)
                    } else {
                        None
                    }
                }) {
                    Some(v) => Ok(v),
                    None => {
                        if apply_evolution {
                            match Evolution::apply_evolution_factory(field, value, metadata) {
                                Ok(v) => Ok(v),
                                Err(e) => Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Value {} is not a boolean", value)))),
                            }
                        } else {
                            Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Value {} is not a string", value))))
                        }
                    }
                }
            }
        }
        _ => Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Unknown data type '{}'", data_type)))),
    }

}

pub fn fast_set_date(field: &str, value: &Value, metadata: &HashMap<String, Metadata>) -> Result<Value, Box<dyn Error>> {
    // Hive Timestamp doesn't support string dates
    match value.as_str() {
        Some(val) => {
            let fmt = &metadata
                .get(field)
                .unwrap()
                .date_candidate
                .as_ref()
                .unwrap()
                .format;
            match DateFormats::from_str(fmt) {
                Ok(f) => match NaiveDateTime::parse_from_str(val, f.as_str()) {
                    Ok(date) => {
                        let millis = date.timestamp() * 1000;
                        Ok(millis.into())
                    }
                    Err(_) => Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, "Invalid date format"))),
                },
                Err(err) => {
                    println!("Error date: {}", err);
                    Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, "Invalid date format")))
                }
            }
        },
        None => {
            // println!("Could not format date to int using format");
            Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, "Could not format date to int using format")))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::metadata;
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

    fn get_or_panic(result: Result<Value, Box<dyn Error>>) -> Value {
        result.expect("Unexpected error")
    }

    #[test]
    fn test_match_scalar_value_string() {

        let metadata = HashMap::new();
        metadata.insert("field".to_string(), Metadata::new().unwrap());

        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "string", &str_to_val("hello"), &metadata, true)),
            str_to_val("hello")
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "string", &i64_to_val(123), &metadata, true)),
            str_to_val("123")
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "string", &f64_to_val(123.4), &metadata, true)),
            str_to_val("123.4")
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "string", &bool_to_val(true), &metadata, true)),
            str_to_val("true")
        );
    }

    #[test]
    fn test_match_scalar_value_fast_int() {

        let metadata = HashMap::new();
        metadata.insert("field".to_string(), Metadata::new().unwrap());

        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "int", &i64_to_val(123), &metadata, true)),
            i64_to_val(123)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "int", &str_to_val("123"), &metadata, true)),
            i64_to_val(123)
        );
    }

    #[test]
    fn test_match_scalar_value_fast_double() {

        let metadata = HashMap::new();
        metadata.insert("field".to_string(), Metadata::new().unwrap());

        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "double", &f64_to_val(123.4), &metadata, true)),
            f64_to_val(123.4)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "double", &str_to_val("123.4"), &metadata, true)),
            f64_to_val(123.4)
        );
    }

    #[test]
    fn test_match_scalar_value_fast_boolean() {

        let metadata = HashMap::new();
        metadata.insert("field".to_string(), Metadata::new().unwrap());

        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "boolean", &bool_to_val(true), &metadata, true)),
            bool_to_val(true)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "boolean", &str_to_val("true"), &metadata, true)),
            bool_to_val(true)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "boolean", &i64_to_val(1), &metadata, true)),
            bool_to_val(true)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "boolean", &i64_to_val(0), &metadata, true)),
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

        let metadata = HashMap::new();
        metadata.insert("field".to_string(), Metadata::new().unwrap());

        get_or_panic(match_scalar_value_fast("field", "unknown", &str_to_val("hello"), &metadata, true));
    }
}
