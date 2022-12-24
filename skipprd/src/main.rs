mod arr;

use std::any::Any;
use std::collections::HashMap;
use crate::arr::Arr;
mod helpers;
use crate::helpers::Helpers;
mod metrics;
use crate::metrics::Metrics;
mod discover;
use crate::discover::AnalyseSchema;
use crate::discover::Metadata;
// mod converters;
// use self::converters::avro_parquet::AvroSchema;




use serde_json::{Result, Value};


pub fn untyped_example() -> HashMap<String, Metadata> {
    // Some JSON input data as a &str. Maybe this comes from the user.
    let data = r#"
    {
        "rider_id":"10e974bf-4a43-305a-9e39-1636c43cb22a",
        "bike_id":"8b86f753-05f8-3254-aba6-739188a3c0b6",
        "isbn":"9407496597",
        "trip":{
            "start_temprature":0,
            "end_temprature":2
            },
        "last_crank":[2,15,33,45,56,57,47,36,19,5],
        "crank_torques":[[2,15,33,45,56,57,47,36,19,5],[1,13,33,48,56,58,45,35,15,6]],
        "hardware":{
            "manufacturer":"Beier, Emmerich and Rutherford",
            "model":"synergize ubiquitous e-commerce",
            "maintenance":{
                "last_rebuild":"20\/04\/2010",
                "last_service":"12\/07\/1973"
            }
        },
        "metadata":{
            "rcvd_time":1615474895,
            "sent_time":1615474930,
            "prcd_micro_time":1615474853.999185,
            "tags":[
                {
                    "name":"type",
                    "value":"trip"
                },
                {
                    "name":"auto",
                    "value":false
                }
            ]
        }
    }
    "#;

    // let data = r#"
    //     {
    //         "name": "John Doe",
    //         "age": 43,
    //         "phones": [
    //             "+44 1234567",
    //             "+44 2345678"
    //         ],
    //         "metadata": {
    //             "tags": [],
    //         }
    //     }"#;

    // Parse the string of data into serde_json::Value.
    let mut v: Value = serde_json::from_str(data).unwrap();

    let mut foo: AnalyseSchema = AnalyseSchema { i: 0};

    // let mut newMeta = Metadata {
    //     count: 0,
    //     types: HashMap::new(),
    //     parent_type: "".to_string(),
    //     fields: Box::new(Default::default()),
    //     date_candidate: None,
    //     evolution: Box::new(Default::default()),
    //     enabled: true,
    //     determined_type: "".to_string(),
    // };

    // let mut newMeta: &mut Option<HashMap<String, &mut Metadata>> = &mut None;

    let mut newMeta =  Metadata {
        count: 0,
        types: HashMap::new(),
        parent_type: "".to_string(),
        fields: Box::new(Default::default()),
        date_candidate: None,
        evolution: Box::new(Default::default()),
        enabled: true,
        determined_type: "".to_string(),
    };

    let mut metadata = HashMap::new();
    metadata.insert("skpr-time".to_string(), newMeta);
    let mut newMeta: &mut HashMap<String, Metadata> = &mut metadata;

    AnalyseSchema::analyse_payload(&mut foo, &mut v, &mut newMeta);

    println!("Rider is types: {:?}", newMeta.get_mut("rider_id").unwrap().types);
    println!("last_crank is types: {:?}", newMeta.get_mut("last_crank").unwrap().types);
    println!("Hardware is types: {:?}", newMeta.get_mut("hardware").unwrap().types);
    println!("Hardware.maintenance is types: {:?}", newMeta.get_mut("hardware").unwrap().fields.get_mut("maintenance").unwrap().types);
    println!("Metadata is types: {:?}", newMeta.get_mut("metadata").unwrap().types);
    println!("Metadata.rcvd_time is types: {:?}", newMeta.get_mut("metadata").unwrap().fields.get_mut("rcvd_time").unwrap().types);
    println!("Metadata.tags is types: {:?}", newMeta.get_mut("metadata").unwrap().fields.get_mut("tags").unwrap().types);

    return newMeta.clone();

    // println!("Phones is a array {}", v["phones"].is_array());
    // println!("Phone is a array {}", v["phones"][0].is_array());
    // println!("Phones is a string {}", v["phones"].is_string());
    // println!("Phone is a string {}", v["phones"][0].is_string());

    // Access parts of the data by indexing with square brackets.
    // println!("Age as a Rust String {}", v["age"].to_string());
    // println!("Phones as a Rust String {}", v["phones"].to_string());
    // println!("Phone as a Rust String {}", v["phones"][0].to_string());
    // println!("Name as a Rust String {}", v["name"].to_string());

    // Ok(())
}

#[test]
fn test_untyped_example() {
    assert_eq!(untyped_example().get_mut("phones").unwrap().determined_type, "".to_string());
}

fn main() {

    untyped_example();
}

// fn skippr_emit(
//     payload: char,
//     offset: char,
//     namespace: char,
//     partition: char
// ) {
//
// }

