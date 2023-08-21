use crate::discover::date_formats::DateFormats;
use crate::discover::{AnalyseSchema, Metadata};
use crate::helpers::configuration::Config;
use crate::helpers::Helpers;
use arrow::json::reader::ValueIter;
use chrono::NaiveDateTime;
use serde_json::{Map, Value};
use std::borrow::BorrowMut;
use std::collections::HashMap;
use std::io::{BufReader, Read};
use crate::discover::evolution::Evolution;


#[derive(Default)]
pub struct IngestRecord {
    pub(crate) source_namespace: String,
    pub(crate) source_partition: String,
    pub(crate) skpr_event_ts: i64,
    pub(crate) skpr_namespace: String,
    pub(crate) skpr_partition: String,
    pub(crate) record: Value,
}

pub fn ingest_buf<R: Read>(reader: &mut BufReader<R>) -> ValueIter<R> {
    ValueIter::new(reader, None)
}

// pub fn fast_path_ingest(unwrapped_message: &mut IngestRecord, metadata: &mut HashMap<String, Metadata>) -> HashMap<String, Message<Value>> {
// pub fn fast_path_ingest(unwrapped_message: &mut IngestRecord, metadata: &mut HashMap<String, Metadata>) {
pub fn ingest(
    unwrapped_message: &Value,
    metadata: &mut HashMap<String, Metadata>,
    updated_schema: &mut String,
    flatten: bool,
) -> Value {
    // let mut helpers = Helpers { clean_field_cache: Default::default() };

    // let mut message = default_msgs[namespace].clone();
    // let mut message = SerderParquet::default_message(metadata);

    // let mut message: Vec<Value> = Vec::with_capacity(batch_size);
    let mut message: Value = Value::Null;

    for (field, value) in unwrapped_message.as_object().unwrap() {
        // let field = Helpers::clean_field_name(field.to_string());

        // println!("Ingesting field: {:?}", field);

        // if special_fields.contains_key(field) {
        //     message.insert(field.to_string(), value.to_string());
        // } else {
        //         let resolved_value = Value::Null;

        let field_data_type = match metadata.get_mut(&field.to_string()) {
            Some(data_type) => {
                // println!("{:?}",  data_type.determined_type.clone());
                data_type.determined_type.clone()
            }
            None => "".to_string(),
        };

        let resolved_value = set_value(
            &field_data_type,
            &field.to_string(),
            value,
            None,
            None,
            metadata,
            updated_schema,
        );

        // println!("Setting message with field: {:?} and value {:?}", field, resolved_value);

        // let foo = resolved_value;
        // ignore if null, use default message which has correct null for data type
        if !resolved_value.is_null() {
            message[metadata
                .get(&field.to_string())
                .unwrap()
                .clone()
                .out_field_name] = resolved_value;

            // println!("{:?}", message);
            // match resolved_value {
            //     Value::Object(_) => message.as_array_mut().unwrap().push(resolved_value),
            //     _ => println!("Row needs to be of type object, got: {:?}", resolved_value)
            //     // _ => {
            //     //     return Err(ArrowError::JsonError(format!(
            //     //         "Row needs to be of type object, got: {:?}",
            //     //         v
            //     //     )));
            // }

            // message.unwrap().message.insert(field, resolved_value);
            // message.get_mut(field).unwrap() = resolved_value;
        }
        // let bar = message;
        // }
    }

    // total_entries += 1;
    // increment_namespaces_count(namespace);
    // current_entries += 1;
    // fast_path += 1;
    //
    // message.unwrap()

    // if message.is_empty() {
    // reached end of file
    // return Ok(None);
    // }

    // let message = message[..];

    // println!("{:?}", message);
    if flatten {
        message = Helpers::flatten(&message, &metadata).unwrap();
    }
    // println!("{:?}", message);
    // exit(0);
    message
}

/**
 * @todo - handle return Result<Value, Error>

 * @param $dataType string - the expected data type of the field value
 * @param $field string - field name
 * @param $value string -  the actual field value
 * @return mixed|null - value data type on success or null on error
 */
pub fn set_value(
    data_type: &str,
    field: &str,
    value: &Value,
    parent_field: Option<&str>,
    parent_data_type: Option<&str>,
    metadata: &mut HashMap<String, Metadata>,
    updated_schema: &mut String,
) -> Value {
    // let _parent_type = match metadata.get_mut(field) {
    //     Some(pt) => &pt.parent_type,
    //     None => ""
    // };

    // if data_type != "" || parent_type == "map" {
    if !data_type.is_empty() {

        if value.is_null() {
            return Value::Null;
        }
        if value.is_string() && value.as_str().unwrap_or_default().is_empty() {
            return Value::Null;
        }

        // let data_type: &str = &metadata.get_mut(field).unwrap().determined_type;
        // let data_type = "record";

        let x: Value;
        let mut new_value: Value = Value::Null;

        if !value.to_string().is_empty() {
            if data_type == "record" {
                // println!("{} is record", field);

                let mut m = Map::new();

                if value.is_object() {
                    for (sub_field, sub_value) in value.as_object().unwrap() {
                        // let clean_sub_field = Helpers::clean_field_name(sub_field.to_string());

                        // println!("({}) ingesting {} => {} with value: {}", data_type, field, &sub_field.to_string(), sub_value);

                        match metadata
                            .get(&field.to_string())
                            .unwrap()
                            .fields
                            .get(&sub_field.to_string())
                        {
                            Some(_t) => (),
                            None => {
                                // println!("({}) no metadata for {} => {} with value: {}", data_type, field, &sub_field.to_string(), sub_value);
                                discover_ingest(
                                    &sub_field.to_string(),
                                    sub_value,
                                    Some(field),
                                    Some(data_type),
                                    &mut metadata.get_mut(&field.to_string()).unwrap().fields,
                                    updated_schema,
                                );
                                // discover_ingest(field, value, metadata, updatedSchema, flatten);
                            }
                        }

                        // only ingest fields enabled to sync to output
                        if metadata.get_mut(&field.to_string()).is_some()
                            && metadata
                                .get_mut(&field.to_string())
                                .unwrap()
                                .fields
                                .get_mut(&sub_field.to_string())
                                .is_some()
                            && metadata
                                .get_mut(&field.to_string())
                                .unwrap()
                                .fields
                                .get_mut(&sub_field.to_string())
                                .expect(&format!(
                                    "No metadata for field: {} with value: {:?}",
                                    &sub_field, sub_value
                                ))
                                .enabled
                        {
                            let newval = set_value(
                                &metadata
                                    .get_mut(&field.to_string())
                                    .unwrap()
                                    .fields
                                    .get_mut(&sub_field.to_string())
                                    .unwrap()
                                    .determined_type
                                    .clone(),
                                &sub_field.to_string(),
                                // &mut sub_value.as_str().unwrap_or(&value.to_string()), // pass string val or string representation of map/array, etc
                                sub_value,
                                Some(field),
                                Some(data_type),
                                &mut metadata.get_mut(&field.to_string()).unwrap().fields,
                                updated_schema,
                            );

                            m.insert(
                                metadata
                                    .get(&field.to_string())
                                    .unwrap()
                                    .fields
                                    .get(&sub_field.to_string())
                                    .unwrap()
                                    .clone()
                                    .out_field_name,
                                newval,
                            );
                        }
                    }
                }

                // terrible duplication.
                // We determine that arrays containing arrays are record types
                // So we'll sometimes end up here
                if value.is_array() {
                    let mut i = 0;
                    for sub_value in value.as_array().unwrap() {
                        // let clean_sub_field = Helpers::clean_field_name(i.to_string());

                        // println!("({}) ingesting {} => {} with value: {}", data_type, &field.to_string(), &i.to_string(), sub_value);

                        match metadata
                            .get(&field.to_string())
                            .unwrap()
                            .fields
                            .get(&i.to_string())
                        {
                            Some(_t) => (),
                            None => {
                                // println!("({}.array) no metadata for {} => {} with value: {}", data_type, &field.to_string(), i.to_string(), sub_value);
                                // discover_ingest(&field.to_string(), value, metadata, updatedSchema, flatten);
                                discover_ingest(
                                    &i.to_string(),
                                    sub_value,
                                    Some(field),
                                    Some(data_type),
                                    &mut metadata.get_mut(&field.to_string()).unwrap().fields,
                                    updated_schema,
                                );
                            }
                        }

                        // only ingest fields enabled to sync to output
                        if metadata.get_mut(&field.to_string()).is_some()
                            && metadata
                                .get_mut(&field.to_string())
                                .unwrap()
                                .fields
                                .get_mut(&i.to_string())
                                .is_some()
                            && metadata
                                .get_mut(&field.to_string())
                                .unwrap()
                                .fields
                                .get_mut(&i.to_string())
                                .unwrap()
                                .enabled
                        {
                            let newval = set_value(
                                &metadata
                                    .get_mut(&field.to_string())
                                    .unwrap()
                                    .fields
                                    .get_mut(&i.to_string())
                                    .unwrap()
                                    .determined_type
                                    .clone(),
                                &i.to_string(),
                                // &mut sub_value.as_str().unwrap_or(&value.to_string()), // pass string val or string representation of map/array, etc
                                sub_value,
                                Some(field),
                                Some(data_type),
                                &mut metadata.get_mut(&field.to_string()).unwrap().fields,
                                updated_schema,
                            );

                            m.insert(
                                metadata
                                    .get(&field.to_string())
                                    .unwrap()
                                    .fields
                                    .get(&i.to_string())
                                    .unwrap()
                                    .clone()
                                    .out_field_name,
                                newval,
                            );
                        }

                        i += 1;
                    }
                }

                x = m.into();
                new_value = x;
            } else if data_type == "map" {
                if value.is_object() {
                    for (key, val) in value
                        .as_object()
                        .unwrap()
                        .iter()
                        .filter_map(|(k, v)| Some((k, v)))
                    {
                        // if field == "trip" {
                        //     println!("{:?}", metadata.get_mut(field));
                        // }

                        if Some(val).is_some() {
                            match metadata.get(field).unwrap().fields.get(key) {
                                Some(_t) => (),
                                None => {
                                    // println!("({}) no metadata for {} => {} with value: {}", data_type, field, key, val);
                                    // discover_ingest(key, val, &mut metadata.get_mut(field).unwrap().fields, updatedSchema, flatten);
                                    discover_ingest(
                                        field,
                                        value,
                                        Some(field),
                                        Some(data_type),
                                        metadata,
                                        updated_schema,
                                    );
                                }
                            }

                            // only ingest fields enabled to sync to output
                            if metadata.get_mut(&field.to_string()).is_some()
                                && metadata
                                    .get_mut(&field.to_string())
                                    .unwrap()
                                    .fields
                                    .get_mut(key)
                                    .is_some()
                                && metadata
                                    .get_mut(field)
                                    .unwrap()
                                    .fields
                                    .get_mut(key)
                                    .unwrap()
                                    .enabled
                            {
                                // println!("ingesting {} => {} with value: {}", field, key, val);

                                new_value[metadata
                                    .get(field)
                                    .unwrap()
                                    .fields
                                    .get(key)
                                    .unwrap()
                                    .clone()
                                    .out_field_name] = set_value(
                                    &metadata
                                        .get_mut(field)
                                        .unwrap()
                                        .fields
                                        .get_mut(&key.to_string())
                                        .unwrap()
                                        .determined_type
                                        .clone(),
                                    &key.to_string(),
                                    val,
                                    Some(field),
                                    Some(data_type),
                                    &mut metadata.get_mut(field).unwrap().fields,
                                    updated_schema,
                                );
                            }
                        }
                    }
                }
                // for (key, val) in value.as_object().unwrap() {
                //     if Some(val) != None {
                //         new_value[key] = fast_set_value(
                //             metadata
                //                 .get_mut(field)
                //                 .unwrap()
                //                 .determined_type_values
                //                 .clone(),
                //             key,
                //             val,
                //             &mut metadata.get_mut(field).unwrap().fields,
                //             updatedSchema
                //         );
                //     }
                // }
            } else if data_type == "array" {
                new_value = value.to_owned();
            } else {
                // println!("value is {}", value);
                // println!("field is {}", field);
                // println!("data_type is {}", data_type);

                if data_type == "date" {
                    let date_new_value = set_date(field, value, metadata, updated_schema);
                    new_value = date_new_value.unwrap_or(Value::Null);
                } else {
                    // let scalar_value = match data_type {
                    //     "string" => value.as_str().map(|s| Value::String(s.to_string())),
                    //     "timestamp" | "timestamp_milli" | "int" | "integer" | "long" => {
                    //         value.as_i64().map(Value::from)
                    //     }
                    //     "double" => value.as_f64().map(Value::from),
                    //     "boolean" => value.as_bool().map(Value::from),
                    //     _ => None,
                    // };

                    // @todo - handle return Result<Value, Error>
                    let scalar_value = match_scalar_value(field, data_type, value, metadata, updated_schema);

                    new_value = scalar_value.unwrap_or(Value::Null);
                }

                // println!("field {} value: {:?}", field, scalar_value);

                // println!("field {} value: {:?}", field, new_value);

                // if data_type == "string" {
                //     new_value = match value.as_str() {
                //         Some(val) => Value::String(val.to_string()),
                //         None => Value::Null,
                //     };
                // } else if data_type == "timestamp" || data_type == "timestamp_milli" {
                //     new_value = match value.as_i64() {
                //         Some(val) => Value::from(val),
                //         None => Value::Null,
                //     }
                // } else if data_type == "int" || data_type == "integer" {
                //     new_value = match value.as_i64() {
                //         Some(val) => Value::from(val),
                //         None => Value::Null,
                //     };
                //
                // } else if data_type == "long" {
                //     new_value = match value.as_i64() {
                //         Some(val) => Value::from(val),
                //         None => Value::Null,
                //     }
                // } else if data_type == "double" {
                //     new_value = match value.as_f64() {
                //         Some(val) => Value::from(val),
                //         None => Value::Null,
                //     }
                // } else if data_type == "boolean" {
                //     new_value = match value.as_bool() {
                //         Some(val) => Value::from(val),
                //         None => Value::Null,
                //     }
                // }
            }
        }
        new_value
    } else {
        // println!("({}) no metadata for {} with value: {}", data_type, &field.to_string(), value);

        let discoverd_data_type = discover_ingest(
            &field.to_string(),
            value,
            parent_field,
            parent_data_type,
            metadata,
            updated_schema,
        );

        if discoverd_data_type != "" {
            return set_value(
                &discoverd_data_type,
                &field.to_string(),
                value,
                parent_field,
                parent_data_type,
                metadata,
                updated_schema,
            );
        } else {
            return Value::Null;
        }
    }
}

fn match_scalar_value(
    field: &str,
    data_type: &str,
    value: &Value,
    metadata: &mut HashMap<String, Metadata>,
    mut updated_schema: &mut String,
) -> Result<Value, Box<dyn std::error::Error>> {
    match data_type {
        "string" => match value.as_str().map(Value::from) {
            Some(v) => Ok(v),
            None => match value.as_i64().map(|v| v.to_string()).map(Value::from) {
                Some(v) => Ok(v),
                None => match value.as_f64().map(|v| v.to_string()).map(Value::from) {
                    Some(v) => Ok(v),
                    None => match value.as_bool().map(|v| v.to_string()).map(Value::from) {
                        Some(v) => Ok(v),
                        // None => Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Value {} is not a string", value)))),
                        None => {
                            // Handle the value error applying the Evolution Strategy
                            Evolution::evolve_field(&field.to_string(), value, metadata, updated_schema)
                        }
                    }
                }
            }
        }
        "timestamp" | "timestamp_milli" | "int" | "integer" | "long" => match value.as_i64().map(Value::from) {
            Some(v) => Ok(v),
            None => match value.as_str().and_then(|v| v.parse::<i64>().ok()).map(Value::from) {
                Some(v) => Ok(v),
                None => match value.as_f64().and_then(|v| v.to_string().parse::<i64>().ok()).map(Value::from) {
                    Some(v) => Ok(v),
                    None => match value.as_bool().and_then(|v| v.to_string().parse::<i64>().ok()).map(Value::from) {
                        Some(v) => Ok(v),
                        // None => Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Value {} is not an integer", value)))),
                        None => {
                            // Handle the value error applying the Evolution Strategy
                            Evolution::evolve_field(&field.to_string(), value, metadata, updated_schema)
                        }
                    }
                }
            }
            // Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Value {} is not an integer", value)))),
        }
        "double" => match value.as_f64().map(Value::from) {
            Some(v) => Ok(v),
            None => match value.as_str().and_then(|v| v.parse::<f64>().ok()).map(Value::from) {
                Some(v) => Ok(v),
                None => match value.as_i64().and_then(|v| v.to_string().parse::<f64>().ok()).map(Value::from) {
                    Some(v) => Ok(v),
                    None => match value.as_bool().and_then(|v| v.to_string().parse::<f64>().ok()).map(Value::from) {
                        Some(v) => Ok(v),
                        // None => Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Value {} is not a double", value)))),
                        None => {
                            // Handle the value error applying the Evolution Strategy
                            Evolution::evolve_field(&field.to_string(), value, metadata, updated_schema)
                        }
                    }
                }
            }
            // Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData,  format!("Value {} is not a double", value)))),
        }
        "boolean" => match value.as_bool().map(Value::from) {
            Some(v) => Ok(v),
            None => match value.as_str().and_then(|v| v.parse::<bool>().ok()).map(Value::from) {
                Some(v) => Ok(v),
                None => match value.as_i64().and_then(|v| v.to_string().parse::<bool>().ok()).map(Value::from) {
                    Some(v) => Ok(v),
                    None => match value.as_i64().and_then(|v| {
                        if v == 0 || v == 1 {
                            Some((v == 1).to_string().parse::<bool>().ok()).map(Value::from)
                        } else {
                            None
                        }
                    }) {
                        Some(v) => Ok(v),
                        // None => Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Value {} is not a boolean", value)))),
                        None => {
                            println!("Value {} is not a boolean", value);
                            // Handle the value error applying the Evolution Strategy
                            Evolution::evolve_field(&field.to_string(), value, metadata, updated_schema)
                        }
                    }
                }
            }
        }
        _ => Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Unknown data type '{}'", data_type)))),
    }

}

pub fn discover_ingest(
    field: &str,
    value: &Value,
    parent_field: Option<&str>,
    parent_data_type: Option<&str>,
    metadata: &mut HashMap<String, Metadata>,
    updated_schema: &mut String,
) -> String {
    let foo: AnalyseSchema = AnalyseSchema { i: 0 };

    let mut discoverd_data_type = &"string".to_string().clone();

    // For null values, we need to create a new field and default to string
    // This is to avoid constantly trying to discover the field and slowing ingestion
    // One could argue we should accept the speed penalty and simply ignore the field till we discover a type (if ever)
    if value.is_null() || (value.is_string() && value.as_str().unwrap_or_default().is_empty()) {

        let mut new_field: Metadata = Metadata::new().unwrap();
        new_field.determined_type = "string".to_string();

        metadata.insert(
            field.to_string(),
            new_field
        );
    } else {
        AnalyseSchema::analyse_field(
            &foo,
            &field.to_string(),
            value.clone().borrow_mut(),
            metadata,
        );

        let flatten = Config::truth_value(&Config::getenv("TRANSFORM_FLATTEN_EVENTS", "no"));

        AnalyseSchema::determine_field_types(metadata, None, None, flatten);

        discoverd_data_type = &metadata.get(field).unwrap().determined_type;
    }

    println!(
        "Discovered new field: '{}' of type: '{}' with parent: '{}'",
        field, discoverd_data_type, parent_field.unwrap_or_default()
    );

    // let handle = tokio::runtime::Handle::current();
    // handle.enter();
    // block_on(Config::set_config(&metadata, true));
    // spawn_blocking(Config::set_config(&metadata, true).await);

    //@todo - if mutable mode

    // exit(0);

    *updated_schema = "yes".to_string();

    discoverd_data_type.clone()
}

pub fn set_date(
    field: &str,
    value: &Value,
    metadata: &mut HashMap<String, Metadata>,
    mut updated_schema: &mut String
) -> Result<Value, Box<dyn std::error::Error>> {
    // Hive Timestamp doesn't support string dates
    match value.clone().as_str() {
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
                    Err(_) => Ok(Value::Null),
                },
                Err(err) => {
                    println!("Error date: {}", err);
                    Ok(Value::Null)
                }
            }
        }
        None => {
            // println!("Could not format date to int using format");
            // Handle the value error applying the Evolution Strategy
            Evolution::evolve_field(&field.to_string(), value, metadata, updated_schema)
            // Value::Null
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover::DateCandidate;
    use chrono::NaiveDateTime;
    
    use std::collections::HashMap;

    fn generate_metadata(field: &str, format_name: &str) -> HashMap<String, Metadata> {
        let date_candidate = DateCandidate {
            check_count: 1,
            valid_count: 1,
            field: String::from(field.clone()),
            format: String::from(format_name),
        };

        let mut meta = HashMap::new();

        meta.insert(
            String::from(field.clone()),
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
    fn test_set_date_with_valid_date() {
        let foo: AnalyseSchema = AnalyseSchema { i: 0 };

        let field = "test_field";

        let date_str = "2023-05-21 12:34:56";
        let format_name = foo.is_valid_date(date_str).unwrap();
        let format = DateFormats::from_str(format_name).unwrap().as_str();

        let mut meta = generate_metadata(field, format);

        let value = Value::String(String::from(date_str));

        let mut updated_schema = "no".to_string();

        let result = set_date(field, &value, &mut meta, &mut updated_schema);

        let expected_date = NaiveDateTime::parse_from_str(date_str, format).unwrap();
        let expected_millis =
            Value::Number(serde_json::Number::from(expected_date.timestamp() * 1000));

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), expected_millis);
    }

    #[test]
    fn test_set_date_with_valid_iso_date() {
        let foo: AnalyseSchema = AnalyseSchema { i: 0 };

        let field = "test_field";

        let date_str = "2023-05-23T07:09:03.000Z";
        let format_name = foo.is_valid_date(date_str).unwrap();
        let format = DateFormats::from_str(format_name).unwrap().as_str();

        let mut meta = generate_metadata(field, format);

        let value = Value::String(String::from(date_str));

        let mut updated_schema = "no".to_string();

        let result = set_date(field, &value, &mut meta, &mut updated_schema);

        let expected_date = NaiveDateTime::parse_from_str(date_str, format).unwrap();
        let expected_millis =
            Value::Number(serde_json::Number::from(expected_date.timestamp() * 1000));

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), expected_millis);
    }
}

#[cfg(test)]
mod test_discover_on_ingest {
    use serial_test::serial;
    use std::collections::HashMap;
    use std::fs::{remove_file, File, OpenOptions};
    use std::io::{Seek, Write};
    

    use parquet::data_type::AsBytes;
    use rand::Rng;
    use std::path::Path;

    use serde_json::{Number, Value};

    use crate::discover::AnalyseSchema;

    use crate::ingest::ingest::ingest;
    
    use crate::serdes::json::SerdeJson;

    #[test]
    #[serial]
    fn test_set_date_valid() {
        let mut foo: AnalyseSchema = AnalyseSchema { i: 0 };

        let field = r#"
        {
            "sheep": "dog",
            "arable": false,
               "crank": {
                "voltage": [2, 3, 4, 6, 7, 4, 3, 6, 7, 9],
                "start_temprature": 5,
                "end_temprature": 7,
                "engine": {
                    "details": {
                        "manufacturer": "General Electric",
                        "model": "PZ - 09 - 126178"
                    },
                    "rebuild_dates": [
                        "01/02/19/85",
                        "15/06/19/2005"
                    ]
                }
            },
            "crank_torques": [
                [2, 15, 33, 45, 56, 57, 47, 36, 19, 5],
                [1, 13, 33, 48, 56, 58, 45, 35, 15, 6]
            ],
            "hardware": {
                "maintenance": {
                  "last_rebuild": "20/04/2010",
                  "last_service": "12/07/1973"
                },
                "manufacturer": "Beier, Emmerich and Rutherford",
                "model": "synergize ubiquitous e-commerce"
            },
            "isbn": "9407496597",
            "last_crank": [2, 15, 33, 45, 56, 57, 47, 36, 19, 5],
            "metadata": {
                "prcd_micro_time": 1615474853.999185,
                "rcvd_time": 1615474895,
                "sent_time": 1615474930,
                "tags": [
                    {
                        "name": "type",
                        "value": "trip"
                    },
                    {
                        "name": "auto",
                        "value": false
                    }
                ]
            },
            "rider_id": "10e974bf-4a43-305a-9e39-1636c43cb22a",
            "trip": {
                "end_temprature": 2,
                "start_temprature": 0
            }
        }"#;

        let json: Value = serde_json::from_str(field).unwrap();

        let record_line = serde_json::to_string(&json).unwrap();

        let mut rng = rand::thread_rng();
        let random_tmp_file_name = rng.gen::<i32>();

        let mut test_file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .create_new(true)
            .open(format!("./{}", random_tmp_file_name))
            .unwrap();
        // let mut test_file = MemFile::create(rng.gen::<i32>(), CreateOptions::new()).unwrap();

        test_file.write(record_line.as_bytes()).unwrap();

        test_file.rewind().unwrap();

        let in_file = File::open(format!("./{}", random_tmp_file_name)).unwrap();
        // let mut in_file = MemFile::create(rng.gen::<i32>(), CreateOptions::new()).unwrap();

        let mut metadata = HashMap::new();

        AnalyseSchema::infer_json_schema(&mut foo, in_file, Some(1), &mut metadata);

        let str = r#"{"rider_id":"10e974bf-4a43-305a-9e39-1636c43cb22a","bike_id":"8b86f753-05f8-3254-aba6-739188a3c0b6","isbn":"9407496597","trip":{"start_temprature":0,"end_temprature":2},"last_crank":[2,15,33,45,56,57,47,36,19,5],"crank_torques":[[2,15,33,45,56,57,47,36,19,5],[1,13,33,48,56,58,45,35,15,6]],"hardware":{"manufacturer":"Beier, Emmerich and Rutherford","model":"synergize ubiquitous e-commerce","maintenance":{"last_rebuild":"20\/04\/2010","last_service":"12\/07\/1973"}},"metadata":{"rcvd_time":1615474895,"sent_time":1615474930,"prcd_micro_time":1615474853.999185,"tags":[{"name":"type","value":"trip"},{"name":"auto","value":false}]}}"#;

        let records: Vec<Value> = SerdeJson::deserialize(str);

        let mut updatedSchema = "no".to_string();

        // NOTE: This schema is assert tested in discovery
        let ingestValue = ingest(
            records.first().unwrap(),
            &mut metadata.get_mut("default").unwrap().fields,
            &mut updatedSchema,
            false,
        );

        let mut f: Vec<Value> = vec![];
        f.insert(0, Value::Number(Number::from(2)));
        f.insert(1, Value::Number(Number::from(15)));
        f.insert(2, Value::Number(Number::from(33)));
        f.insert(3, Value::Number(Number::from(45)));
        f.insert(4, Value::Number(Number::from(56)));
        f.insert(5, Value::Number(Number::from(57)));
        f.insert(6, Value::Number(Number::from(47)));
        f.insert(7, Value::Number(Number::from(36)));
        f.insert(8, Value::Number(Number::from(19)));
        f.insert(9, Value::Number(Number::from(5)));

        let v = ingestValue.get("last_crank").unwrap().as_array().unwrap();

        assert_eq!(&f, v);

        let mut map = serde_json::Map::new();
        map.insert("end_temprature".to_string(), Value::Number(Number::from(2)));
        map.insert(
            "start_temprature".to_string(),
            Value::Number(Number::from(0)),
        );

        let trip_map = ingestValue.get("trip").unwrap().as_object().unwrap();

        assert_eq!(&map, trip_map);

        remove_file(Path::new(&format!("./{}", random_tmp_file_name)));
    }
}
