use crate::discover::date_formats::DateFormats;
use crate::discover::{AnalyseSchema, Metadata, SkipprDataType};
use crate::helpers::configuration::Config;
use crate::helpers::Helpers;
use chrono::{DateTime, Utc};
use once_cell::sync::Lazy;
use serde_json::{Map, Value};
use std::borrow::BorrowMut;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{debug, info};

thread_local! {
    static CURRENT_NAMESPACE: RefCell<String> = RefCell::new(String::new());
}

static PER_NAMESPACE_STATS: Lazy<Mutex<HashMap<String, (usize, Instant)>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static SEEN_FIELDS_BY_NAMESPACE: Lazy<Mutex<HashMap<String, HashSet<String>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

use crate::discover::evolution::Evolution;
use crate::ingest::fast_ingest::DEFAULT_NESTED_MESSAGE;

#[derive(Debug)]
pub struct ResolvedFieldValue {
    pub(crate) field: String,
    pub(crate) value: Value,
}

impl ResolvedFieldValue {
    pub fn new(field: String, value: Value) -> ResolvedFieldValue {
        ResolvedFieldValue { field, value }
    }
}

// pub fn ingest_buf<R: Read>(reader: &mut BufReader<R>) -> ValueIter<R> {
//     ValueIter::new(reader, None)
// }

// pub fn fast_path_ingest(unwrapped_message: &mut IngestRecord, metadata: &mut HashMap<String, Metadata>) -> HashMap<String, Message<Value>> {
// pub fn fast_path_ingest(unwrapped_message: &mut IngestRecord, metadata: &mut HashMap<String, Metadata>) {
pub fn ingest(
    unwrapped_message: &Value,
    metadata: &mut HashMap<String, Metadata>,
    namespace: &str,
    updated_schema: &mut String,
    flatten: bool,
) -> Result<Value, Box<dyn std::error::Error>> {
    // let mut helpers = Helpers { clean_field_cache: Default::default() };

    // let mut message = default_msgs[namespace].clone();
    // let mut message = SerderParquet::default_message(metadata);

    // let mut message: Vec<Value> = Vec::with_capacity(batch_size);
    // let mut message: Value = Value::Null;
    let mut message: Value;
    {
        message = match DEFAULT_NESTED_MESSAGE.read().get(namespace) {
            Some(m) => m.clone(),
            None => {
                debug!(
                    "ingest: no DEFAULT_NESTED_MESSAGE for ns={}, starting with empty object",
                    namespace
                );
                Value::Object(Map::new())
            }
        };
    }

    CURRENT_NAMESPACE.with(|ns| {
        *ns.borrow_mut() = namespace.to_string();
    });

    let _i = 0;

    let obj = match unwrapped_message.as_object() {
        Some(o) => o,
        None => {
            debug!("ingest: input was not an object for ns={}", namespace);
            return Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "ingest expects a JSON object record",
            )));
        }
    };
    for (field, value) in obj {
        // let field = Helpers::clean_field_name(field.to_string());

        // println!("Ingesting field: {:?}", field);

        // if special_fields.contains_key(field) {
        //     message.insert(field.to_string(), value.to_string());
        // } else {
        //         let resolved_value = Value::Null;

        let field_data_type = match metadata.get_mut(&field.to_string()) {
            Some(data_type) => {
                // println!("{:?}",  data_type.determined_type.clone());
                data_type.determined_type.to_string()
            }
            None => "".to_string(),
        };

        let resolved_value = match set_value(
            &field_data_type,
            &field.to_string(),
            value,
            None,
            None,
            metadata,
            updated_schema,
            true,
            flatten,
        ) {
            Ok(v) => v,
            Err(_e) => {
                // apply evolution strategy
                match Evolution::evolve_field(
                    &field.to_string(),
                    value,
                    None,
                    None,
                    metadata,
                    updated_schema,
                    flatten,
                ) {
                    Ok(v) => v,
                    Err(e) => return Err(e),
                }
            }
        };

        // println!("Setting message with field: {:?} and value {:?}", field, resolved_value);

        // let foo = resolved_value;
        // ignore if null, use default message which has correct null for data type
        if !resolved_value.value.is_null() {
            message[resolved_value.field] = resolved_value.value;

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
    Ok(message)
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
    allow_evolve: bool,
    flatten: bool,
) -> Result<ResolvedFieldValue, Box<dyn std::error::Error>> {
    // let _parent_type = match metadata.get_mut(field) {
    //     Some(pt) => &pt.parent_type,
    //     None => ""
    // };

    // if data_type != "" || parent_type == "map" {
    if !data_type.is_empty() {
        if value.is_null() {
            return Ok(ResolvedFieldValue::new(field.to_string(), Value::Null));
        }
        if value.is_string() && value.as_str().unwrap_or_default().is_empty() {
            return Ok(ResolvedFieldValue::new(field.to_string(), Value::Null));
        }

        // let data_type: &str = &metadata.get_mut(field).unwrap().determined_type;
        // let data_type = "record";

        let x: Value;
        let mut new_value = Ok(ResolvedFieldValue::new(field.to_string(), Value::Null));

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
                                    .to_string(),
                                &sub_field.to_string(),
                                // &mut sub_value.as_str().unwrap_or(&value.to_string()), // pass string val or string representation of map/array, etc
                                sub_value,
                                Some(field),
                                Some(data_type),
                                &mut metadata.get_mut(&field.to_string()).unwrap().fields,
                                updated_schema,
                                allow_evolve,
                                flatten,
                            );

                            match newval {
                                Ok(v) => {
                                    m.insert(v.field, v.value);
                                }
                                Err(e) => return Err(e),
                            }
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
                                    .to_string(),
                                &i.to_string(),
                                // &mut sub_value.as_str().unwrap_or(&value.to_string()), // pass string val or string representation of map/array, etc
                                sub_value,
                                Some(field),
                                Some(data_type),
                                &mut metadata.get_mut(&field.to_string()).unwrap().fields,
                                updated_schema,
                                allow_evolve,
                                flatten,
                            );

                            match newval {
                                Ok(v) => {
                                    m.insert(v.field, v.value);
                                }
                                Err(e) => return Err(e),
                            }
                        }

                        i += 1;
                    }
                }

                x = m.into();
                new_value = Ok(ResolvedFieldValue::new(field.to_string(), x));
            } else if data_type == "map" {
                if value.is_object() {
                    let mut m = Map::new();

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

                                let newval = set_value(
                                    &metadata
                                        .get_mut(field)
                                        .unwrap()
                                        .fields
                                        .get_mut(&key.to_string())
                                        .unwrap()
                                        .determined_type
                                        .to_string(),
                                    &key.to_string(),
                                    val,
                                    Some(field),
                                    Some(data_type),
                                    &mut metadata.get_mut(field).unwrap().fields,
                                    updated_schema,
                                    allow_evolve,
                                    flatten,
                                );

                                match newval {
                                    Ok(v) => {
                                        m.insert(v.field, v.value);
                                    }
                                    Err(e) => return Err(e),
                                }
                            }
                        }
                    }

                    new_value = Ok(ResolvedFieldValue::new(field.to_string(), m.into()));
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
                let flatten = Config::get_transform_flatten_events();

                if metadata.get(field).unwrap().determined_type_values == Some(SkipprDataType::Record)
                    && value.is_array()
                {
                    let mut arr_new_value: Vec<Value> = Vec::new();

                    for (i, sub_value) in value.as_array().unwrap().iter().enumerate() {
                        // let index = flatten ? &i.to_string() : &0.to_string();
                        let index = match flatten {
                            true => &i.to_string(),
                            false => &0.to_string(),
                        };

                        match metadata.get(&field.to_string()).unwrap().fields.get(index) {
                            Some(_t) => (),
                            None => {
                                // println!("({}.array) no metadata for {} => {} with value: {}", data_type, &field.to_string(), i.to_string(), sub_value);
                                // discover_ingest(&field.to_string(), value, metadata, updatedSchema, flatten);
                                discover_ingest(
                                    index,
                                    sub_value,
                                    Some(field),
                                    Some(data_type),
                                    &mut metadata.get_mut(&field.to_string()).unwrap().fields,
                                    updated_schema,
                                );
                            }
                        }

                        let parent_type = match flatten {
                            true => Some("record"),
                            false => Some("array"),
                        };

                        let foo = set_value(
                            "record",
                            index,
                            sub_value,
                            Some(field),
                            parent_type,
                            &mut metadata.get_mut(field).unwrap().fields,
                            updated_schema,
                            true,
                            flatten,
                        );

                        // println!("Array ingested field: {:?}", foo);

                        let val = match foo {
                            Ok(v) => v,
                            Err(e) => return Err(e),
                        };
                        arr_new_value.insert(i, val.value);
                    }

                    // i += 1;

                    // println!("Array ingested array: {:?}", arr_new_value);

                    new_value = Ok(ResolvedFieldValue::new(
                        field.to_string(),
                        arr_new_value.into(),
                    ));
                } else {
                    // println!("#### ingesting array: {}", field);

                    let mut arr_new_value: Vec<Value> = Vec::new();

                    let mut values_valid = true;

                    match value.as_array() {
                        Some(t) => {
                            for (i, sub_value) in t.iter().enumerate() {
                                if !values_valid {
                                    return Err(Box::new(std::io::Error::new(
                                        std::io::ErrorKind::InvalidData,
                                        format!("Array values not match type: {}", field),
                                    )));

                                    // println!("#### array values not match type: {}", field);
                                    //
                                    // arr_new_value.clear();
                                    //
                                    //
                                    // // // evolve array field
                                    // let foo = Evolution::evolve_field(&field.to_string(), value, parent_field, parent_data_type, metadata, updated_schema);
                                    //
                                    // match foo {
                                    //     Ok(v) => {
                                    //         println!("#### Evolved array field: {} with value: {}", field, v.value);
                                    //         arr_new_value = v.value.as_array().unwrap().to_vec();
                                    //
                                    //         return Ok(ResolvedFieldValue::new(v.field, arr_new_value.into()));
                                    //
                                    //     },
                                    //     Err(e) => {
                                    //         return Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Array values not match type: {}", field))));
                                    //     }
                                    // }
                                } else {
                                    match metadata.get(&field.to_string()) {
                                        Some(_t) => (),
                                        None => {
                                            // println!("({}.array) no metadata for {} => {} with value: {}", data_type, &field.to_string(), i.to_string(), sub_value);
                                            // discover_ingest(&field.to_string(), value, metadata, updatedSchema, flatten);
                                            discover_ingest(
                                                field,
                                                sub_value,
                                                parent_field,
                                                parent_data_type,
                                                &mut metadata.borrow_mut(),
                                                updated_schema,
                                            );

                                            // println!("({}.array) no metadata for {} => {} with value: {}", data_type, &field.to_string(), i.to_string(), sub_value);
                                            // println!("metadata {:?}", metadata)
                                        }
                                    }

                                    let sub_data_type = metadata
                                        .get(&field.to_string())
                                        .unwrap()
                                        .determined_type_values
                                        .as_ref()
                                        .map(|t| t.to_string())
                                        .unwrap_or_default();
                                    let foo = set_value(
                                        &sub_data_type,
                                        field,
                                        sub_value,
                                        parent_field,
                                        parent_data_type,
                                        &mut metadata.borrow_mut(),
                                        updated_schema,
                                        false,
                                        flatten,
                                    );

                                    let res = match foo {
                                        Ok(v) => v,
                                        Err(_e) => {
                                            // println!("Error: {}", e);
                                            values_valid = false;
                                            ResolvedFieldValue::new(field.to_string(), Value::Null)
                                        }
                                    };

                                    arr_new_value.insert(i, res.value);
                                }
                            }
                            new_value = Ok(ResolvedFieldValue::new(
                                field.to_string(),
                                arr_new_value.into(),
                            ));
                        }
                        None => {
                            new_value = Ok(ResolvedFieldValue::new(field.to_string(), Value::Null));
                        }
                    }

                    // new_value = value.to_owned();
                }
            } else {
                // println!("value is {}", value);
                // println!("field is {}", field);
                // println!("data_type is {}", data_type);

                if data_type == "date" {
                    new_value = set_date(
                        field,
                        value,
                        parent_field,
                        parent_data_type,
                        metadata,
                        updated_schema,
                        flatten,
                    );
                    // let date_new_value = set_date(field, value, parent_field, parent_data_type, metadata, updated_schema);
                    // new_value = date_new_value.unwrap_or(ResolvedFieldValue::new(field.to_string(), Value::Null));
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
                    new_value = match_scalar_value(
                        field,
                        data_type,
                        value,
                        parent_field,
                        parent_data_type,
                        metadata,
                        updated_schema,
                        allow_evolve,
                        flatten,
                    );

                    // let scalar_value = match_scalar_value(field, data_type, value, parent_field, parent_data_type, metadata, updated_schema);

                    // new_value = scalar_value.unwrap_or(ResolvedFieldValue::new(field.to_string(), Value::Null));
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
                allow_evolve,
                flatten,
            );
        } else {
            return Ok(ResolvedFieldValue::new(field.to_string(), Value::Null));
        }
    }
}

fn match_scalar_value(
    field: &str,
    data_type: &str,
    value: &Value,
    parent_field: Option<&str>,
    parent_data_type: Option<&str>,
    metadata: &mut HashMap<String, Metadata>,
    updated_schema: &mut String,
    allow_evolve: bool,
    flatten: bool,
) -> Result<ResolvedFieldValue, Box<dyn std::error::Error>> {
    // Return early for null values
    if value.is_null() {
        return Ok(ResolvedFieldValue::new(
            Metadata::get_field_out_field_name(metadata, field),
            Value::Null,
        ));
    }

    // Use cached field name lookups to reduce repetitive transformations
    let output_field_name = Metadata::get_field_out_field_name(metadata, field);

    match data_type {
        "string" => match value.as_str().map(Value::from) {
            Some(v) => Ok(ResolvedFieldValue::new(output_field_name, v)),
            None => match value.as_i64().map(|v| v.to_string()).map(Value::from) {
                Some(v) => Ok(ResolvedFieldValue::new(output_field_name, v)),
                None => match value.as_f64().map(|v| v.to_string()).map(Value::from) {
                    Some(v) => Ok(ResolvedFieldValue::new(output_field_name, v)),
                    None => match value.as_bool().map(|v| v.to_string()).map(Value::from) {
                        Some(v) => Ok(ResolvedFieldValue::new(output_field_name, v)),
                        None => {
                            if !allow_evolve {
                                return Err(Box::new(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    format!(
                                        "Field: {} => {} value: {} is not a string",
                                        parent_field.unwrap_or("root"),
                                        field,
                                        value
                                    ),
                                )));
                            }
                            // Handle the value error applying the Evolution Strategy
                            Evolution::evolve_field(
                                &field.to_string(),
                                value,
                                parent_field,
                                parent_data_type,
                                metadata,
                                updated_schema,
                                flatten,
                            )
                        }
                    },
                },
            },
        },
        "int" | "integer" => match value.as_i64().map(|v| v as i32).map(Value::from) {
            Some(v) => Ok(ResolvedFieldValue::new(output_field_name, v)),
            None => match value
                .as_str()
                .and_then(|v| v.parse::<i32>().ok())
                .map(Value::from)
            {
                Some(v) => Ok(ResolvedFieldValue::new(output_field_name, v)),
                None => match value
                    .as_f64()
                    .and_then(|v| v.to_string().parse::<i32>().ok())
                    .map(Value::from)
                {
                    Some(v) => Ok(ResolvedFieldValue::new(output_field_name, v)),
                    None => match value
                        .as_bool()
                        .and_then(|v| v.to_string().parse::<i32>().ok())
                        .map(Value::from)
                    {
                        Some(v) => Ok(ResolvedFieldValue::new(output_field_name, v)),
                        None => {
                            if !allow_evolve {
                                return Err(Box::new(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    format!(
                                        "Field: {} => {} value: {} is not an integer",
                                        parent_field.unwrap_or("root"),
                                        field,
                                        value
                                    ),
                                )));
                            }
                            // Handle the value error applying the Evolution Strategy
                            Evolution::evolve_field(
                                &field.to_string(),
                                value,
                                parent_field,
                                parent_data_type,
                                metadata,
                                updated_schema,
                                flatten,
                            )
                        }
                    },
                },
            },
        },
        "timestamp" | "timestamp_milli" | "long" => match value.as_i64().map(Value::from) {
            Some(v) => {
                if data_type == "timestamp_milli" || data_type == "timestamp" {
                    Ok(ResolvedFieldValue::new(
                        output_field_name,
                        AnalyseSchema::coerce_to_milli_seconds(v),
                    ))
                } else {
                    Ok(ResolvedFieldValue::new(output_field_name, v))
                }
            }
            None => match value
                .as_str()
                .and_then(|v| v.parse::<i64>().ok())
                .map(Value::from)
            {
                Some(v) => {
                    if data_type == "timestamp_milli" || data_type == "timestamp" {
                        Ok(ResolvedFieldValue::new(
                            output_field_name,
                            AnalyseSchema::coerce_to_milli_seconds(v),
                        ))
                    } else {
                        Ok(ResolvedFieldValue::new(output_field_name, v))
                    }
                }
                None => match value
                    .as_f64()
                    .and_then(|v| v.to_string().parse::<i64>().ok())
                    .map(Value::from)
                {
                    Some(v) => {
                        if data_type == "timestamp_milli" || data_type == "timestamp" {
                            Ok(ResolvedFieldValue::new(
                                output_field_name,
                                AnalyseSchema::coerce_to_milli_seconds(v),
                            ))
                        } else {
                            Ok(ResolvedFieldValue::new(output_field_name, v))
                        }
                    }
                    None => match value
                        .as_bool()
                        .and_then(|v| v.to_string().parse::<i64>().ok())
                        .map(Value::from)
                    {
                        Some(v) => {
                            if data_type == "timestamp_milli" || data_type == "timestamp" {
                                Ok(ResolvedFieldValue::new(
                                    output_field_name,
                                    AnalyseSchema::coerce_to_milli_seconds(v),
                                ))
                            } else {
                                Ok(ResolvedFieldValue::new(output_field_name, v))
                            }
                        }
                        None => {
                            if !allow_evolve {
                                return Err(Box::new(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    format!(
                                        "Field: {} => {} value: {} is not an integer",
                                        parent_field.unwrap_or("root"),
                                        field,
                                        value
                                    ),
                                )));
                            }
                            // Handle the value error applying the Evolution Strategy
                            Evolution::evolve_field(
                                &field.to_string(),
                                value,
                                parent_field,
                                parent_data_type,
                                metadata,
                                updated_schema,
                                flatten,
                            )
                        }
                    },
                },
            },
        },
        "double" => match value.as_f64().map(Value::from) {
            Some(v) => Ok(ResolvedFieldValue::new(output_field_name, v)),
            None => match value
                .as_str()
                .and_then(|v| v.parse::<f64>().ok())
                .map(Value::from)
            {
                Some(v) => Ok(ResolvedFieldValue::new(output_field_name, v)),
                None => match value
                    .as_i64()
                    .and_then(|v| v.to_string().parse::<f64>().ok())
                    .map(Value::from)
                {
                    Some(v) => Ok(ResolvedFieldValue::new(output_field_name, v)),
                    None => match value
                        .as_bool()
                        .and_then(|v| v.to_string().parse::<f64>().ok())
                        .map(Value::from)
                    {
                        Some(v) => Ok(ResolvedFieldValue::new(output_field_name, v)),
                        None => {
                            if !allow_evolve {
                                return Err(Box::new(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    format!(
                                        "Field: {} => {} value: {} is not a double",
                                        parent_field.unwrap_or("root"),
                                        field,
                                        value
                                    ),
                                )));
                            }
                            // Handle the value error applying the Evolution Strategy
                            Evolution::evolve_field(
                                &field.to_string(),
                                value,
                                parent_field,
                                parent_data_type,
                                metadata,
                                updated_schema,
                                flatten,
                            )
                        }
                    },
                },
            },
        },
        "boolean" => match value.as_bool().map(Value::from) {
            Some(v) => Ok(ResolvedFieldValue::new(output_field_name, v)),
            None => match value
                .as_str()
                .and_then(|v| v.parse::<bool>().ok())
                .map(Value::from)
            {
                Some(v) => Ok(ResolvedFieldValue::new(output_field_name, v)),
                None => match value
                    .as_i64()
                    .and_then(|v| v.to_string().parse::<bool>().ok())
                    .map(Value::from)
                {
                    Some(v) => Ok(ResolvedFieldValue::new(output_field_name, v)),
                    None => match value.as_i64().and_then(|v| {
                        if v == 0 || v == 1 {
                            Some((v == 1).to_string().parse::<bool>().ok()).map(Value::from)
                        } else {
                            None
                        }
                    }) {
                        Some(v) => Ok(ResolvedFieldValue::new(output_field_name, v)),
                        None => {
                            if !allow_evolve {
                                return Err(Box::new(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    format!(
                                        "Field: {} => {} value: {} is not a boolean",
                                        parent_field.unwrap_or("root"),
                                        field,
                                        value
                                    ),
                                )));
                            }
                            // Handle the value error applying the Evolution Strategy
                            Evolution::evolve_field(
                                &field.to_string(),
                                value,
                                parent_field,
                                parent_data_type,
                                metadata,
                                updated_schema,
                                flatten,
                            )
                        }
                    },
                },
            },
        },
        _ => Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Unknown data type '{}'", data_type),
        ))),
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
    static TOTAL_NEW_FIELDS: Lazy<AtomicUsize> = Lazy::new(|| AtomicUsize::new(0));

    let _foo: AnalyseSchema = AnalyseSchema { i: 0 };
    let _was_present = metadata.contains_key(field);

    let discoverd_data_type = SkipprDataType::String;

    // For null values, we need to create a new field and default to string
    // This is to avoid constantly trying to discover the field and slowing ingestion
    // One could argue we should accept the speed penalty and simply ignore the field till we discover a type (if ever)
    if value.is_null() || (value.is_string() && value.as_str().unwrap_or_default().is_empty()) {
        let mut new_field: Metadata = Metadata::new().unwrap();
        new_field.determined_type = discoverd_data_type;

        metadata.insert(field.to_string(), new_field);
    } else {
        AnalyseSchema::analyse_field(
            &_foo,
            &field.to_string(),
            value.clone().borrow_mut(),
            metadata,
        );

        // println!("Determining field types for field: {} with type: {} and Parent: {}", field, metadata.get(field).unwrap().determined_type, parent_field.unwrap_or_default());

        // if (parent_field.is_some()) {
        //     AnalyseSchema::determine_field_types(metadata.get_mut(field).unwrap().fields.as_mut(), parent_data_type, parent_field, flatten);
        // } else {
    }

    let flatten = Config::get_transform_flatten_events();

    AnalyseSchema::determine_field_types(metadata, parent_data_type, flatten);

    // discoverd_data_type = metadata.get(field).unwrap().determined_type;

    // Determine if this is a new field for the current process run and namespace
    let ns = CURRENT_NAMESPACE.with(|ns| ns.borrow().clone());
    let mut is_new_for_ns = false;
    if !ns.is_empty() {
        let mut seen = SEEN_FIELDS_BY_NAMESPACE.lock().unwrap();
        let entry = seen.entry(ns.clone()).or_insert_with(HashSet::new);
        // Compose a stable key using parent and field to reduce duplicates for nested structures
        let key = match parent_field {
            Some(p) if !p.is_empty() => format!("{}.{}", p, field),
            _ => field.to_string(),
        };
        if !entry.contains(&key) {
            entry.insert(key);
            is_new_for_ns = true;
        }
    }

    // Only log when first seen in this run for the namespace
    if is_new_for_ns {
        // Update global total
        let _ = TOTAL_NEW_FIELDS.fetch_add(1, Ordering::Relaxed) + 1;

        // Per-namespace stats and summaries
        let mut stats = PER_NAMESPACE_STATS.lock().unwrap();
        let entry = stats.entry(ns.clone()).or_insert((0, Instant::now()));

        // Use the unique count from the seen set if available to avoid drift
        let unique_count = SEEN_FIELDS_BY_NAMESPACE
            .lock()
            .ok()
            .and_then(|m| m.get(&ns).map(|s| s.len()))
            .unwrap_or(entry.0);
        entry.0 = unique_count;

        // Suppress detailed logs after a small threshold; keep summaries
        const DETAIL_LIMIT: usize = 50;
        if entry.0 <= DETAIL_LIMIT {
            info!(
                "Discovered new field: {} of type: {} on namespace '{}'{}{}",
                field,
                metadata.get(field).unwrap().determined_type,
                ns,
                if parent_field.is_some() {
                    " (parent: "
                } else {
                    ""
                },
                if parent_field.is_some() {
                    format!("{})", parent_field.unwrap())
                } else {
                    String::new()
                }
            );
        }

        let elapsed_ns = entry.1.elapsed();
        if entry.0 % 100 == 0 || elapsed_ns >= Duration::from_secs(10) {
            info!(
                "Discovered {} new fields so far on namespace '{}'",
                entry.0, ns
            );
            entry.1 = Instant::now();
        }
    }

    // let handle = tokio::runtime::Handle::current();
    // handle.enter();
    // block_on(Config::set_config(&metadata, true));
    // spawn_blocking(Config::set_config(&metadata, true).await);

    //@todo - if mutable mode

    // exit(0);

    *updated_schema = "yes".to_string();

    metadata.get(field).unwrap().determined_type.to_string()
}

pub fn set_date(
    field: &str,
    value: &Value,
    parent_field: Option<&str>,
    _parent_data_type: Option<&str>,
    metadata: &mut HashMap<String, Metadata>,
    _updated_schema: &mut String,
    _flatten: bool,
) -> Result<ResolvedFieldValue, Box<dyn std::error::Error>> {
    // Use cached field name lookups to reduce repetitive transformations
    let output_field_name = Metadata::get_field_out_field_name(metadata, field);

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
                Ok(f) => {
                    let meta = metadata.get(field).unwrap();
                    let kind = meta.date_parser_kind.clone();
                    let tz = meta.timezone;
                    // Select parser by kind; avoid string inspections in hot path
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
                        Some(ms) => Ok(ResolvedFieldValue::new(output_field_name, ms.into())),
                        None => Ok(ResolvedFieldValue::new(output_field_name, Value::Null)),
                    }
                }
                Err(err) => {
                    info!("Error date: {}", err);
                    Ok(ResolvedFieldValue::new(output_field_name, Value::Null))
                }
            }
        }
        None => {
            // Handle the value error applying the Evolution Strategy
            // No direct evolution here; return error so caller can propose via sequencer
            Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "Field: {} => {} value: {} mismatched",
                    parent_field.unwrap_or("root"),
                    field,
                    value
                ),
            )))
        }
    }
}

#[cfg(test)]
mod tests_set_date {
    use super::*;
    use crate::discover::DateCandidate;
    #[allow(unused_imports)]
    use chrono::{FixedOffset, NaiveDateTime};

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
    fn test_set_date_with_valid_date() {
        let _foo: AnalyseSchema = AnalyseSchema { i: 0 };

        let field = "test_field";

        let date_str = "2023-05-21 12:34:56";
        let format_name = AnalyseSchema::is_valid_date(date_str).unwrap();
        let format = DateFormats::from_str(format_name).unwrap().as_str();

        let mut meta = generate_metadata(field, format_name);

        let value = Value::String(String::from(date_str));

        let mut updated_schema = "no".to_string();

        let result = set_date(
            field,
            &value,
            None,
            None,
            &mut meta,
            &mut updated_schema,
            false,
        );

        let expected_date = Helpers::parse_date_from_string(date_str, format).unwrap();

        let expected_millis =
            Value::Number(serde_json::Number::from(expected_date.timestamp() * 1000));

        assert!(result.is_ok());
        assert_eq!(result.unwrap().value, expected_millis);
    }

    #[test]
    fn test_set_date_with_valid_iso_date() {
        let _foo: AnalyseSchema = AnalyseSchema { i: 0 };

        let field = "test_field";

        let date_str = "2023-05-23T07:09:03.000Z";
        let format_name = AnalyseSchema::is_valid_date(date_str).unwrap();
        let format = DateFormats::from_str(format_name).unwrap().as_str();

        let mut meta = generate_metadata(field, format_name);

        let value = Value::String(String::from(date_str));

        let mut updated_schema = "no".to_string();

        let result = set_date(
            field,
            &value,
            None,
            None,
            &mut meta,
            &mut updated_schema,
            false,
        );

        let expected_date = Helpers::parse_date_from_string(date_str, format).unwrap();

        let mills = expected_date.timestamp() * 1000;
        let expected_millis: Value = mills.into();
        // Value::Number(serde_json::Number::from(expected_date.timestamp() * 1000));

        assert!(result.is_ok());
        assert_eq!(result.unwrap().value, expected_millis);
    }

    #[test]
    fn test_set_date_with_valid_iso_timezone_date() {
        let _foo: AnalyseSchema = AnalyseSchema { i: 0 };

        let field = "test_field";

        let date_str = "2023-12-11T15:49:31+01:00";
        let format_name = AnalyseSchema::is_valid_date(date_str).unwrap();
        let format = DateFormats::from_str(format_name).unwrap().as_str();

        let mut meta = generate_metadata(field, format_name);

        let value = Value::String(String::from(date_str));

        let mut updated_schema = "no".to_string();

        let result = set_date(
            field,
            &value,
            None,
            None,
            &mut meta,
            &mut updated_schema,
            false,
        );

        let expected_date = Helpers::parse_date_from_string(date_str, format).unwrap();

        let expected_millis =
            Value::Number(serde_json::Number::from(expected_date.timestamp() * 1000));

        assert!(result.is_ok());
        assert_eq!(result.unwrap().value, expected_millis);
    }
}

#[cfg(test)]
mod test_smoke_tests {
    use serial_test::serial;
    use std::collections::HashMap;
    #[allow(unused_imports)]
    use std::fs::{remove_file, File, OpenOptions};
    #[allow(unused_imports)]
    use std::io::{Seek, Write};

    #[allow(unused_imports)]
    use parquet::data_type::AsBytes;
    use rand::Rng;
    #[allow(unused_imports)]
    use std::path::Path;

    use serde_json::{Number, Value};

    use crate::discover::AnalyseSchema;

    use crate::ingest::ingest::ingest;

    use crate::serdes::json::SerdeJson;

    #[test]
    #[serial]
    fn test_discover_and_ingest() {
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

        let mut record_line = serde_json::to_string(&json).unwrap();

        let mut _rng = rand::thread_rng();
        let _random_tmp_file_name = _rng.gen::<u64>();

        // let mut test_file = OpenOptions::new()
        //     .write(true)
        //     .truncate(true)
        //     .create_new(true)
        //     .open(format!("./{}", random_tmp_file_name))
        //     .unwrap();
        // // let mut test_file = MemFile::create(rng.gen::<i32>(), CreateOptions::new()).unwrap();
        //
        // test_file.write(record_line.as_bytes()).unwrap();
        //
        // test_file.rewind().unwrap();

        // let in_file = File::open(format!("./{}", random_tmp_file_name)).unwrap();
        // let mut in_file = MemFile::create(rng.gen::<i32>(), CreateOptions::new()).unwrap();

        let mut metadata = HashMap::new();

        AnalyseSchema::infer_json_schema(&mut foo, &mut record_line, Some(1), &mut metadata);

        let str = r#"{"rider_id":"10e974bf-4a43-305a-9e39-1636c43cb22a","bike_id":"8b86f753-05f8-3254-aba6-739188a3c0b6","isbn":"9407496597","trip":{"start_temprature":0,"end_temprature":2},"last_crank":[2,15,33,45,56,57,47,36,19,5],"crank_torques":[[2,15,33,45,56,57,47,36,19,5],[1,13,33,48,56,58,45,35,15,6]],"hardware":{"manufacturer":"Beier, Emmerich and Rutherford","model":"synergize ubiquitous e-commerce","maintenance":{"last_rebuild":"20\/04\/2010","last_service":"12\/07\/1973"}},"metadata":{"rcvd_time":1615474895,"sent_time":1615474930,"prcd_micro_time":1615474853.999185,"tags":[{"name":"type","value":"trip"},{"name":"auto","value":false}]}}"#;

        let records: Vec<Value> = SerdeJson::deserialize(str);

        let mut updated_schema = "no".to_string();

        // NOTE: This schema is assert tested in discovery
        let ingest_value = ingest(
            records.first().unwrap(),
            &mut metadata.get_mut("default").unwrap().fields,
            "foo",
            &mut updated_schema,
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

        let value_clone = ingest_value.unwrap().clone();

        let v = value_clone
            .get("last_crank")
            .unwrap()
            .as_array()
            .unwrap()
            .clone();

        assert_eq!(&f, &v);

        let mut map = serde_json::Map::new();
        map.insert("end_temprature".to_string(), Value::Number(Number::from(2)));
        map.insert(
            "start_temprature".to_string(),
            Value::Number(Number::from(0)),
        );

        let trip_map = value_clone
            .get("trip")
            .unwrap()
            .as_object()
            .unwrap()
            .clone();

        assert_eq!(&map, &trip_map);

        // remove_file(Path::new(&format!("./{}", random_tmp_file_name))).unwrap();
    }
}
