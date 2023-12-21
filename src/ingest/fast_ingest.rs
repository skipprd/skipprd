use std::collections::HashMap;

use serde_json::Value;
use std::error::Error;
use std::sync::Arc;

use chrono::{DateTime, NaiveDateTime};
use once_cell::sync::Lazy;
use serde_json::Map;

use crate::discover::{AnalyseSchema, Metadata};
use crate::discover::date_formats::DateFormats;
use crate::discover::evolution::Evolution;

use crate::helpers::Helpers;
use crate::helpers::timed_rwlock::TimedRwLock;
use crate::ingest::ingest::{ResolvedFieldValue};

#[derive(Default)]
pub struct IngestRecord {
    pub(crate) source_namespace: String,
    pub(crate) source_partition: String,
    pub(crate) skpr_event_ts: i64,
    pub(crate) skpr_namespace: String,
    pub(crate) skpr_partition: String,
    pub(crate) record: Value,
}

pub static DEFAULT_NESTED_MESSAGE: Lazy<Arc<TimedRwLock<HashMap<String, Value>>>> = Lazy::new(|| {
    Arc::new(TimedRwLock::new("default_message".to_string(), HashMap::new()))
});

pub fn create_default_nested_message(metadata: &HashMap<String, Metadata>) -> Value {
    let mut message = Value::Object(Map::new());
    for (field, meta_data) in metadata {
        if meta_data.enabled {
            if meta_data.fields.is_empty() {
                message[meta_data.out_field_name.clone()] = Value::Null;
            } else if meta_data.determined_type == "array" {
                if meta_data.determined_type_values == "record" {
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
            } else if meta_data.determined_type == "map" {
                message[meta_data.out_field_name.clone()] = Value::Object(Map::new());
            } else {
                let mut sub_fields = Map::new();
                sub_fields.insert(field.to_string() , create_default_nested_message(&meta_data.fields));
                message[meta_data.out_field_name.clone()] = Value::Object(sub_fields);
            }
        }
    }
    message
}

pub fn fast_path_ingest(
    unwrapped_message: &Value,
    metadata: &HashMap<String, Metadata>,
    namespace: &str,
    flatten: bool,
) -> Result<Value, Box<dyn Error>> {
    // let mut message: Value = Value::Null;
    // @todo - create a default message containing every field in metadata, including nested fields
    // let mut message = create_default_nested_message(metadata);

    let mut message = match DEFAULT_NESTED_MESSAGE.read().get(namespace) {
        Some(m) => m.clone(),
        None => {
           Value::Null
        }
    };

    // panic!("message is: {:?}", message);

    for (field, value) in unwrapped_message.as_object().ok_or("Invalid JSON object")? {
        let meta_data = metadata.get(field).ok_or(format!("Field '{}' not found in metadata", field))?;
        let field_data_type = meta_data.determined_type.clone();
        let resolved_value = match fast_set_value(
                &field_data_type,
                field,
                value,
                metadata,
                None
            ) {
                Ok(v) => v,
                Err(e) => {
                    return Err(e);
                }
            };

        if !resolved_value.value.is_null() {
            // if meta_data.out_field_name == "item_0" {
            //     message[0] = resolved_value;
            // } else {
                message[resolved_value.field] = resolved_value.value;
            // }
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

pub fn fast_set_value(
    data_type: &str,
    field: &str,
    value: &Value,
    metadata: &HashMap<String, Metadata>,
    apply_evolution: Option<bool>,
) -> Result<ResolvedFieldValue, Box<dyn Error>> {
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
    if data_type.is_empty() {
        return Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, "No data type specified")));
    }

    let apply_evolution_bool = apply_evolution.unwrap_or(true);

    match data_type {
        "record" => process_record_field(field, value, metadata),
        "map" => process_map_field(&field.to_string(), value, metadata),
        "array" => process_array_field(&field.to_string(), value, metadata),
        "date" => fast_set_date(field, value, metadata),
        _ => match_scalar_value_fast(field, data_type, value, metadata, apply_evolution_bool),
    }
}

fn process_record_field(
    field: &str,
    value: &Value,
    metadata: &HashMap<String, Metadata>,
) -> Result<ResolvedFieldValue, Box<dyn Error>> {
    let mut m = Map::new();
    if value.is_object() {
        for (sub_field, sub_value) in value.as_object().ok_or("Value is not an object")? {
            let meta_field = metadata.get(field).ok_or(format!("Field '{}' not found in metadata", sub_field))?.fields.get(sub_field).ok_or(format!("Subfield '{}' not found in fields", sub_field))?;
            if meta_field.enabled {
                let newval = fast_set_value(
                    &meta_field.determined_type,
                    sub_field,
                    sub_value,
                    &metadata.get(field).unwrap().fields,
                    None
                )?;
                m.insert(newval.field, newval.value);
            }
        }
    }
    Ok(ResolvedFieldValue::new(field.to_string(),Value::Object(m)))
}

fn process_map_field(
    field: &str,
    value: &Value,
    metadata: &HashMap<String, Metadata>,
) -> Result<ResolvedFieldValue, Box<dyn Error>> {
    let mut new_value: Value = Value::Null;
    if value.is_object() {
        for (key, val) in value.as_object().ok_or("Value is not an object")? {
            let meta_field = metadata.get(field).and_then(|f| f.fields.get(key)).ok_or(format!("Map field '{}' not found in metadata or it's disabled", key))?;
            if meta_field.enabled {
                let new_val = fast_set_value(
                    &meta_field.determined_type,
                    key,
                    val,
                    &metadata.get(field).unwrap().fields,
                    None
                )?;
                new_value[new_val.field] = new_val.value;
            }
        }
    }
    Ok(ResolvedFieldValue::new(field.to_string(), new_value))
}

fn process_array_field(
    field: &str,
    value: &Value,
    metadata: &HashMap<String, Metadata>,
) -> Result<ResolvedFieldValue, Box<dyn Error>> {
    let mut array: Vec<Value> = Vec::new();
    if value.is_array() {
        let values = value.as_array().ok_or("Value is not an array")?;
        for (idx, val) in values.iter().enumerate() {

            let mut sub_field = idx.to_string();
            if metadata.get(field).unwrap().determined_type_values == "record" {
                sub_field = 0.to_string().clone();
            }

            let meta_field = metadata.get(field).ok_or(format!("Array field '{}' not found in metadata or it's disabled", idx))?;
            if meta_field.enabled {
                let _new_val = match fast_set_value(
                    &meta_field.determined_type_values,
                    &sub_field,
                    val,
                    &metadata.get(field).unwrap().fields,
                    None
                ) {
                    Ok(v) => array.push(v.value),
                    Err(_e) => {
                        // @todo - bubble up error and add array evolution support.
                        // Very slow ingest otherwise so just setting null and dropping values
                        array.push(Value::Null);
                    }
                };

                // println!("new_val: {:?}", new_val);


            }
        }
    }
    Ok(ResolvedFieldValue::new(field.to_string(), Value::Array(array)))
}


pub fn match_scalar_value_fast(
    field: &str,
    data_type: &str,
    value: &Value,
    metadata: &HashMap<String, Metadata>,
    apply_evolution: bool,
) -> Result<ResolvedFieldValue, Box<dyn Error>> {

    if value.is_null() {
        return Ok(ResolvedFieldValue {
            field: Metadata::get_field_out_field_name(metadata, field),
            value: Value::Null,
        });
    }

    match data_type {
        "string" => match value.as_str().map(Value::from) {
            Some(v) => Ok(ResolvedFieldValue {
                field: Metadata::get_field_out_field_name(metadata, field),
                value: v,
            }),
            None => match value.as_i64().map(|v| v.to_string()).map(Value::from) {
                Some(v) => Ok(ResolvedFieldValue {
                    field: Metadata::get_field_out_field_name(metadata, field),
                    value: v,
                }),
                None => match value.as_f64().map(|v| v.to_string()).map(Value::from) {
                    Some(v) => Ok(ResolvedFieldValue {
                        field: Metadata::get_field_out_field_name(metadata, field),
                        value: v,
                    }),
                    None => match value.as_bool().map(|v| v.to_string()).map(Value::from) {
                        Some(v) => Ok(ResolvedFieldValue {
                            field: Metadata::get_field_out_field_name(metadata, field),
                            value: v,
                        }),
                        None => {
                            if apply_evolution {
                                match Evolution::apply_evolution_factory(field, value, metadata) {
                                    Ok(v) => Ok(v),
                                    Err(_e) => Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Value {} is not a {}", value, data_type)))),
                                }
                            } else {
                                Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Value {} is not a {}", value, data_type))))
                            }
                        }
                    }
                }
            }
        }
        "long" => match value.as_i64().map(Value::from) {
            Some(v) => Ok(ResolvedFieldValue {
                field: Metadata::get_field_out_field_name(metadata, field),
                value: v,
            }),
            None => match value.as_str().and_then(|v| v.parse::<i64>().ok()).map(Value::from) {
                Some(v) => Ok(ResolvedFieldValue {
                    field: Metadata::get_field_out_field_name(metadata, field),
                    value: v,
                }),
                None => {
                    if apply_evolution {
                        match Evolution::apply_evolution_factory(field, value, metadata) {
                            Ok(v) => Ok(v),
                            Err(_e) => Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Value {} is not a {}", value, data_type)))),
                        }
                    } else {
                        Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Value {} is not a {}", value, data_type))))
                    }
                }
            }
        }
        // ensure 32bit int
        "int" | "integer" => match value.as_i64().map(|v| v as i32).map(Value::from) {
            Some(v) => Ok(ResolvedFieldValue {
                field: Metadata::get_field_out_field_name(metadata, field),
                value: v,
            }),
            None => match value.as_str().and_then(|v| v.parse::<i32>().ok()).map(Value::from) {
                Some(v) => Ok(ResolvedFieldValue {
                    field: Metadata::get_field_out_field_name(metadata, field),
                    value: v,
                }),
                None => {
                    match value.as_bool().and_then(|v| {
                        if v {
                            Some(1)
                        } else {
                            Some(0)
                        }
                    }).map(|v| v as i32).map(Value::from) {
                        Some(v) => Ok(ResolvedFieldValue {
                            field: Metadata::get_field_out_field_name(metadata, field),
                            value: v,
                        }),
                        None => {
                            // handle string bool as int "true" => 1 and "false" => 0
                            match value.as_str().and_then(|v| {
                                if v == "false" || v == "true" {
                                    Some((v == "true").then(|| 1).unwrap_or(0))
                                } else {
                                    None
                                }
                            }) {
                                Some(v) => Ok(ResolvedFieldValue {
                                    field: Metadata::get_field_out_field_name(metadata, field),
                                    value: Value::from(v),
                                }),
                                None => {
                                    if apply_evolution {
                                        match Evolution::apply_evolution_factory(field, value, metadata) {
                                            Ok(v) => Ok(v),
                                            Err(_e) => Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Value {} is not a {}", value, data_type)))),
                                        }
                                    } else {
                                        Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Value {} is not a {}", value, data_type))))
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        "timestamp_milli" | "timestamp" => match value.as_i64().map(Value::from) {
            Some(v) => Ok(ResolvedFieldValue {
                    field: Metadata::get_field_out_field_name(metadata, field),
                    value: AnalyseSchema::coerce_to_milli_seconds(v),
                }),
            None => match value.as_str().and_then(|v| v.parse::<i64>().ok()).map(Value::from) {
                Some(v) =>
                    Ok(ResolvedFieldValue {
                        field: Metadata::get_field_out_field_name(metadata, field),
                        value: AnalyseSchema::coerce_to_milli_seconds(v),
                    }),
                None => {
                    // handle boolean values
                    match value.as_bool().and_then(|v| {
                        if v {
                            Some(1)
                        } else {
                            Some(0)
                        }
                    }).map(Value::from) {
                        Some(v) => Ok(ResolvedFieldValue {
                            field: Metadata::get_field_out_field_name(metadata, field),
                            value: v,
                        }),
                        None => {
                            if apply_evolution {
                                // println!("### Applying Evolution: {}", field);
                                match Evolution::apply_evolution_factory(field, value, metadata) {
                                    Ok(v) => Ok(v),
                                    Err(_e) => {
                                        // println!("### Fast evolutino Error: {}", e);
                                        Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Field {} value {} is not an {}", field, value, data_type))))
                                    },
                                }
                            } else {
                                Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Field {} value {} is not an {}", field, value, data_type))))
                            }
                        }
                    }
                }
            }
        }
        "double" => match value.as_f64().map(Value::from) {
            Some(v) => Ok(ResolvedFieldValue {
                field: Metadata::get_field_out_field_name(metadata, field),
                value: v,
            }),
            None => match value.as_str().and_then(|v| v.parse::<f64>().ok()).map(Value::from) {
                Some(v) => Ok(ResolvedFieldValue {
                    field: Metadata::get_field_out_field_name(metadata, field),
                    value: v,
                }),
                None => {
                    if apply_evolution {
                        match Evolution::apply_evolution_factory(field, value, metadata) {
                            Ok(v) => Ok(v),
                            Err(_e) => Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Value {} is not a {}", value, data_type)))),
                        }
                    } else {
                        Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Value {} is not a {}", value, data_type))))
                    }
                }
            }
            // Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData,  format!("Value {} is not a double", value)))),
        }
        "boolean" => match value.as_bool().map(Value::from) {
            Some(v) => Ok(ResolvedFieldValue {
                field: Metadata::get_field_out_field_name(metadata, field),
                value: v,
            }),
            None => match value.as_str().and_then(|v| v.parse::<bool>().ok()).map(Value::from) {
                Some(v) => Ok(ResolvedFieldValue {
                    field: Metadata::get_field_out_field_name(metadata, field),
                    value: v,
                }),
                None => {
                    match value.as_i64().and_then(|v| {
                        if v == 0 || v == 1 {
                            Some((v == 1).to_string().parse::<bool>().ok()).map(Value::from)
                        } else {
                            None
                        }
                    }) {
                        Some(v) => Ok(ResolvedFieldValue {
                            field: Metadata::get_field_out_field_name(metadata, field),
                            value: v,
                        }),
                        None => {
                            // handle bool as string
                            match value.as_str().and_then(|v| {
                                if v == "0" || v == "1" {
                                    Some((v == "1").to_string().parse::<bool>().ok()).map(Value::from)
                                } else {
                                    None
                                }
                            }) {
                                Some(v) => Ok(ResolvedFieldValue {
                                    field: Metadata::get_field_out_field_name(metadata, field),
                                    value: v,
                                }),
                                None => {
                                    if apply_evolution {
                                        match Evolution::apply_evolution_factory(field, value, metadata) {
                                            Ok(v) => Ok(v),
                                            Err(_e) => Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Value {} is not a {}", value, data_type)))),
                                        }
                                    } else {
                                        Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Value {} is not a {}", value, data_type))))
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        _ => Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Unknown data type '{}'", data_type)))),
    }

}

pub fn fast_set_date(field: &str, value: &Value, metadata: &HashMap<String, Metadata>) -> Result<ResolvedFieldValue, Box<dyn Error>> {
    // Hive Timestamp doesn't support string dates
    match value.as_str() {
        Some(val) => {
            let parent_field_meta = match metadata
                .get(field) {
                    Some(m) => m,
                    None => return Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Could not find field metadata for {}", field)))),
                };

            let date_meta = match parent_field_meta.date_candidate
                .as_ref() {
                    Some(f) => f,
                    None => return Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Could not find date candidate in metadata for {}", field)))),
                };

            let fmt = &date_meta.format;

            match DateFormats::from_str(fmt) {
                Ok(f) => match Helpers::parse_date_from_string(val, f.as_str()) {
                    Ok(date) => {
                        let millis = date.timestamp() * 1000;
                        Ok(ResolvedFieldValue {
                            field: Metadata::get_field_out_field_name(metadata, field),
                            value: millis.into(),
                        })
                    }
                    Err(_) => {
                        Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Could not parse date {} with format {} for field {}", val, fmt, field))))
                    }
                },
                Err(err) => {
                    println!("Error date: {}", err);
                    Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, "Invalid date format")))
                }
            }
        },
        None => {
            Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Could not format date, expected value {} to parse as a string", value))))
        }
    }
}

#[cfg(test)]
mod tests_fast_set_date {
    use super::*;
    use crate::discover::DateCandidate;
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
                parent_type: String::from("parent"),
                fields: Box::new(HashMap::new()),
                date_candidate: Some(date_candidate),
                evolution: Box::new(HashMap::new()),
                enabled: true,
                out_field_name: String::from(field),
                determined_type: String::from("date"),
                determined_type_values: "".to_string(),
            },
        );

        meta
    }

    #[test]
    fn test_fast_set_date_with_valid_date() {
        let foo: AnalyseSchema = AnalyseSchema { i: 0 };

        let field = "test_field";

        let date_str = "2023-05-21 12:34:56";
        let format_name = foo.is_valid_date(date_str).unwrap();
        let format = DateFormats::from_str(format_name).unwrap().as_str();

        let mut meta = generate_metadata(field, format_name);

        let value = Value::String(String::from(date_str));

        let mut updated_schema = "no".to_string();

        let result = fast_set_date(field, &value, &mut meta);

        let expected_date = Helpers::parse_date_from_string(date_str, format).unwrap();
        let expected_millis =
            Value::Number(serde_json::Number::from(expected_date.timestamp() * 1000));

        assert!(result.is_ok());
        assert_eq!(result.unwrap().value, expected_millis);
    }

    #[test]
    fn test_fast_set_date_with_valid_iso_date() {
        let foo: AnalyseSchema = AnalyseSchema { i: 0 };

        let field = "test_field";

        // let date_str = "2023-05-23T07:09:03.000Z";
        // let date_str = "2023-07-11T12:56:44.000Z";
        // let date_str = "2023-07-11T14:56:44+02:00";
        let date_str = "2023-07-11T14:56:44";
        let format_name = foo.is_valid_date(date_str).unwrap();
        let format = DateFormats::from_str(format_name).unwrap().as_str();

        let mut meta = generate_metadata(field, format_name);

        let value = Value::String(String::from(date_str));

        let mut updated_schema = "no".to_string();

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
        let foo: AnalyseSchema = AnalyseSchema { i: 0 };

        let field = "test_field";

        let date_str = "2023-12-11T15:49:31+01:00";
        let format_name = foo.is_valid_date(date_str).unwrap();
        let format = DateFormats::from_str(format_name).unwrap().as_str();

        let mut meta = generate_metadata(field, format_name);

        let value = Value::String(String::from(date_str));

        let mut updated_schema = "no".to_string();

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

        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "string", &str_to_val("hello"), &metadata, true)).value,
            str_to_val("hello")
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "string", &i64_to_val(123), &metadata, true)).value,
            str_to_val("123")
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "string", &f64_to_val(123.4), &metadata, true)).value,
            str_to_val("123.4")
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "string", &bool_to_val(true), &metadata, true)).value,
            str_to_val("true")
        );
    }

    #[test]
    fn test_match_scalar_value_fast_int() {

        let mut metadata = HashMap::new();
        metadata.insert("field".to_string(), Metadata::new().unwrap());

        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "int", &i64_to_val(123), &metadata, true)).value,
            i64_to_val(123)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "int", &str_to_val("123"), &metadata, true)).value,
            i64_to_val(123)
        );

        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "int", &bool_to_val(true), &metadata, true)).value,
            i64_to_val(1)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "int", &bool_to_val(false), &metadata, true)).value,
            i64_to_val(0)
        );

        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "int", &str_to_val("true"), &metadata, true)).value,
            i64_to_val(1)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "int", &str_to_val("false"), &metadata, true)).value,
            i64_to_val(0)
        );
        assert_eq!(
            match_scalar_value_fast("field", "int", &str_to_val("True"), &metadata, true).is_err(),
            true
        );
        assert_eq!(
            match_scalar_value_fast("field", "int", &str_to_val("False"), &metadata, true).is_err(),
            true
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "int", &i64_to_val(1), &metadata, true)).value,
            i64_to_val(1)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "int", &str_to_val("1"), &metadata, true)).value,
            i64_to_val(1)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "int", &i64_to_val(0), &metadata, true)).value,
            i64_to_val(0)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "int", &str_to_val("0"), &metadata, true)).value,
            i64_to_val(0)
        );
    }

    #[test]
    fn test_match_scalar_value_fast_double() {

        let mut metadata = HashMap::new();
        metadata.insert("field".to_string(), Metadata::new().unwrap());

        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "double", &f64_to_val(123.4), &metadata, true)).value,
            f64_to_val(123.4)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "double", &str_to_val("123.4"), &metadata, true)).value,
            f64_to_val(123.4)
        );
    }

    #[test]
    fn test_match_scalar_value_fast_boolean() {

        let mut metadata = HashMap::new();
        metadata.insert("field".to_string(), Metadata::new().unwrap());

        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "boolean", &bool_to_val(true), &metadata, true)).value,
            bool_to_val(true)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "boolean", &bool_to_val(false), &metadata, true)).value,
            bool_to_val(false)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "boolean", &str_to_val("true"), &metadata, true)).value,
            bool_to_val(true)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "boolean", &str_to_val("false"), &metadata, true)).value,
            bool_to_val(false)
        );
        assert_eq!(
            match_scalar_value_fast("field", "boolean", &str_to_val("True"), &metadata, true).is_err(),
            true
        );
        assert_eq!(
            match_scalar_value_fast("field", "boolean", &str_to_val("False"), &metadata, true).is_err(),
            true
        );
        assert_eq!(
            match_scalar_value_fast("field", "boolean", &str_to_val("Yes"), &metadata, true).is_err(),
            true
        );
        assert_eq!(
            match_scalar_value_fast("field", "boolean", &str_to_val("No"), &metadata, true).is_err(),
            true
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "boolean", &i64_to_val(1), &metadata, true)).value,
            bool_to_val(true)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "boolean", &i64_to_val(0), &metadata, true)).value,
            bool_to_val(false)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "boolean", &str_to_val("1"), &metadata, true)).value,
            bool_to_val(true)
        );
        assert_eq!(
            get_or_panic(match_scalar_value_fast("field", "boolean", &str_to_val("0"), &metadata, true)).value,
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

        get_or_panic(match_scalar_value_fast("field", "unknown", &str_to_val("hello"), &metadata, true));
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
        let mut metadata = HashMap::new();
        let mut meta_data_item = Metadata::new()?;
        meta_data_item.determined_type_values = "int".to_string();
        metadata.insert(field.to_string(), meta_data_item);

        let result = process_array_field(field, &value, &metadata)?;

        assert_eq!(result.value, json!([1, 2, 3]));
        Ok(())
    }

    #[test]
    fn test_process_array_field_floats() -> Result<(), Box<dyn Error>> {
        let field = "test_field";
        let value = json!([1.2, 2.3, 3.4]);
        let mut metadata = HashMap::new();
        let mut meta_data_item = Metadata::new()?;
        meta_data_item.determined_type_values = "double".to_string();
        metadata.insert(field.to_string(), meta_data_item);

        let result = process_array_field(field, &value, &metadata)?;

        assert_eq!(result.value, json!([1.2, 2.3, 3.4]));
        Ok(())
    }

    #[test]
    fn test_process_array_field_booleans() -> Result<(), Box<dyn Error>> {
        let field = "test_field";
        let value = json!([true, false, true]);
        let mut metadata = HashMap::new();
        let mut meta_data_item = Metadata::new()?;
        meta_data_item.determined_type_values = "boolean".to_string();
        metadata.insert(field.to_string(), meta_data_item);

        let result = process_array_field(field, &value, &metadata)?;

        assert_eq!(result.value, json!([true, false, true]));
        Ok(())
    }

    #[test]
    fn test_process_array_field_booleans_int() -> Result<(), Box<dyn Error>> {
        let field = "test_field";
        let value = json!([1, 0, 1]);
        let mut metadata = HashMap::new();
        let mut meta_data_item = Metadata::new()?;
        meta_data_item.determined_type_values = "boolean".to_string();
        metadata.insert(field.to_string(), meta_data_item);

        let result = process_array_field(field, &value, &metadata)?;

        assert_eq!(result.value, json!([true, false, true]));
        Ok(())
    }

    #[test]
    fn test_process_array_field_null() -> Result<(), Box<dyn Error>> {
        let field = "test_field";
        let value = json!([null, null, null]);
        let mut metadata = HashMap::new();
        let mut meta_data_item = Metadata::new()?;
        meta_data_item.determined_type_values = "null".to_string();
        metadata.insert(field.to_string(), meta_data_item);

        let result = process_array_field(field, &value, &metadata)?;

        assert_eq!(result.value, json!([null, null, null]));
        Ok(())
    }

    #[test]
    fn test_process_array_field_strings() -> Result<(), Box<dyn Error>> {
        let field = "test_field";
        let value = json!(["one", "two", "three"]);
        let mut metadata = HashMap::new();
        let mut meta_data_item = Metadata::new()?;
        meta_data_item.determined_type_values = "string".to_string();
        metadata.insert(field.to_string(), meta_data_item);

        let result = process_array_field(field, &value, &metadata)?;

        assert_eq!(result.value, json!(["one", "two", "three"]));
        Ok(())
    }

    #[test]
    fn test_process_array_field_field_not_found() {
        let field = "test_field";
        let value = json!([1, 2, 3]);
        let metadata = HashMap::new();

        let result = process_array_field(field, &value, &metadata);

        assert!(result.is_err());
    }
}
