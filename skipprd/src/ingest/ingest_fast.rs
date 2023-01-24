use crate::discover::{AnalyseSchema, Metadata};
use crate::helpers::Helpers;
use crate::serdes::parquet::{Message, SerdeParquet};
use arrow::json::reader::ValueIter;
use serde_json::{Map, Value};
use std::any::Any;
use std::borrow::{Borrow, BorrowMut};
use std::collections::HashMap;
use std::io::{BufReader, Read};
use std::process::exit;
use arrow::datatypes::DataType::Duration;
use futures::executor::block_on;
use tokio::task::spawn_blocking;
use crate::discover::arrow_schema::convert_skippr_to_arrow;
use crate::helpers::configuration::Config;

#[derive(Default)]
pub struct IngestRecord {
    pub(crate) source_namespace: String,
    pub(crate) source_partition: String,
    pub(crate) skpr_event_ts: i64,
    pub(crate) skpr_namespace: String,
    pub(crate) skpr_partition: String,
    pub(crate) record: Value,
}

pub fn fast_path_ingest_buf<R: Read>(reader: &mut BufReader<R>) -> ValueIter<R> {
    ValueIter::new(reader, None)
}

// pub fn fast_path_ingest(unwrapped_message: &mut IngestRecord, metadata: &mut HashMap<String, Metadata>) -> HashMap<String, Message<Value>> {
// pub fn fast_path_ingest(unwrapped_message: &mut IngestRecord, metadata: &mut HashMap<String, Metadata>) {
pub fn fast_path_ingest(
    unwrapped_message: &Value,
    metadata: &mut HashMap<String, Metadata>,
) -> Value {

    let mut updatedSchema: String = "no".to_string();

    // let mut helpers = Helpers { clean_field_cache: Default::default() };

    // let mut message = default_msgs[namespace].clone();
    // let mut message = SerderParquet::default_message(metadata);

    // let mut message: Vec<Value> = Vec::with_capacity(batch_size);
    let mut message: Value = Value::Null;

    for (field, value) in unwrapped_message.as_object().unwrap() {
        let field = Helpers::clean_field_name(field.to_string());

        // println!("Ingesting field: {:?}", field);

        // if special_fields.contains_key(field) {
        //     message.insert(field.to_string(), value.to_string());
        // } else {
        //         let resolved_value = Value::Null;

        let field_data_type = match metadata.get_mut(&field) {
            Some(data_type) => data_type.determined_type.clone(),
            None => "".to_string()
        };

        let resolved_value = fast_set_value(
            field_data_type,
            &field,
            value,
            metadata,
            &mut updatedSchema
        );

        // println!("Setting message with field: {:?}", resolved_value);

        // let foo = resolved_value;
        // ignore if null, use default message which has correct null for data type
        if !resolved_value.is_null() {
            message[field] = resolved_value;
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

    if updatedSchema == "yes".to_string() {

        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                Config::set_config(&metadata, true).await;
            });

        updatedSchema = "no".to_string();
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

    message
}

/**
 * @param $dataType string - the expected data type of the field value
 * @param $field string - field name
 * @param $value string -  the actual field value
 * @return mixed|null - value data type on success or null on error
 */
fn fast_set_value(
    data_type: String,
    field: &str,
    value: &Value,
    metadata: &mut HashMap<String, Metadata>,
    mut updatedSchema: &mut String
) -> Value {

    let parent_type = match metadata.get_mut(field) {
        Some(pt) => &pt.parent_type,
        None => ""
    };

    if data_type != "" || parent_type == "map" {

        // let data_type: &str = &metadata.get_mut(field).unwrap().determined_type;
        // let data_type = "record";

        let x: Value;
        let mut new_value: Value = Value::Null;

        if value.to_string().len() > 0 {
            // println!("value is {}", value);
            // println!("field is {}", field);
            // println!("data_type is {}", data_type);

            if data_type == "record" {
                // println!("{} is record", field);

                let mut m = Map::new();

                for (sub_field, sub_value) in value.as_object().unwrap() {

                    match metadata
                        .get(field)
                        .unwrap()
                        .fields
                        .get(sub_field) {

                        Some(t) => (),
                        None => {
                           discoverIngest(field, value, metadata, updatedSchema);
                        }
                    }

                    // only ingest fields enabled to sync to output
                    if metadata
                        .get_mut(field)
                        .unwrap()
                        .fields
                        .get_mut(sub_field)
                        .unwrap()
                        .enabled
                        == true
                    {
                        let clean_sub_field = Helpers::clean_field_name(sub_field.to_string());

                        let newval = fast_set_value(
                            metadata
                                .get_mut(field)
                                .unwrap()
                                .fields
                                .get_mut(sub_field)
                                .unwrap()
                                .determined_type
                                .clone(),
                            sub_field,
                            // &mut sub_value.as_str().unwrap_or(&value.to_string()), // pass string val or string representation of map/array, etc
                            sub_value,
                            &mut metadata.get_mut(field).unwrap().fields,
                            updatedSchema
                        );

                        m.insert(clean_sub_field.to_string(), newval.into());
                    }
                }

                x = m.into();
                new_value = x;
            } else {
                if data_type == "map" {
                    for (key, val) in value.as_object().unwrap() {
                        // println!("map value is {:?} key is {:?}", val, key);
                        if Some(val) != None {
                            new_value[key] = fast_set_value(
                                metadata
                                    .get_mut(field)
                                    .unwrap()
                                    .determined_type_values
                                    .clone(),
                                key,
                                val,
                                &mut metadata.get_mut(field).unwrap().fields,
                                updatedSchema
                            );
                            // println!("new_value is {:?}", new_value);
                        }
                    }
                } else {
                    if data_type == "array" {
                        for (val) in value.as_array().unwrap() {
                            // println!("map value is {:?} key is {:?}", val, key);
                            if Some(val) != None {
                                new_value = val.to_owned();
                                // new_value[key] = fast_set_value(
                                //     metadata.get_mut(field).unwrap().determined_type_values.clone(),
                                //     key,
                                //     val,
                                //     &mut metadata.get_mut(field).unwrap().fields,
                                // );
                                // println!("new_value is {:?}", new_value);
                            }
                        }

                        // println!("\n\nField: {} array value is {:?}", field, value);

                        // new_value[field] = Value::Array(Vec::new());

                        // let mut b = Vec::new();
                        //
                        // let mut i = 0;
                        //
                        // for val in value.as_array().unwrap() {
                        //     if Some(val) != None {
                        //
                        //         println!("field is {:?}", field);
                        //         println!("array val is {:?}", val);
                        //         println!("array val data_types is {:?}", metadata.get_mut(field).unwrap().determined_type_values.clone());
                        //
                        //         let new_v = fast_set_value(
                        //             metadata.get_mut(field).unwrap().determined_type_values.clone(),
                        //             &i.to_string(),
                        //             val,
                        //             &mut metadata.get_mut(field).unwrap().fields,
                        //         );
                        //
                        //         i += 1;
                        //
                        //
                        //         // println!("array new_v is {:?}\n\n\n", new_v);
                        //
                        //         b.push(new_v);
                        //
                        //     }
                        // }
                        //
                        // new_value[field] = Value::from(b);

                        // println!("new_value is {:?}", new_value[field]);
                    } else {
                        if data_type == "string" || data_type == "date" {
                            // value += "";

                            // println!("string value is {:?}", value);

                            new_value = match value.as_str() {
                                Some(val) => Value::String(val.to_string()),
                                None => Value::Null,
                            };

                            // println!("string new_value is {:?}", new_value);
                        } else if data_type == "timestamp" || data_type == "timestamp_milli" {
                            new_value = match value.as_i64() {
                                Some(val) => Value::from(val),
                                None => Value::Null,
                            }
                            // value = (int) value + 0; // force string to int
                            //            } elseif ($dataType === 'date') {
                            //                $value = (int) $value + 0; // force string to int
                        } else if data_type == "int" || data_type == "integer" {
                            // println!("number value is {:?}", value);

                            new_value = match value.as_i64() {
                                Some(val) => Value::from(val),
                                None => Value::Null,
                            };

                            // println!("number new_value is {:?}", new_value);

                            // value = (int) value + 0; // force string to int
                        } else if data_type == "long" {
                            new_value = match value.as_i64() {
                                Some(val) => Value::from(val),
                                None => Value::Null,
                            }
                            // value = (int) value + 0; // force strings to long
                        } else if data_type == "double" {
                            new_value = match value.as_f64() {
                                Some(val) => Value::from(val),
                                None => Value::Null,
                            }
                            // value = (float) sprintf("%.2f", value);
                        } else if data_type == "boolean" {
                            new_value = match value.as_bool() {
                                Some(val) => Value::from(val),
                                None => Value::Null,
                            }
                            // value = (bool) value;
                        }
                    }
                }
            }
        }
        new_value

    } else {

        let discoverd_data_type = discoverIngest(field, value, metadata, updatedSchema);

        return fast_set_value(
            discoverd_data_type.clone(),
            &field,
            value,
            metadata,
            updatedSchema
        );
    }


}

fn discoverIngest(
    field: &str,
    value: &Value,
    metadata: &mut HashMap<String, Metadata>,
    mut updatedSchema: &mut String
) -> String {
    let mut foo: AnalyseSchema = AnalyseSchema { i: 0 };

    // AnalyseSchema::resolve_field_type(&foo, metadata, &field.to_string(), value.clone().borrow_mut());
    AnalyseSchema::analyse_field(&foo, &field.to_string(), value.clone().borrow_mut(), metadata);


    AnalyseSchema::determine_field_types(metadata, None);

    // println!("{:?}", metadata.get_mut(field).unwrap());


    let discoverd_data_type = &metadata.get(field).unwrap().determined_type;

    println!("Discovered new field: '{}' of type: '{}'", field, discoverd_data_type);

    // let handle = tokio::runtime::Handle::current();
    // handle.enter();
    // block_on(Config::set_config(&metadata, true));
    // spawn_blocking(Config::set_config(&metadata, true).await);


    //@todo - if mutable mode

    // exit(0);

    *updatedSchema = "yes".to_string();

    discoverd_data_type.clone()


}