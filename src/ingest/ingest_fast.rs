use crate::discover::date_formats::DateFormats;
use crate::discover::{AnalyseSchema, Metadata};
use crate::helpers::Helpers;
use arrow::json::reader::ValueIter;
use chrono::NaiveDateTime;
use serde_json::{Map, Value};
use std::borrow::BorrowMut;
use std::collections::HashMap;
use std::io::{BufReader, Read};

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
    updatedSchema: &mut String,
) -> Value {
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
            Some(data_type) => {
                // println!("{:?}",  data_type.determined_type.clone());
                data_type.determined_type.clone()
            }
            None => "".to_string(),
        };

        let resolved_value =
            fast_set_value(&field_data_type, &field, value, metadata, updatedSchema);

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
    data_type: &str,
    field: &str,
    value: &Value,
    metadata: &mut HashMap<String, Metadata>,
    updatedSchema: &mut String,
) -> Value {
    // let _parent_type = match metadata.get_mut(field) {
    //     Some(pt) => &pt.parent_type,
    //     None => ""
    // };

    // if data_type != "" || parent_type == "map" {
    if data_type != "" {
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

                if value.is_object() {
                    for (sub_field, sub_value) in value.as_object().unwrap() {
                        let clean_sub_field = Helpers::clean_field_name(sub_field.to_string());

                        match metadata.get(field).unwrap().fields.get(sub_field) {
                            Some(_t) => (),
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
                            let newval = fast_set_value(
                                &metadata
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
                                updatedSchema,
                            );

                            m.insert(clean_sub_field.to_string(), newval.into());
                        }
                    }
                }

                // terrible duplication.
                // We determine that arrays containing arrays are record types
                // So we'll sometimes end up here
                if value.is_array() {
                    let mut i = 0;
                    for sub_value in value.as_array().unwrap() {
                        let clean_sub_field = Helpers::clean_field_name(i.to_string());

                        match metadata.get(field).unwrap().fields.get(&clean_sub_field) {
                            Some(_t) => (),
                            None => {
                                discoverIngest(field, value, metadata, updatedSchema);
                            }
                        }

                        // only ingest fields enabled to sync to output
                        if metadata
                            .get_mut(field)
                            .unwrap()
                            .fields
                            .get_mut(&clean_sub_field)
                            .unwrap()
                            .enabled
                            == true
                        {
                            let newval = fast_set_value(
                                &metadata
                                    .get_mut(field)
                                    .unwrap()
                                    .fields
                                    .get_mut(&clean_sub_field)
                                    .unwrap()
                                    .determined_type
                                    .clone(),
                                &clean_sub_field,
                                // &mut sub_value.as_str().unwrap_or(&value.to_string()), // pass string val or string representation of map/array, etc
                                sub_value,
                                &mut metadata.get_mut(field).unwrap().fields,
                                updatedSchema,
                            );

                            m.insert(clean_sub_field.to_string(), newval.into());
                        }

                        i += 1;
                    }
                }

                x = m.into();
                new_value = x;
            } else {
                if data_type == "map" {
                    let metadata_field = metadata.get_mut(field).unwrap();
                    let determined_type_values = &metadata_field.determined_type_values;
                    let fields = &mut metadata_field.fields;
                    for (key, val) in value
                        .as_object()
                        .unwrap()
                        .iter()
                        .filter_map(|(k, v)| Some((k, v)))
                    {
                        if Some(val) != None {
                            new_value[key] = fast_set_value(
                                determined_type_values,
                                &key,
                                value,
                                fields,
                                updatedSchema,
                            );
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
                } else {
                    if data_type == "array" {
                        new_value = value.to_owned();
                    } else {
                        if data_type == "date" {
                            // Hive Timestamp doesn't support string dates

                            new_value = match value.as_str() {
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
                                            match NaiveDateTime::parse_from_str(val, f.as_str()) {
                                                Ok(date) => {
                                                    let millis = date.timestamp() * 1000;
                                                    millis.into()
                                                }
                                                Err(_) => Value::Null,
                                            }
                                        }
                                        Err(err) => {
                                            println!("Error date: {}", err);
                                            Value::Null
                                        }
                                    }
                                }
                                None => {
                                    println!("Could not format date to int using format");
                                    Value::Null
                                }
                            };
                        }

                        let new_value = match data_type {
                            "string" => value.as_str().map(|s| Value::String(s.to_string())),
                            "timestamp" | "timestamp_milli" | "int" | "integer" | "long" => {
                                value.as_i64().map(Value::from)
                            }
                            "double" => value.as_f64().map(Value::from),
                            "boolean" => value.as_bool().map(Value::from),
                            _ => None,
                        };

                        new_value.unwrap_or(Value::Null);

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
            }
        }
        new_value
    } else {
        let discoverd_data_type = discoverIngest(field, value, metadata, updatedSchema);

        return fast_set_value(&discoverd_data_type, &field, value, metadata, updatedSchema);
    }
}

fn discoverIngest(
    field: &str,
    value: &Value,
    metadata: &mut HashMap<String, Metadata>,
    updatedSchema: &mut String,
) -> String {
    let foo: AnalyseSchema = AnalyseSchema { i: 0 };

    /////

    // let mut jsonValue: Value;
    //
    // jsonValue = value.clone();
    //
    // if value.as_str().is_some() {
    //     if serde_json::from_str(value.as_str().unwrap()).unwrap_or(false) {
    //         if serde_json::from_str(value.as_str().unwrap()).unwrap() {
    //             jsonValue = serde_json::from_str(value.as_str().unwrap()).unwrap();
    //         }
    //     }
    // }

    // AnalyseSchema::analyse_payload(&mut foo, &mut jsonValue, metadata);

    // AnalyseSchema::analyse_field(&foo, &field.to_string(), &mut jsonValue, metadata);

    ////

    AnalyseSchema::analyse_field(
        &foo,
        &field.to_string(),
        value.clone().borrow_mut(),
        metadata,
    );

    AnalyseSchema::determine_field_types(metadata, None);

    // println!("{:?}", metadata.get_mut(field).unwrap());

    let discoverd_data_type = &metadata.get(field).unwrap().determined_type;

    println!(
        "Discovered new field: '{}' of type: '{}'",
        field, discoverd_data_type
    );

    // let handle = tokio::runtime::Handle::current();
    // handle.enter();
    // block_on(Config::set_config(&metadata, true));
    // spawn_blocking(Config::set_config(&metadata, true).await);

    //@todo - if mutable mode

    // exit(0);

    *updatedSchema = "yes".to_string();

    discoverd_data_type.clone()
}

#[cfg(test)]
mod tests {
    use serial_test::serial;
    use std::collections::HashMap;
    use std::fs::{remove_file, File, OpenOptions};
    use std::io::{Seek, Write};

    use parquet::data_type::AsBytes;
    use rand::Rng;
    use std::path::Path;

    use serde_json::Value;

    use crate::discover::AnalyseSchema;

    use crate::ingest::ingest_fast::fast_path_ingest;
    use crate::serdes::json::SerdeJson;

    #[test]
    #[serial]
    fn test_discover_complex_types() {
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

        test_file.write(&record_line.as_bytes()).unwrap();

        test_file.rewind().unwrap();

        let in_file = File::open(format!("./{}", random_tmp_file_name)).unwrap();
        // let mut in_file = MemFile::create(rng.gen::<i32>(), CreateOptions::new()).unwrap();

        let mut metadata = HashMap::new();

        let mut newMeta =
            AnalyseSchema::infer_json_schema(&mut foo, in_file, Some(1), &mut metadata).unwrap();

        let str = r#"{"rider_id":"10e974bf-4a43-305a-9e39-1636c43cb22a","bike_id":"8b86f753-05f8-3254-aba6-739188a3c0b6","isbn":"9407496597","trip":{"start_temprature":0,"end_temprature":2},"last_crank":[2,15,33,45,56,57,47,36,19,5],"crank_torques":[[2,15,33,45,56,57,47,36,19,5],[1,13,33,48,56,58,45,35,15,6]],"hardware":{"manufacturer":"Beier, Emmerich and Rutherford","model":"synergize ubiquitous e-commerce","maintenance":{"last_rebuild":"20\/04\/2010","last_service":"12\/07\/1973"}},"metadata":{"rcvd_time":1615474895,"sent_time":1615474930,"prcd_micro_time":1615474853.999185,"tags":[{"name":"type","value":"trip"},{"name":"auto","value":false}]}}"#;

        let records: Vec<Value> = SerdeJson::deserialize(&str);

        let mut updatedSchema = "no".to_string();

        // NOTE: This schema is assert tested in discovery
        let ingestValue = fast_path_ingest(
            records.first().unwrap(),
            &mut newMeta.get_mut("").unwrap().fields,
            &mut updatedSchema,
        );

        let _f = [2, 15, 33, 45, 56, 57, 47, 36, 19, 5];
        let _v = ingestValue.get("last_crank").unwrap().as_array().unwrap();

        remove_file(Path::new(&format!("./{}", random_tmp_file_name)));
    }
}
