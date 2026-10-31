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

pub(crate) fn current_namespace() -> String {
    CURRENT_NAMESPACE.with(|ns| ns.borrow().clone())
}

static PER_NAMESPACE_STATS: Lazy<Mutex<HashMap<String, (usize, Instant)>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static SEEN_FIELDS_BY_NAMESPACE: Lazy<Mutex<HashMap<String, HashSet<String>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

use crate::discover::evolution::Evolution;
use crate::ingest::fast_ingest::{validate_required_fields, DEFAULT_NESTED_MESSAGE};
use crate::ingest::scalar::{
    coerce_scalar, scalar_mismatch, skippr_data_type_is_scalar_or_structural,
};

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
    config: &Config,
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
        message = match DEFAULT_NESTED_MESSAGE.get(namespace) {
            Some(m) => m.as_ref().clone(),
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
            config,
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
            Err(e) => match evolve_or_err(
                config,
                field,
                value,
                None,
                None,
                metadata,
                updated_schema,
                true,
                flatten,
                e,
            ) {
                Ok(v) => v,
                Err(e) => return Err(e),
            },
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
    validate_required_fields(metadata, &message)?;

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
    config: &Config,
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
        let dt = SkipprDataType::from_str(data_type);
        skippr_data_type_is_scalar_or_structural(dt.clone());

        if dt == SkipprDataType::Unknown {
            let discoverd_data_type = discover_ingest(
                config,
                &field.to_string(),
                value,
                parent_field,
                parent_data_type,
                metadata,
                updated_schema,
            );
            if !discoverd_data_type.is_empty() && discoverd_data_type != "unknown" {
                return set_value(
                    config,
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
            }
            return Ok(ResolvedFieldValue::new(field.to_string(), Value::Null));
        }

        if !value.to_string().is_empty() {
            if dt == SkipprDataType::Record {
                if !value.is_object() && !value.is_array() {
                    return evolve_own_shape(
                        config,
                        field,
                        value,
                        parent_field,
                        parent_data_type,
                        metadata,
                        updated_schema,
                        allow_evolve,
                        flatten,
                    );
                }
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
                                    config,
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
                                config,
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
                                Err(e) => {
                                    let evolved = evolve_or_err(
                                        config,
                                        sub_field,
                                        sub_value,
                                        Some(field),
                                        Some(data_type),
                                        &mut metadata.get_mut(&field.to_string()).unwrap().fields,
                                        updated_schema,
                                        allow_evolve,
                                        flatten,
                                        e,
                                    )?;
                                    m.insert(evolved.field, evolved.value);
                                }
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
                                    config,
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
                                config,
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
                                Err(e) => {
                                    let evolved = evolve_or_err(
                                        config,
                                        &i.to_string(),
                                        sub_value,
                                        Some(field),
                                        Some(data_type),
                                        &mut metadata.get_mut(&field.to_string()).unwrap().fields,
                                        updated_schema,
                                        allow_evolve,
                                        flatten,
                                        e,
                                    )?;
                                    m.insert(evolved.field, evolved.value);
                                }
                            }
                        }

                        i += 1;
                    }
                }

                x = m.into();
                new_value = Ok(ResolvedFieldValue::new(field.to_string(), x));
            } else if dt == SkipprDataType::Map {
                if value.is_object() {
                    let mut m = Map::new();

                    for (key, val) in value.as_object().unwrap().iter() {
                        // if field == "trip" {
                        //     println!("{:?}", metadata.get_mut(field));
                        // }

                        if metadata.get(field).unwrap().fields.get(key).is_none() {
                            discover_ingest(
                                config,
                                key,
                                val,
                                Some(field),
                                Some(data_type),
                                &mut metadata.get_mut(field).unwrap().fields,
                                updated_schema,
                            );
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
                                config,
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
                                Err(e) => {
                                    let evolved = evolve_or_err(
                                        config,
                                        key,
                                        val,
                                        Some(field),
                                        Some(data_type),
                                        &mut metadata.get_mut(field).unwrap().fields,
                                        updated_schema,
                                        allow_evolve,
                                        flatten,
                                        e,
                                    )?;
                                    m.insert(evolved.field, evolved.value);
                                }
                            }
                        }
                    }

                    new_value = Ok(ResolvedFieldValue::new(field.to_string(), m.into()));
                } else {
                    new_value = evolve_own_shape(
                        config,
                        field,
                        value,
                        parent_field,
                        parent_data_type,
                        metadata,
                        updated_schema,
                        allow_evolve,
                        flatten,
                    );
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
            } else if dt == SkipprDataType::Array {
                if !value.is_array() {
                    new_value = evolve_own_shape(
                        config,
                        field,
                        value,
                        parent_field,
                        parent_data_type,
                        metadata,
                        updated_schema,
                        allow_evolve,
                        flatten,
                    );
                } else if metadata.get(field).unwrap().determined_type_values
                    == Some(SkipprDataType::Record)
                {
                    let mut arr_new_value: Vec<Value> = Vec::new();
                    const TEMPLATE: &str = "0";

                    for (i, sub_value) in value.as_array().unwrap().iter().enumerate() {
                        if metadata
                            .get(&field.to_string())
                            .unwrap()
                            .fields
                            .get(TEMPLATE)
                            .is_none()
                        {
                            discover_ingest(
                                config,
                                TEMPLATE,
                                sub_value,
                                Some(field),
                                Some(data_type),
                                &mut metadata.get_mut(&field.to_string()).unwrap().fields,
                                updated_schema,
                            );
                        }
                        if i > 0 {
                            let slot = i.to_string();
                            let missing = metadata
                                .get(&field.to_string())
                                .unwrap()
                                .fields
                                .get(&slot)
                                .is_none();
                            if missing {
                                if let Some(template) = metadata
                                    .get(field)
                                    .and_then(|m| m.fields.get(TEMPLATE))
                                    .cloned()
                                {
                                    let mut copy = template;
                                    copy.out_field_name = slot.clone();
                                    metadata.get_mut(field).unwrap().fields.insert(slot, copy);
                                }
                            }
                        }

                        let parent_type = match flatten {
                            true => Some("record"),
                            false => Some("array"),
                        };

                        let foo = set_value(
                            config,
                            SkipprDataType::Record.as_str(),
                            TEMPLATE,
                            sub_value,
                            Some(field),
                            parent_type,
                            &mut metadata.get_mut(field).unwrap().fields,
                            updated_schema,
                            false,
                            flatten,
                        );

                        let val = match foo {
                            Ok(v) => v,
                            Err(e) => {
                                return Err(crate::ingest::deny::wrap_array_mixed(e, field));
                            }
                        };
                        arr_new_value.insert(i, val.value);
                    }

                    if let Some(meta) = metadata.get_mut(field) {
                        let n = arr_new_value.len() as i32;
                        if n > meta.repetition_count {
                            meta.repetition_count = n;
                            *updated_schema = "yes".to_string();
                        }
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

                    // For nested arrays (array-of-arrays) the parent metadata
                    // describes the outer array; the inner element metadata
                    // lives under parent.fields["0"].  Clone it once so we can
                    // recurse with the correct type context.
                    let sub_dt = metadata
                        .get(&field.to_string())
                        .and_then(|m| m.determined_type_values.clone());
                    let mut inner_fields = if sub_dt == Some(SkipprDataType::Array) {
                        let flds = &metadata.get(&field.to_string()).unwrap().fields;
                        if !flds.is_empty() {
                            Some(flds.as_ref().clone())
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    match value.as_array() {
                        Some(t) => {
                            for (i, sub_value) in t.iter().enumerate() {
                                if let Some(ref mut inner) = inner_fields {
                                    let foo = set_value(
                                        config,
                                        SkipprDataType::Array.as_str(),
                                        "0",
                                        sub_value,
                                        Some(field),
                                        Some("array"),
                                        inner,
                                        updated_schema,
                                        false,
                                        flatten,
                                    );
                                    let res = match foo {
                                        Ok(v) => v,
                                        Err(e) => {
                                            return Err(crate::ingest::deny::wrap_array_mixed(
                                                e, field,
                                            ))
                                        }
                                    };
                                    arr_new_value.insert(i, res.value);
                                } else {
                                    match metadata.get(&field.to_string()) {
                                        Some(_t) => (),
                                        None => {
                                            // println!("({}.array) no metadata for {} => {} with value: {}", data_type, &field.to_string(), i.to_string(), sub_value);
                                            // discover_ingest(&field.to_string(), value, metadata, updatedSchema, flatten);
                                            discover_ingest(
                                                config,
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
                                        .and_then(|m| m.determined_type_values.as_ref())
                                        .map(|t| t.as_str())
                                        .unwrap_or("");
                                    let foo = set_value(
                                        config,
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
                                        Err(e) => {
                                            return Err(crate::ingest::deny::wrap_array_mixed(
                                                e, field,
                                            ))
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

                if dt == SkipprDataType::Date {
                    new_value = match set_date(
                        field,
                        value,
                        parent_field,
                        parent_data_type,
                        metadata,
                        updated_schema,
                        flatten,
                    ) {
                        Ok(v) => Ok(v),
                        Err(e) => evolve_or_err(
                            config,
                            field,
                            value,
                            parent_field,
                            parent_data_type,
                            metadata,
                            updated_schema,
                            allow_evolve,
                            flatten,
                            e,
                        ),
                    };
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
                        config,
                        field,
                        &dt,
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
            config,
            &field.to_string(),
            value,
            parent_field,
            parent_data_type,
            metadata,
            updated_schema,
        );

        if discoverd_data_type != "" {
            return set_value(
                config,
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

fn type_split_forbidden(metadata: &HashMap<String, Metadata>, field: &str) -> bool {
    metadata.get(field).is_some_and(|m| {
        matches!(
            m.determined_type,
            SkipprDataType::Record | SkipprDataType::Map | SkipprDataType::Array
        )
    })
}

fn evolve_own_shape(
    config: &Config,
    field: &str,
    value: &Value,
    parent_field: Option<&str>,
    parent_data_type: Option<&str>,
    metadata: &mut HashMap<String, Metadata>,
    updated_schema: &mut String,
    allow_evolve: bool,
    flatten: bool,
) -> Result<ResolvedFieldValue, Box<dyn std::error::Error>> {
    if !allow_evolve {
        return Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "Field: {} => {} value: {:?} is not a {}",
                parent_field.unwrap_or("-"),
                field,
                value,
                metadata
                    .get(field)
                    .map(|m| m.determined_type.as_str())
                    .unwrap_or("unknown")
            ),
        )));
    }
    Evolution::evolve_field(
        config,
        &field.to_string(),
        value,
        parent_field,
        parent_data_type,
        metadata,
        updated_schema,
        flatten,
    )
}

fn evolve_or_err(
    config: &Config,
    field: &str,
    value: &Value,
    parent_field: Option<&str>,
    parent_data_type: Option<&str>,
    metadata: &mut HashMap<String, Metadata>,
    updated_schema: &mut String,
    allow_evolve: bool,
    flatten: bool,
    err: Box<dyn std::error::Error>,
) -> Result<ResolvedFieldValue, Box<dyn std::error::Error>> {
    if !allow_evolve || type_split_forbidden(metadata, field) {
        return Err(err);
    }
    Evolution::evolve_field(
        config,
        &field.to_string(),
        value,
        parent_field,
        parent_data_type,
        metadata,
        updated_schema,
        flatten,
    )
}

fn match_scalar_value(
    config: &Config,
    field: &str,
    data_type: &SkipprDataType,
    value: &Value,
    parent_field: Option<&str>,
    parent_data_type: Option<&str>,
    metadata: &mut HashMap<String, Metadata>,
    updated_schema: &mut String,
    allow_evolve: bool,
    flatten: bool,
) -> Result<ResolvedFieldValue, Box<dyn std::error::Error>> {
    if value.is_null() {
        return Ok(ResolvedFieldValue::new(
            Metadata::get_field_out_field_name(metadata, field),
            Value::Null,
        ));
    }

    let output_field_name = Metadata::get_field_out_field_name(metadata, field);

    match coerce_scalar(data_type, value) {
        Ok(v) => Ok(ResolvedFieldValue::new(output_field_name, v)),
        Err(_) => evolve_or_err(
            config,
            field,
            value,
            parent_field,
            parent_data_type,
            metadata,
            updated_schema,
            allow_evolve,
            flatten,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                scalar_mismatch(data_type, parent_field, field, value),
            )),
        ),
    }
}

pub fn discover_ingest(
    config: &Config,
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
        _foo.analyse_field(
            config,
            &field.to_string(),
            value.clone().borrow_mut(),
            metadata,
        );

        // println!("Determining field types for field: {} with type: {} and Parent: {}", field, metadata.get(field).unwrap().determined_type, parent_field.unwrap_or_default());

        // if (parent_field.is_some()) {
        //     AnalyseSchema::determine_field_types(metadata.get_mut(field).unwrap().fields.as_mut(), parent_data_type, parent_field, flatten);
        // } else {
    }

    let flatten = config.get_transform_flatten_events();

    AnalyseSchema::determine_field_types(config, metadata, parent_data_type, flatten);

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
            let fmt = match metadata.get(field).and_then(|m| m.date_candidate.as_ref()) {
                Some(c) => c.format.clone(),
                None => {
                    return Err(Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("No date format for field {}", field),
                    )));
                }
            };
            match DateFormats::from_str(&fmt) {
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
                        None => Err(Box::new(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!(
                                "Could not parse date {} with format {} for field {}",
                                val, fmt, field
                            ),
                        ))),
                    }
                }
                Err(err) => Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("Invalid date format for field {}: {}", field, err),
                ))),
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
mod tests_nested_ingest_evolution {
    use super::*;
    use serde_json::json;

    fn root_with_record(
        field: &str,
        children: HashMap<String, Metadata>,
    ) -> HashMap<String, Metadata> {
        let mut metadata = HashMap::new();
        let mut record = Metadata::new().unwrap();
        record.determined_type = SkipprDataType::Record;
        record.out_field_name = field.to_string();
        record.fields = Box::new(children);
        metadata.insert(field.to_string(), record);
        metadata
    }

    fn root_with_map(
        field: &str,
        children: HashMap<String, Metadata>,
    ) -> HashMap<String, Metadata> {
        let mut metadata = HashMap::new();
        let mut map = Metadata::new().unwrap();
        map.determined_type = SkipprDataType::Map;
        map.out_field_name = field.to_string();
        map.fields = Box::new(children);
        metadata.insert(field.to_string(), map);
        metadata
    }

    fn scalar(field: &str, data_type: SkipprDataType) -> Metadata {
        let mut metadata = Metadata::new().unwrap();
        metadata.determined_type = data_type;
        metadata.out_field_name = field.to_string();
        metadata
    }

    #[test]
    fn record_ingest_discovers_missing_nested_child() {
        let mut children = HashMap::new();
        children.insert("name".to_string(), scalar("name", SkipprDataType::String));
        let mut metadata = root_with_record("profile", children);
        let mut updated_schema = "no".to_string();

        let resolved = set_value(
            &Config::new(),
            "record",
            "profile",
            &json!({"name": "Ada", "active": true}),
            None,
            None,
            &mut metadata,
            &mut updated_schema,
            true,
            false,
        )
        .expect("record should normalize and discover nested child");

        assert_eq!(updated_schema, "yes");
        let profile = metadata.get("profile").unwrap();
        assert_eq!(
            profile.fields.get("active").unwrap().determined_type,
            SkipprDataType::Boolean
        );
        assert_eq!(resolved.value["active"], true);
    }

    #[test]
    fn map_ingest_discovers_missing_nested_child_under_map_metadata() {
        let mut children = HashMap::new();
        children.insert(
            "existing".to_string(),
            scalar("existing", SkipprDataType::String),
        );
        let mut metadata = root_with_map("attrs", children);
        let mut updated_schema = "no".to_string();

        let resolved = set_value(
            &Config::new(),
            "map",
            "attrs",
            &json!({"existing": "kept", "flag": true}),
            None,
            None,
            &mut metadata,
            &mut updated_schema,
            true,
            false,
        )
        .expect("map should normalize and discover nested child");

        assert_eq!(updated_schema, "yes");
        assert!(
            metadata.get("flag").is_none(),
            "map children must not be discovered at the root"
        );
        let attrs = metadata.get("attrs").unwrap();
        assert_eq!(
            attrs.fields.get("flag").unwrap().determined_type,
            SkipprDataType::Boolean
        );
        assert_eq!(resolved.value["flag"], true);
    }

    #[test]
    fn record_ingest_evolves_nested_child_sibling_under_parent_metadata() {
        let mut children = HashMap::new();
        children.insert("age".to_string(), scalar("age", SkipprDataType::Integer));
        let mut metadata = root_with_record("profile", children);
        let mut updated_schema = "no".to_string();

        let resolved = set_value(
            &Config::new(),
            "record",
            "profile",
            &json!({"age": "senior"}),
            None,
            None,
            &mut metadata,
            &mut updated_schema,
            true,
            false,
        )
        .expect("nested child should evolve inside parent metadata");

        assert_eq!(updated_schema, "yes");
        assert!(
            metadata.get("age_string").is_none(),
            "nested evolution must not create root-level siblings"
        );
        let profile = metadata.get("profile").unwrap();
        assert!(profile.fields.get("age_string").is_some());
        assert_eq!(resolved.value["age_string"], "senior");
    }

    #[test]
    fn record_field_wrong_shape_evolves_own_string_sibling() {
        let mut metadata = root_with_record("detail", HashMap::new());
        let mut updated_schema = "no".to_string();
        let resolved = set_value(
            &Config::new(),
            "record",
            "detail",
            &json!("not-an-object"),
            None,
            None,
            &mut metadata,
            &mut updated_schema,
            true,
            false,
        )
        .expect("record vs string is an own-shape type-split");
        assert!(
            metadata.get("detail_string").is_some(),
            "expected detail_string, keys={:?}",
            metadata.keys().collect::<Vec<_>>()
        );
        assert!(metadata.get("detail_record").is_none());
        assert_eq!(resolved.field, "detail_string");
        assert_eq!(resolved.value, json!("not-an-object"));
    }

    #[test]
    fn record_ingest_evolves_nested_decimal_leaf_to_string_sibling() {
        let mut children = HashMap::new();
        children.insert(
            "docket_aggregate_size_code".to_string(),
            scalar("docket_aggregate_size_code", SkipprDataType::Decimal),
        );
        let mut metadata = root_with_record("detail", children);
        let mut updated_schema = "no".to_string();

        let resolved = set_value(
            &Config::new(),
            "record",
            "detail",
            &json!({"docket_aggregate_size_code": "20mm"}),
            None,
            None,
            &mut metadata,
            &mut updated_schema,
            true,
            true,
        )
        .expect("nested decimal leaf should type-split to a string sibling");

        let err = resolved.value.to_string();
        assert!(
            !err.contains("No schema evolutions succeeded for field: 'detail'"),
            "leaf coerce must not evolve parent detail: {err}"
        );
        let detail = metadata.get("detail").unwrap();
        assert!(
            detail
                .fields
                .get("docket_aggregate_size_code_string")
                .is_some(),
            "expected leaf sibling, fields={:?}",
            detail.fields.keys().collect::<Vec<_>>()
        );
        assert_eq!(resolved.value["docket_aggregate_size_code_string"], "20mm");
    }

    #[test]
    fn map_ingest_evolves_nested_decimal_leaf_to_string_sibling() {
        let mut children = HashMap::new();
        children.insert(
            "docket_aggregate_size_code".to_string(),
            scalar("docket_aggregate_size_code", SkipprDataType::Decimal),
        );
        let mut metadata = root_with_map("attrs", children);
        let mut updated_schema = "no".to_string();

        let resolved = set_value(
            &Config::new(),
            "map",
            "attrs",
            &json!({"docket_aggregate_size_code": "20mm"}),
            None,
            None,
            &mut metadata,
            &mut updated_schema,
            true,
            false,
        )
        .expect("nested map decimal leaf should type-split");

        assert!(metadata.get("attrs_record").is_none());
        let attrs = metadata.get("attrs").unwrap();
        assert!(attrs
            .fields
            .get("docket_aggregate_size_code_string")
            .is_some());
        assert_eq!(resolved.value["docket_aggregate_size_code_string"], "20mm");
    }

    #[test]
    fn record_ingest_evolves_l2_decimal_leaf_to_string_sibling() {
        let mut inner = HashMap::new();
        inner.insert("leaf".to_string(), scalar("leaf", SkipprDataType::Decimal));
        let mut nested = Metadata::new().unwrap();
        nested.determined_type = SkipprDataType::Record;
        nested.out_field_name = "nested".to_string();
        nested.fields = Box::new(inner);
        let mut children = HashMap::new();
        children.insert("nested".to_string(), nested);
        let mut metadata = root_with_record("detail", children);
        let mut updated_schema = "no".to_string();

        let resolved = set_value(
            &Config::new(),
            "record",
            "detail",
            &json!({"nested": {"leaf": "20mm"}}),
            None,
            None,
            &mut metadata,
            &mut updated_schema,
            true,
            false,
        )
        .expect("L2 nested decimal leaf should type-split");

        let detail = metadata.get("detail").unwrap();
        assert!(
            detail.fields.get("nested_record").is_none(),
            "must not evolve L1 nested record, fields={:?}",
            detail.fields.keys().collect::<Vec<_>>()
        );
        assert!(metadata.get("detail_record").is_none());
        let nested = detail.fields.get("nested").unwrap();
        assert!(
            nested.fields.get("leaf_string").is_some(),
            "expected L2 leaf sibling, fields={:?}",
            nested.fields.keys().collect::<Vec<_>>()
        );
        assert_eq!(resolved.value["nested"]["leaf_string"], "20mm");
    }

    fn assert_array_mixed(err: Box<dyn std::error::Error>) {
        let deny = err
            .downcast_ref::<crate::ingest::deny::IngestDenyError>()
            .unwrap_or_else(|| panic!("expected ARRAY_MIXED_TYPES, got {err}"));
        assert_eq!(deny.code, crate::ingest::deny::ARRAY_MIXED_TYPES);
        assert!(
            deny.message
                .contains("Mixed types in arrays are not type-split"),
            "hint missing: {}",
            deny.message
        );
    }

    #[test]
    fn nested_primitive_array_denies_mixed_scalars() {
        let mut arr = Metadata::new().unwrap();
        arr.determined_type = SkipprDataType::Array;
        arr.determined_type_values = Some(SkipprDataType::Integer);
        arr.out_field_name = "codes".to_string();
        arr.enabled = true;
        let mut children = HashMap::new();
        children.insert("codes".to_string(), arr);
        let mut metadata = root_with_record("detail", children);
        let mut updated_schema = "no".to_string();

        let err = set_value(
            &Config::new(),
            "record",
            "detail",
            &json!({"codes": [1234, "BWL"]}),
            None,
            None,
            &mut metadata,
            &mut updated_schema,
            true,
            false,
        )
        .expect_err("nested primitive array mixed scalars must deny");
        assert_array_mixed(err);
        let detail = metadata.get("detail").unwrap();
        assert!(
            metadata.get("detail_record").is_none()
                && detail.fields.get("codes_0_string").is_none()
                && !detail.fields.keys().any(|k| k.contains("codes_array")),
            "must not parent-split or type-split indexes, fields={:?}",
            detail.fields.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn leaf_type_split_matrix_scalars_under_record() {
        let scalars = [
            SkipprDataType::Integer,
            SkipprDataType::Long,
            SkipprDataType::Short,
            SkipprDataType::Byte,
            SkipprDataType::Double,
            SkipprDataType::Float,
            SkipprDataType::Decimal,
            SkipprDataType::Boolean,
            SkipprDataType::Timestamp,
            SkipprDataType::TimestampMilli,
            SkipprDataType::Date,
            SkipprDataType::Time,
            SkipprDataType::Uuid,
            SkipprDataType::Json,
            SkipprDataType::Null,
        ];
        for dt in scalars {
            let mut children = HashMap::new();
            children.insert("leaf".to_string(), scalar("leaf", dt.clone()));
            let mut metadata = root_with_record("detail", children);
            let mut updated_schema = "no".to_string();
            let resolved = set_value(
                &Config::new(),
                "record",
                "detail",
                &json!({"leaf": "20mm"}),
                None,
                None,
                &mut metadata,
                &mut updated_schema,
                true,
                false,
            )
            .unwrap_or_else(|err| panic!("{dt:?} nested leaf must type-split, got {err}"));
            let detail = metadata.get("detail").unwrap();
            assert!(
                detail.fields.get("leaf_string").is_some(),
                "{dt:?}: expected leaf_string, fields={:?}",
                detail.fields.keys().collect::<Vec<_>>()
            );
            assert!(
                metadata.get("detail_record").is_none(),
                "{dt:?}: must not evolve parent record"
            );
            assert_eq!(resolved.value["leaf_string"], "20mm");
        }
    }

    #[test]
    fn array_of_records_denies_object_element_leaf_type_split() {
        let leaf = scalar("max_size_mm", SkipprDataType::Decimal);
        let mut elem = Metadata::new().unwrap();
        elem.determined_type = SkipprDataType::Record;
        elem.out_field_name = "0".to_string();
        elem.fields.insert("max_size_mm".to_string(), leaf);
        let mut arr = Metadata::new().unwrap();
        arr.determined_type = SkipprDataType::Array;
        arr.determined_type_values = Some(SkipprDataType::Record);
        arr.out_field_name = "aggregate".to_string();
        arr.fields.insert("0".to_string(), elem);
        let mut metadata = HashMap::new();
        metadata.insert("aggregate".to_string(), arr);
        let mut updated_schema = "no".to_string();
        let err = set_value(
            &Config::new(),
            "array",
            "aggregate",
            &json!([{"max_size_mm": "20mm"}]),
            None,
            None,
            &mut metadata,
            &mut updated_schema,
            true,
            false,
        )
        .expect_err("array-of-records leaf must deny type-split");
        assert_array_mixed(err);
        let elem = metadata.get("aggregate").unwrap().fields.get("0").unwrap();
        assert!(
            elem.fields.get("max_size_mm_string").is_none(),
            "must not CREATE element leaf sibling, fields={:?}",
            elem.fields.keys().collect::<Vec<_>>()
        );
        assert!(metadata.get("aggregate_record").is_none());
    }

    #[test]
    fn primitive_array_mixed_types_denied_not_parent_split() {
        let mut arr = Metadata::new().unwrap();
        arr.determined_type = SkipprDataType::Array;
        arr.determined_type_values = Some(SkipprDataType::Integer);
        arr.out_field_name = "codes".to_string();
        arr.enabled = true;
        let mut metadata = HashMap::new();
        metadata.insert("codes".to_string(), arr);
        let mut updated_schema = "no".to_string();
        let err = ingest(
            &Config::new(),
            &json!({"codes": [1234, "BWL"]}),
            &mut metadata,
            "default",
            &mut updated_schema,
            false,
        )
        .expect_err("primitive mixed array must deny");
        assert_array_mixed(err);
        assert!(
            metadata.get("codes_0_string").is_none()
                && !metadata.keys().any(|k| k.contains("codes_array")),
            "must not type-split indexes or parent-split, keys={:?}",
            metadata.keys().collect::<Vec<_>>(),
        );
    }

    #[test]
    fn primitive_array_fractional_float_denied() {
        let mut arr = Metadata::new().unwrap();
        arr.determined_type = SkipprDataType::Array;
        arr.determined_type_values = Some(SkipprDataType::Integer);
        arr.out_field_name = "codes".to_string();
        arr.enabled = true;
        let mut metadata = HashMap::new();
        metadata.insert("codes".to_string(), arr);
        let mut updated_schema = "no".to_string();
        let err = ingest(
            &Config::new(),
            &json!({"codes": [16, 1.5]}),
            &mut metadata,
            "default",
            &mut updated_schema,
            false,
        )
        .expect_err("JSON 1.5 in an integer array must deny");
        assert_array_mixed(err);
        assert!(!metadata.keys().any(|k| k.contains("codes_array")));
    }

    fn speed_pattern_stages_metadata() -> HashMap<String, Metadata> {
        let leaf = scalar("rpm_min", SkipprDataType::Integer);
        let mut elem = Metadata::new().unwrap();
        elem.determined_type = SkipprDataType::Record;
        elem.out_field_name = "0".to_string();
        elem.enabled = true;
        elem.fields.insert("rpm_min".to_string(), leaf);
        let mut arr = Metadata::new().unwrap();
        arr.determined_type = SkipprDataType::Array;
        arr.determined_type_values = Some(SkipprDataType::Record);
        arr.out_field_name = "speed_pattern_stages".to_string();
        arr.enabled = true;
        arr.repetition_count = 1;
        arr.fields.insert("0".to_string(), elem);
        let mut children = HashMap::new();
        children.insert("speed_pattern_stages".to_string(), arr);
        root_with_record("detail", children)
    }

    #[test]
    fn flattened_array_index_mixed_leaf_denied() {
        let mut metadata = speed_pattern_stages_metadata();
        let mut updated_schema = "no".to_string();
        let err = ingest(
            &Config::new(),
            &json!({"detail":{"speed_pattern_stages":[{"rpm_min":8},{"rpm_min":1.5}]}}),
            &mut metadata,
            "default",
            &mut updated_schema,
            true,
        )
        .expect_err("flatten=true mixed rpm_min must deny");
        assert_array_mixed(err);
        let arr = metadata
            .get("detail")
            .unwrap()
            .fields
            .get("speed_pattern_stages")
            .unwrap();
        assert_eq!(
            arr.fields
                .get("0")
                .unwrap()
                .fields
                .get("rpm_min")
                .unwrap()
                .determined_type,
            SkipprDataType::Integer
        );
        if let Some(idx1) = arr.fields.get("1") {
            assert_eq!(
                idx1.fields.get("rpm_min").unwrap().determined_type,
                SkipprDataType::Integer,
                "index 1 must stay a clone of template 0, not decimal"
            );
        }
        assert!(arr
            .fields
            .get("0")
            .unwrap()
            .fields
            .get("rpm_min_decimal")
            .is_none());
    }

    #[test]
    fn array_of_records_mixed_leaf_denied_flatten_false() {
        let mut metadata = speed_pattern_stages_metadata();
        let mut updated_schema = "no".to_string();
        let err = ingest(
            &Config::new(),
            &json!({"detail":{"speed_pattern_stages":[{"rpm_min":8},{"rpm_min":1.5}]}}),
            &mut metadata,
            "default",
            &mut updated_schema,
            false,
        )
        .expect_err("flatten=false mixed rpm_min must deny");
        assert_array_mixed(err);
        assert!(metadata
            .get("detail")
            .unwrap()
            .fields
            .get("speed_pattern_stages")
            .unwrap()
            .fields
            .get("0")
            .unwrap()
            .fields
            .get("rpm_min_decimal")
            .is_none());
    }

    #[test]
    fn flattened_array_whole_float_later_slot_kept() {
        let mut metadata = speed_pattern_stages_metadata();
        let mut updated_schema = "no".to_string();
        ingest(
            &Config::new(),
            &json!({"detail":{"speed_pattern_stages":[{"rpm_min":8},{"rpm_min":16.0}]}}),
            &mut metadata,
            "default",
            &mut updated_schema,
            true,
        )
        .expect("JSON 16.0 still coerces to integer");
    }

    #[test]
    fn flattened_array_new_key_on_later_element_discovers_on_template() {
        let mut metadata = speed_pattern_stages_metadata();
        let mut updated_schema = "no".to_string();
        ingest(
            &Config::new(),
            &json!({"detail":{"speed_pattern_stages":[{"rpm_min":8},{"rpm_min":8,"status":"ok"}]}}),
            &mut metadata,
            "default",
            &mut updated_schema,
            true,
        )
        .expect("new object keys on later elements still discover");
        let template = &metadata
            .get("detail")
            .unwrap()
            .fields
            .get("speed_pattern_stages")
            .unwrap()
            .fields
            .get("0")
            .unwrap()
            .fields;
        assert!(
            template.get("status").is_some(),
            "status must land on template 0, keys={:?}",
            template.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn denied_mixed_row_does_not_poison_later_compatible_row() {
        let mut metadata = speed_pattern_stages_metadata();
        let mut updated_schema = "no".to_string();
        let err = ingest(
            &Config::new(),
            &json!({"detail":{"speed_pattern_stages":[{"rpm_min":8},{"rpm_min":1.5}]}}),
            &mut metadata,
            "default",
            &mut updated_schema,
            true,
        )
        .expect_err("mixed row denied");
        assert_array_mixed(err);
        ingest(
            &Config::new(),
            &json!({"detail":{"speed_pattern_stages":[{"rpm_min":8},{"rpm_min":16}]}}),
            &mut metadata,
            "default",
            &mut updated_schema,
            true,
        )
        .expect("compatible row after deny must ingest");
    }

    #[test]
    fn flatten_metadata_after_denied_mixed_row_stays_integer() {
        let mut metadata = speed_pattern_stages_metadata();
        let mut updated_schema = "no".to_string();
        let _ = ingest(
            &Config::new(),
            &json!({"detail":{"speed_pattern_stages":[{"rpm_min":8},{"rpm_min":1.5}]}}),
            &mut metadata,
            "default",
            &mut updated_schema,
            true,
        );
        let mut root = Metadata::new().unwrap();
        root.fields = Box::new(metadata);
        let mut flattened = crate::discover::OutputMetadata::new();
        Metadata::flatten_metadata(&root, &mut flattened);
        for (name, col) in flattened.fields.iter() {
            if name.contains("rpm_min") {
                assert_eq!(
                    col.determined_type,
                    SkipprDataType::Integer,
                    "{name} must stay integer, got {:?}",
                    col.determined_type
                );
            }
        }
    }

    fn try_serialize(
        schema: std::sync::Arc<arrow::datatypes::Schema>,
        value: &Value,
    ) -> Result<(), String> {
        let mut decoder = arrow::json::ReaderBuilder::new(schema)
            .build_decoder()
            .map_err(|e| format!("build_decoder: {e}"))?;
        decoder
            .serialize(&[value])
            .map_err(|e| format!("serialize: {e}"))?;
        match decoder.flush() {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err("flush returned no batch".into()),
            Err(e) => Err(format!("flush: {e}")),
        }
    }

    #[test]
    fn mixed_array_row_denied_before_arrow_serialize() {
        use arrow::datatypes::{DataType, Field, Schema};
        use std::sync::Arc;

        let mut metadata = speed_pattern_stages_metadata();
        let mut updated_schema = "no".to_string();
        let live = ingest(
            &Config::new(),
            &json!({"detail":{"speed_pattern_stages":[{"rpm_min":8},{"rpm_min":16}]}}),
            &mut metadata,
            "default",
            &mut updated_schema,
            true,
        )
        .expect("all-integer stages must live");
        let schema = Arc::new(Schema::new(vec![
            Field::new(
                "detail_speed_pattern_stages_0_rpm_min",
                DataType::Int32,
                true,
            ),
            Field::new(
                "detail_speed_pattern_stages_1_rpm_min",
                DataType::Int32,
                true,
            ),
        ]));
        try_serialize(schema.clone(), &live).expect("live integer row must serialize");

        let err = ingest(
            &Config::new(),
            &json!({"detail":{"speed_pattern_stages":[{"rpm_min":8},{"rpm_min":1.5}]}}),
            &mut metadata,
            "default",
            &mut updated_schema,
            true,
        )
        .expect_err("1.5 must deny before Arrow");
        assert_array_mixed(err);

        let again = ingest(
            &Config::new(),
            &json!({"detail":{"speed_pattern_stages":[{"rpm_min":8},{"rpm_min":4}]}}),
            &mut metadata,
            "default",
            &mut updated_schema,
            true,
        )
        .expect("later integer row still lives");
        try_serialize(schema, &again).expect("later integer row must serialize");
    }

    #[test]
    fn docket_leaf_split_still_serializes() {
        use arrow::datatypes::{DataType, Field, Schema};
        use std::sync::Arc;

        let mut children = HashMap::new();
        children.insert(
            "docket_aggregate_size_code".to_string(),
            scalar("docket_aggregate_size_code", SkipprDataType::Decimal),
        );
        children.insert(
            "docket_plant_code".to_string(),
            scalar("docket_plant_code", SkipprDataType::Integer),
        );
        let mut metadata = root_with_record("detail", children);
        let mut updated_schema = "no".to_string();
        let live = ingest(
            &Config::new(),
            &json!({"detail":{"docket_aggregate_size_code":"20mm","docket_plant_code":"I312"}}),
            &mut metadata,
            "default",
            &mut updated_schema,
            true,
        )
        .expect("docket 20mm / I312 must stay live");
        let obj = live.as_object().expect("flattened object");
        assert!(
            obj.keys()
                .any(|k| k.ends_with("docket_aggregate_size_code_string")),
            "expected size-code string sibling, keys={:?}",
            obj.keys().collect::<Vec<_>>()
        );
        assert!(
            obj.keys().any(|k| k.ends_with("docket_plant_code_string")),
            "expected plant-code string sibling, keys={:?}",
            obj.keys().collect::<Vec<_>>()
        );
        let schema = Arc::new(Schema::new(vec![
            Field::new(
                "detail_docket_aggregate_size_code_string",
                DataType::Utf8,
                true,
            ),
            Field::new("detail_docket_plant_code_string", DataType::Utf8, true),
        ]));
        try_serialize(schema, &live).expect("leaf-split siblings must serialize");
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
                ..Metadata::new().unwrap()
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

    #[test]
    fn set_date_unparseable_string_is_err_not_null() {
        let field = "test_field";
        let format_name = AnalyseSchema::is_valid_date("2023-05-21 12:34:56").unwrap();
        let mut meta = generate_metadata(field, format_name);
        let mut updated_schema = "no".to_string();
        let result = set_date(
            field,
            &serde_json::json!("20mm"),
            None,
            None,
            &mut meta,
            &mut updated_schema,
            false,
        );
        assert!(
            result.is_err(),
            "unparseable date must Err so evolve_or_err can CREATE, got {result:?}"
        );
        let resolved = set_value(
            &Config::new(),
            SkipprDataType::Date.as_str(),
            field,
            &serde_json::json!("20mm"),
            None,
            None,
            &mut meta,
            &mut updated_schema,
            true,
            false,
        )
        .expect("date S_non must type-split");
        assert!(meta.get("test_field_string").is_some());
        assert_eq!(resolved.value, serde_json::json!("20mm"));
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
    use crate::helpers::configuration::Config;

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

        AnalyseSchema::infer_json_schema(
            &mut foo,
            &Config::new(),
            &mut record_line,
            Some(1),
            &mut metadata,
            Some("default"),
        );

        let str = r#"{"rider_id":"10e974bf-4a43-305a-9e39-1636c43cb22a","bike_id":"8b86f753-05f8-3254-aba6-739188a3c0b6","isbn":"9407496597","trip":{"start_temprature":0,"end_temprature":2},"last_crank":[2,15,33,45,56,57,47,36,19,5],"crank_torques":[[2,15,33,45,56,57,47,36,19,5],[1,13,33,48,56,58,45,35,15,6]],"hardware":{"manufacturer":"Beier, Emmerich and Rutherford","model":"synergize ubiquitous e-commerce","maintenance":{"last_rebuild":"20\/04\/2010","last_service":"12\/07\/1973"}},"metadata":{"rcvd_time":1615474895,"sent_time":1615474930,"prcd_micro_time":1615474853.999185,"tags":[{"name":"type","value":"trip"},{"name":"auto","value":false}]}}"#;

        let records: Vec<Value> = SerdeJson::deserialize(&Config::new(), str);

        let mut updated_schema = "no".to_string();

        // NOTE: This schema is assert tested in discovery
        let ingest_value = ingest(
            &Config::new(),
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

    #[test]
    #[serial]
    fn cloudcycle_docket_nested_type_split_keeps_row() {
        let disc = concat!(
            r#"{"version":"0","id":"disc-1","detail-type":"cloudcycle-docket","source":"com.cloudcycle.concrete-control-centre.cloudcycle-docket","account":"201015722121","time":"2024-10-10T10:00:00Z","region":"eu-west-1","resources":[],"detail":{"docket_rmc":"Cemex","docket_id":"Cemex--1","docket_aggregate_size_code":"20.00","docket_plant_code":"1234","docket_order_code":"1025594637","docket_datetime_batch_utc":"2024-10-10T10:00:00.000Z"}}"#
        );
        let later = serde_json::json!({
            "version": "0",
            "id": "2cbc9030-adbe-afe2-b6be-c879f6eed5f5",
            "detail-type": "cloudcycle-docket",
            "source": "com.cloudcycle.concrete-control-centre.cloudcycle-docket",
            "account": "201015722121",
            "time": "2024-10-10T10:39:54Z",
            "region": "eu-west-1",
            "resources": [],
            "detail": {
                "docket_aggregate_size_code": "20mm",
                "docket_plant_code": "I324",
                "docket_order_code": "1025594637",
                "docket_rmc": "Cemex",
                "docket_id": "Cemex--1025603147",
                "docket_datetime_batch_utc": "2024-10-10T10:31:11.000Z"
            }
        });
        let mut metadata = HashMap::new();
        AnalyseSchema { i: 0 }.infer_json_schema(
            &Config::new(),
            &mut disc.to_string(),
            Some(1),
            &mut metadata,
            Some("cloudcycle_docket"),
        );
        let ns = metadata
            .keys()
            .find(|k| k.contains("docket") || k.as_str() == "default")
            .cloned()
            .unwrap_or_else(|| {
                metadata
                    .keys()
                    .next()
                    .cloned()
                    .expect("discovered namespace")
            });
        let fields = metadata
            .get_mut(&ns)
            .expect("namespace metadata")
            .fields
            .as_mut();
        if let Some(leaf) = fields
            .get_mut("detail")
            .and_then(|d| d.fields.get_mut("docket_aggregate_size_code"))
        {
            leaf.determined_type = crate::discover::SkipprDataType::Decimal;
        } else {
            panic!(
                "discover must produce nested detail.docket_aggregate_size_code, keys={:?}",
                fields.keys().collect::<Vec<_>>()
            );
        }
        if let Some(leaf) = fields
            .get_mut("detail")
            .and_then(|d| d.fields.get_mut("docket_plant_code"))
        {
            leaf.determined_type = crate::discover::SkipprDataType::Integer;
        } else {
            panic!(
                "discover must produce nested detail.docket_plant_code, keys={:?}",
                fields.keys().collect::<Vec<_>>()
            );
        }
        let mut updated_schema = "no".to_string();
        let ingested = ingest(
            &Config::new(),
            &later,
            fields,
            &ns,
            &mut updated_schema,
            true,
        );
        let row = ingested.unwrap_or_else(|err| {
            panic!("record 2 must stay on the live table, not deadletter: {err}")
        });
        let obj = row.as_object().expect("flattened object");
        let size_string = obj
            .keys()
            .find(|k| k.ends_with("docket_aggregate_size_code_string"))
            .cloned();
        let plant_string = obj
            .keys()
            .find(|k| k.ends_with("docket_plant_code_string"))
            .cloned();
        assert!(
            size_string.is_some(),
            "expected flattened *_string sibling for size code, keys={:?}",
            obj.keys().collect::<Vec<_>>()
        );
        assert_eq!(row[size_string.as_ref().unwrap()], "20mm");
        assert!(
            plant_string.is_some(),
            "expected flattened *_string sibling for plant code, keys={:?}",
            obj.keys().collect::<Vec<_>>()
        );
        assert_eq!(row[plant_string.as_ref().unwrap()], "I324");
        assert!(
            obj.keys().all(|k| !k.starts_with("detail_record_")),
            "leaf coerce must not emit detail_record_* keys={:?}",
            obj.keys().collect::<Vec<_>>()
        );
    }
}
