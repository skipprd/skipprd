use arrow::error::ArrowError;
use std::any::Any;
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::Read;

use chrono::{DateTime, LocalResult, NaiveDate, NaiveDateTime, TimeZone, Utc};
use serde_derive::{Deserialize, Serialize};

use serde_json::Value;

// use std::simd::usizex2;

use crate::helpers::Helpers;
pub(crate) mod date_formats;
use crate::discover::date_formats::DateFormats;
mod filter_float;

mod filter_bool;
use crate::discover::filter_bool::parse_bool;
mod filter_parse_int;

pub mod arrow_schema;
use crate::helpers::configuration::Config;
use crate::ingest::ingest::IngestRecord;
use crate::serdes::json::SerdeJson;

#[derive(Default, Debug, Clone, Deserialize, Serialize)]
pub struct DateCandidate {
    pub(crate) check_count: i32,
    pub(crate) valid_count: i32,
    pub(crate) field: String,
    pub(crate) format: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Evolution {
    type_string: String,
    new_value: String,
    sovled: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Metadata {
    pub(crate) count: i32,
    pub(crate) types: HashMap<String, u32>,
    pub(crate) parent_type: String,
    pub(crate) fields: Box<HashMap<String, Metadata>>,
    pub(crate) date_candidate: Option<DateCandidate>,
    pub(crate) evolution: Box<HashMap<String, Evolution>>,
    pub(crate) enabled: bool,
    pub(crate) out_field_name: String,
    pub(crate) determined_type: String,
    pub(crate) determined_type_values: String,
}

// pub struct IterMut<'a, Met> {
//     obj: &'a mut Metadata,
//     cursor: usize,
// }
//
//
// impl<'a, T> Iterator for IterMut<'a, T> {
//     // type Item = &'a T;
//     type Item = &'a mut T;
//
//     fn next(&mut self) -> Option<Self::Item> {
//         self.next.take().map(|node| {
//             self.next = node.next.as_deref_mut();
//             &mut node.elem
//         })
//     }
// }

impl Metadata {
    #[inline]
    #[must_use]
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            count: 0,
            types: Default::default(),
            parent_type: "".to_string(),
            fields: Box::default(),
            date_candidate: None,
            evolution: Box::default(),
            enabled: true,
            out_field_name: "".to_string(),
            determined_type: "".to_string(),
            determined_type_values: "".to_string(),
        })
    }
}

pub struct AnalyseSchema {
    pub i: i32,
    // pub discovered_field_occurrence: HashMap<String, i32>,
    // pub continue_: HashSet<String>,
    // pub data_type_masks: HashMap<String, String>,
    // pub data_type_drops: HashMap<String, String>,
    // pub data_type_casts: HashMap<String, Vec<String>>,
}

const DATE_FIELD_VALIDATION_MIN_SAMPLE: i32 = 100;

fn get_type(value: &str) -> String {
    let _foo = "";

    match value.parse::<i32>() {
        Ok(_bool) => {
            return "integer".to_string();
        }
        Err(..) => {
            let timmed_value = value.trim_matches('"');
            let json_value: Result<i32, _> = serde_json::from_str(timmed_value);
            match json_value {
                Ok(_) => {
                    return "integer".to_string();
                }
                Err(_) => {}
            }
        }
    }

    match value.parse::<i64>() {
        Ok(_bool) => {
            return "long".to_string();
        }
        Err(..) => {
            let timmed_value = value.trim_matches('"');
            let json_value: Result<i128, _> = serde_json::from_str(timmed_value);
            match json_value {
                Ok(_) => {
                    return "long".to_string();
                }
                Err(_) => {}
            }
        }
    }

    match value.parse::<f32>() {
        Ok(_bool) => {
            return "double".to_string();
        }
        Err(..) => {}
    }

    let v: Value = serde_json::from_str(value).unwrap_or_default();
    match v.is_array().then_some(true) {
        Some(_bool) => {
            return "array".to_string();
        }
        None => {}
    }

    match v.is_object().then_some(true) {
        Some(_bool) => {
            return "array".to_string();
        }
        None => {}
    }

    match value.parse::<String>() {
        Ok(_bool) => {
            return "string".to_string();
        }
        Err(_String) => {}
    }

    match value.parse::<String>() {
        Ok(_bool) => {
            return "string".to_string();
        }
        Err(_String) => {}
    }

    "unknown".to_string()
}

const min_discovery_records: i32 = 100;

impl AnalyseSchema {
    // pub fn validate_schema<R: Read>(
    //     &mut self,
    //     reader: &mut BufReader<R>,
    //     schema_ref: Arc<Schema>,
    // )
    // // -> Result<bool, String> {
    // {
    //     let value_iter = ValueIter::new(reader, Some(0));
    //
    //     for record in value_iter {
    //
    //
    //         let batch = RecordBatch::try_new_with_options(
    //             schema_ref,
    //             vec![Arc::new(BinaryArray::from(record.unwrap().to_string().as_bytes().to_vec()))],
    //             &RecordBatchOptions::new().with_match_field_names(true)
    //         ).unwrap();
    //
    //         // match batch.t {
    //         //
    //         // }
    //
    //
    //         // let mut vs: Vec<Value> = SerdeJson::deserialize(record.unwrap().to_string());
    //         // // let mut vs: Vec<Value> = serde_json::from_str(&line.unwrap()).unwrap();
    //         //
    //         // for mut v in vs {
    //         //     println!("Discovering schema for {}", v);
    //         // }
    //     }
    //
    // }

    pub fn infer_json_schema(
        &mut self,
        input_file: File,
        _max_read_records: Option<usize>,
        newMeta: &mut HashMap<std::string::String, Metadata>,
    ) -> Result<HashMap<std::string::String, Metadata>, ArrowError> {
        // self.infer_json_schema_from_iterator(ValueIter::new(reader, max_read_records))
        self.infer_json_schema_from_iterator(input_file, newMeta)
    }

    // pub fn infer_json_schema_from_iterator<I>(&mut self, value_iter: I) -> Result<HashMap<std::string::String, Metadata>, ArrowError>
    //     where
    //         I: Iterator<Item = Result<Value, ArrowError>>,
    // {
    pub fn infer_json_schema_from_iterator(
        &mut self,
        mut input_file: File,
        newMeta: &mut HashMap<std::string::String, Metadata>,
    ) -> Result<HashMap<std::string::String, Metadata>, ArrowError> {
        let mut parse_namespace_cache: HashMap<String, String> = HashMap::new();

        let mut skpr_namespace: String = "".to_string();

        let faltten_events = &Config::getenv("TRANSFORM_FLATTEN_EVENTS", "no");

        // let mut newMeta: &mut HashMap<String, Metadata>;
        // let defaultMetadata = Metadata::new().unwrap();
        // let mut metadata = HashMap::new();
        // newMeta = &mut metadata;

        let mut foo: AnalyseSchema = AnalyseSchema { i: 0 };

        let str: &mut String = &mut "".to_string();
        input_file.read_to_string(str);

        let records: Vec<Value> = SerdeJson::deserialize(str);

        let mut i = 0;

        for mut v in records {
            let source_namespace = Config::getenv("S3_BUCKET", "");

            // let source_namespace = BufferChunker::decode_file_namespace(path.to_str().unwrap());
            // let source_partition = BufferChunker::decode_file_partition(path.to_str().unwrap());
            skpr_namespace = Helpers::parse_namespace_field(
                &v,
                source_namespace.clone(),
                &mut parse_namespace_cache,
            );

            if !newMeta.contains_key(&skpr_namespace) {
                newMeta.insert(skpr_namespace.clone(), Metadata::new().unwrap());
                // newMeta = &mut metadata.clone();
            }

            // if Config::truth_value(faltten_events) {
            //     v = Helpers::flatten(&v);
            // }

            i += 1;

            if i >= min_discovery_records {
                break;
            }
            // let string = record.unwrap().to_string();

            // println!("record: {:?}", v);

            if v.is_null() {
                continue;
            }

            // let mut vs: Vec<Value> = serde_json::from_str(&line.unwrap()).unwrap();

            // let b = InternalFields;
            //
            // InternalFields::parse_namespace_field(vs, "example");

            // for mut v in vs {
            // println!("Discovering schema for {}", v);

            match v.type_id() {
                _Value => {
                    let mut ingest_record = IngestRecord {
                        source_namespace,
                        source_partition: "".to_string(),
                        skpr_event_ts: 0,
                        skpr_namespace: skpr_namespace.clone(),
                        skpr_partition: "".to_string(),
                        record: v,
                    };

                    AnalyseSchema::analyse_payload(
                        &mut foo,
                        &mut ingest_record.record,
                        &mut newMeta
                            .get_mut(&ingest_record.skpr_namespace)
                            .unwrap()
                            .fields,
                    );
                }
                value => {
                    return Err(ArrowError::ParseError(format!(
                        "Expected serde Value, found {:?}",
                        value
                    )));
                }
            };
            // }
        }

        // let metadata = newMeta.clone();

        Ok(newMeta.clone())
    }

    // pub fn analyse_payload(&mut self, message: &HashMap<String, String>, metadata: &mut HashMap<String, Metadata>) {
    pub fn analyse_payload(&mut self, message: &Value, metadata: &mut HashMap<String, Metadata>) {
        self.i += 1;

        // let mut helpers = Helpers { clean_field_cache: Default::default() };

        for (field, value) in message.as_object().unwrap() {
            // let field = Helpers::clean_field_name(field.to_string());

            self.init_discovered_type(metadata, &field);

            let mut jsonValue: Value;

            jsonValue = value.clone();

            if value.as_str().is_some()
                && serde_json::from_str(value.as_str().unwrap()).unwrap_or(false)
                && serde_json::from_str(value.as_str().unwrap()).unwrap()
            {
                jsonValue = serde_json::from_str(value.as_str().unwrap()).unwrap();
            }

            self.analyse_field(&field, &mut jsonValue, metadata);
        }
    }

    pub fn analyse_field(
        &self,
        field: &String,
        value: &mut Value,
        metadata: &mut HashMap<String, Metadata>,
    ) {
        // Build mapping/Schema
        // if metadata.get(field).unwrap().count < Config::min_discovery_records. {
        if metadata.get_mut(field).is_none()
            || metadata.get_mut(field).unwrap().count < min_discovery_records
        {
            // if value.is_string() {
            self.resolve_field_type(metadata, field, value);
            // }
        }

        if value.is_object() {
            for (sub_field, sub_value) in value.as_object().unwrap() {
                // let sub_field = Helpers::clean_field_name(sub_field.to_string());

                let mut sv = sub_value.clone();
                // let mut svv: Value = serde_json::from_str(sv.unwrap()).unwrap();
                self.analyse_field(
                    &sub_field,
                    &mut sv,
                    metadata.get_mut(field).unwrap().fields.as_mut(),
                );
            }
        }

        if value.is_array() {
            let mut i = 0;
            for sub_value in value.as_array().unwrap() {
                let mut sv = sub_value.clone();
                self.analyse_field(
                    // &Helpers::clean_field_name(i.to_string()),
                    &i.to_string(),
                    &mut sv,
                    metadata
                        .get_mut(&field.to_string())
                        .unwrap()
                        .fields
                        .as_mut(),
                );
                i += 1;
            }
        }

        // use array_init::array_init;
        //
        // if array_init::from_iter(value) == Some() {
        // // if value.is::<[T; N]>() && !empty(value) {
        // //     for (sub_field, sub_value) in value {
        // //         self.analyse_field(sub_field, sub_value, Metadata.get(field).unwrap().fields);
        // //     }
        // }
    }

    pub fn resolve_field_type(
        &self,
        metadata: &mut HashMap<String, Metadata>,
        field: &String,
        value: &mut Value,
    ) -> String {
        self.init_discovered_type(metadata, field);

        let mut data_type = self.get_logical_type(field, value, metadata, true);

        if data_type == "array"
        // && value.as_array().is_some()
        // && value.is_array()
        {
            let mut is_sequential = false;
            if value.as_array().is_some() {
                // let is_sequential = Helpers::is_sequential_array_keys(value.as_array().unwrap());
                is_sequential = Helpers::is_sequential_array_keys(value.as_array().unwrap());
            }

            let mut type_count = HashMap::new();

            if value.as_object().is_some() {
                // println!("{:?} as object", field);

                for (sub_field, sub_value) in value.as_object().unwrap() {
                    let mut sv: Value = serde_json::from_str(&sub_value.to_string()).unwrap();

                    let logical_type = self.get_logical_type(sub_field, &mut sv, metadata, false);

                    // if (field == "trip") {
                    // println!("#### trip sub_field NAME {:?}", sub_field);
                    // println!("#### trip sub field value {}", sv);
                    // println!("#### trip sub field value type {}", logical_type);
                    // }

                    type_count.insert(logical_type, "hit");

                    // Special handling of bools in array/map of ints
                    // [1,2,3] may discover as schema [bool, int, int] and therefore
                    // parent field resolve type as `record`.
                    // When in fact we'd want to discover schema as [int, int, int] and
                    // parent field resolve as `array`.
                    if type_count.len() == 2
                        && type_count.contains_key("integer")
                        && type_count.contains_key("boolean")
                    {
                        type_count.remove("boolean");
                    }
                }
            }

            if value.as_array().is_some() {
                // println!("{:?} as array", field);

                let mut i = 0;

                for sub_value in value.as_array().unwrap() {
                    let mut sv: Value = serde_json::from_str(&sub_value.to_string()).unwrap();

                    let logical_type =
                        self.get_logical_type(&i.to_string(), &mut sv, metadata, false);

                    i += 1;
                    // if (field == "trip") {
                    // println!("#### trip sub_field NAME {:?}", sub_field);
                    // println!("#### trip sub field value {}", sv);
                    // println!("#### trip sub field value type {}", logical_type);
                    // }

                    type_count.insert(logical_type, "hit");

                    // Special handling of bools in array/map of ints
                    // [1,2,3] may discover as schema [bool, int, int] and therefore
                    // parent field resolve type as `record`.
                    // When in fact we'd want to discover schema as [int, int, int] and
                    // parent field resolve as `array`.
                    if type_count.len() == 2
                        && type_count.contains_key("integer")
                        && type_count.contains_key("boolean")
                    {
                        type_count.remove("boolean");
                    }
                }
            }

            // if is_sequential {
            //     // array of sequential int keys is an avro array
            //     data_type = "array".to_string();
            // } else if !is_sequential {
            //     // associative array is an avro map
            //     data_type = "map".to_string();
            // }

            // Multiple type within array values?
            // Must be a record then.
            if type_count.len() > 1 {
                data_type = "record".to_string();
                // Array of Arrays? Use a Record for the parent.
            } else if type_count.contains_key("array") {
                data_type = "record".to_string();
            } else if is_sequential {
                // array of sequential int keys is an avro array
                data_type = "array".to_string();
            } else if !is_sequential {
                // associative array is an avro map
                data_type = "map".to_string();
            }

            // println!("{:?}", field);
            // println!("{:?}", data_type);
        }

        self.set_discovered_occurrence(metadata, field, &data_type, &mut value.to_string());

        data_type
    }

    pub fn get_logical_type(
        &self,
        field: &String,
        json_value: &mut Value,
        metadata: &mut HashMap<String, Metadata>,
        allow_date: bool,
    ) -> String {
        // let value: &mut String = &mut json_value.as_str().unwrap().to_string();
        let value: &mut String = &mut json_value.to_string();

        let mut data_type = get_type(value);

        if data_type == *"string" || data_type == *"integer" || data_type == *"double" {
            // String really an int?
            data_type = self.check_string_or_int(value);

            if allow_date {
                let mut valid_timestamp = false;

                if data_type == *"integer" {
                    valid_timestamp = self.is_valid_timestamp(value);
                } else if data_type == *"long" {
                    valid_timestamp = self.is_valid_timestamp(value);
                }

                if valid_timestamp {
                    // self.set_date_field_candidate(field, metadata, &"".to_string());
                    self.increment_date_field_candidate_count(field, metadata, &"".to_string());
                }
            }

            // if self.is_float(value) {
            //     if Some(Float(value)) {
            //         data_type = "double";
            //     }
            // }
        }

        if data_type == *"string" && allow_date {
            // Limit number of check type attempts for data as expensive operation.

            if metadata
                .get_mut(field)
                .unwrap()
                .date_candidate
                .as_mut()
                .is_none()
                || metadata
                    .get_mut(field)
                    .unwrap()
                    .date_candidate
                    .as_mut()
                    .unwrap()
                    .check_count
                    < DATE_FIELD_VALIDATION_MIN_SAMPLE
            {
                // println!("Checking if {} is date type", field);

                let value_str = json_value.as_str().unwrap_or("");

                if let Some(format) = self.is_valid_date(value_str) {
                    data_type = "date".to_string();
                    // self.set_date_field_candidate(field, metadata, &format);
                    self.increment_date_field_candidate_count(field, metadata, &format.to_string());
                }

                // Already hit date field check limit. Force set type if valid date field.
            } else if metadata
                .get_mut(field)
                .unwrap()
                .date_candidate
                .as_mut()
                .unwrap()
                .valid_count
                >= DATE_FIELD_VALIDATION_MIN_SAMPLE
            {
                data_type = "date".to_string();
            }
        }

        // @todo
        if data_type == *"integer" {
            match parse_bool(value) {
                Err(_i32) => {
                    // println!("Not float");
                }
                Ok(_bool) => {
                    // println!("Is float");
                    return "boolean".to_string();
                }
            }
        }
        // if data_type != "double" && is_bool(filter_var(value, FILTER_VALIDATE_BOOLEAN, FILTER_NULL_ON_FAILURE)) {
        //     data_type = "boolean".to_string();
        // }

        if data_type == *"NULL" {
            // most systems won't support null
            data_type = "string".to_string();
        }
        // @todo - logical interpretation based on field name

        data_type
    }

    pub fn check_string_or_int(&self, value: &mut String) -> String {
        let mut data_type = get_type(value);

        if value.parse::<i32>().is_ok() && (&mut value.parse::<i32>().unwrap().to_string() == value)
        {
            if self.is32bitSignedInt(value) {
                data_type = "integer".to_string();
            } else if self.is64bitSignedInt(value) {
                data_type = "long".to_string();
            }
        }

        data_type
    }

    pub fn increment_date_field_candidate_count(
        &self,
        field: &str,
        metadata: &mut HashMap<String, Metadata>,
        format: &String,
    ) {
        if metadata.get(field).unwrap().date_candidate.is_none() {
            let can: DateCandidate = DateCandidate {
                check_count: 1,
                valid_count: 0,
                field: field.to_string(),
                format: format.to_string(),
            };

            metadata.get_mut(field).unwrap().date_candidate.insert(can);
        } else {
            metadata
                .get_mut(field)
                .unwrap()
                .date_candidate
                .as_mut()
                .unwrap()
                .check_count += 1;
        }
    }

    pub fn set_date_field_candidate(
        &self,
        field: &String,
        metadata: &mut HashMap<String, Metadata>,
        format: &String,
    ) {
        if metadata
            .get_mut(field)
            .unwrap()
            .date_candidate
            .as_mut()
            .is_some()
            && metadata
                .get_mut(field)
                .unwrap()
                .date_candidate
                .as_mut()
                .unwrap()
                .valid_count
                == 0
        {
            metadata
                .get_mut(field)
                .unwrap()
                .date_candidate
                .as_mut()
                .unwrap()
                .valid_count = 1;
        } else {
            metadata
                .get_mut(field)
                .unwrap()
                .date_candidate
                .as_mut()
                .unwrap()
                .valid_count += 1;
        }

        if metadata
            .get_mut(field)
            .unwrap()
            .date_candidate
            .as_mut()
            .unwrap()
            .valid_count
            >= DATE_FIELD_VALIDATION_MIN_SAMPLE
        {
            metadata
                .get_mut(field)
                .unwrap()
                .date_candidate
                .as_mut()
                .unwrap()
                .field = field.clone();

            if !format.is_empty() {
                metadata
                    .get_mut(field)
                    .unwrap()
                    .date_candidate
                    .as_mut()
                    .unwrap()
                    .format = format.clone();
            }
        }
    }

    pub fn is_float(&self, test: &mut String) -> bool {
        let type_string = test.parse::<f64>();
        match type_string {
            Ok(_) => true,
            Err(_) => false,
        }
    }

    fn is32bitSignedInt(&self, value: &mut String) -> bool {
        // let value = value as i32;
        const min: i32 = -2147483647;
        const max: i32 = 2147483647;

        let mut result = false;
        if value.parse::<i32>().unwrap() > min {
            result = true
        }

        if result && value.parse::<i32>().unwrap() < max {
            result = true
        }

        result
    }

    fn is64bitSignedInt(&self, value: &mut String) -> bool {
        let value = value.parse::<i64>().unwrap();
        let options = [-9223372036854775807, 9223372036854775807];
        let mut result = false;
        for i in options.iter() {
            if value == *i {
                result = true;
            }
        }
        result
    }

    pub fn is_valid_timestamp(&self, timestamp: &mut String) -> bool {
        match timestamp.parse::<i64>() {
            Ok(seconds) => {
                match NaiveDateTime::from_timestamp_opt(seconds, 0) {
                    Some(dt) => {
                        let date = Utc.from_utc_datetime(&dt);

                        // let date = Utc.timestamp_opt(seconds, 0).unwrap();
                        let min_date = Utc.ymd(1970, 1, 1).and_hms(0, 0, 0);
                        let max_date = Utc.ymd(2040, 1, 1).and_hms(0, 0, 0);
                        if date >= min_date && date <= max_date {
                            return true;
                        }
                        false
                    }
                    None => false,
                }
            }
            Err(_) => false,
        }
    }

    pub(crate) fn is_valid_date(&self, value: &str) -> Option<&str> {
        for format in DateFormats::iterator() {
            let found_format = match DateTime::parse_from_str(value, format.as_str()) {
                Ok(_) => {
                    // println!("Value {} is format {}", value, format.as_str());
                    return Some(format.name());
                }
                Err(_) => {
                    match NaiveDate::parse_from_str(value, format.as_str()) {
                        Ok(_) => {
                            // println!("Value {} is naive format {}", value, format.as_str());
                            return Some(format.name());
                        }
                        Err(_) => {
                            // println!("Value {} is not a date of format {}: {}, trying naive", value.as_str(), format.name(), format.as_str());
                            None
                        }
                    }
                }
            };

            if found_format.is_some() {
                return found_format;
            }
        }

        // println!("Value {} is not a date of format that's known", value);

        None
    }

    pub fn apply_evolution_factory(
        &self,
        field: &mut std::string::String,
        _value: &mut String,
        evolution: String,
        data_type: &mut String,
        new_value: String,
    ) {
        match &*evolution {
            "cast" => *data_type = new_value,
            "new" => *field = new_value,
            "rename" => *field = new_value,
            "merge" => *field = new_value,
            "default" => {}
            _ => {}
        }
    }

    // pub fn handle_value_error(&self, field: &String, value: &mut String, field_occurrence: &mut HashMap<String, Metadata>) {
    //     let data_type: &mut String = self.get_logical_type(field, value, field_occurrence, false);
    //     let feild_metadata = field_occurrence.get(field).unwrap();
    //     if feild_metadata.evolution.is_some() {
    //         if feild_metadata.evolution.unwrap().get(data_type).unwrap().new_value.is_empty() {
    //             let evolution_type = feild_metadata.evolution.unwrap().get(data_type).unwrap().type_string;
    //             let new_value = feild_metadata.evolution.unwrap().get(data_type).unwrap().new_value;
    //             self.apply_evolution_factory(field, value, evolution_type, data_type, new_value);
    //         }
    //     }
    // }

    fn init_discovered_type(&self, metadata: &mut HashMap<String, Metadata>, field: &String) {
        if metadata.get(field).is_none() {
            let newMeta = Metadata::new().unwrap();

            metadata.insert(field.clone(), newMeta);
        }
    }

    pub fn set_discovered_occurrence(
        &self,
        array: &mut HashMap<String, Metadata>,
        field: &String,
        data_type: &String,
        value: &mut String,
    ) {
        if array
            .get(field)
            .unwrap()
            .types
            .get(&data_type.to_string())
            .is_none()
        {
            array
                .get_mut(field)
                .unwrap()
                .types
                .insert(data_type.to_string(), 1);
            array.get_mut(field).unwrap().evolution.insert(
                data_type.to_string(),
                Evolution {
                    type_string: "".to_string(),
                    new_value: "".to_string(),
                    sovled: false,
                },
            );

            // @todo
            // if !Config::analysing
            //     && Config::run_mode == Config::RUN_MODE_SYNC
            //     && Config::mutable_mode == Config::MUTABLE_MODE_EVOLVE
            // {
            //     // auto-accept new fields and types when syncing in 'evolve' mode
            //     array.get_mut(field).unwrap().determined_type = data_type.to_string();
            // }
        } else {
            let newCount: u32 = array.get_mut(field).unwrap().types.get(data_type).unwrap() + 1;
            array
                .get_mut(field)
                .unwrap()
                .types
                .insert(data_type.to_string(), newCount);
        }

        if data_type == "integer" {
            let valid_timestamp = self.is_valid_timestamp(value);
            if valid_timestamp {
                self.set_discovered_occurrence(array, field, &"timestamp".to_string(), value);
            }
        } else if data_type == "long" {
            let valid_timestamp = self.is_valid_timestamp(value);
            if valid_timestamp {
                self.set_discovered_occurrence(array, field, &"timestamp_milli".to_string(), value);
            }
        }
    }

    pub fn determine_field_types(
        metadata: &mut HashMap<String, Metadata>,
        parent_type: Option<&str>,
        parent_field: Option<&str>,
        flatten: bool,
    ) {
        let demoted_types = vec!["boolean", "date", "timestamp", "timestamp_milli"];

        for (field_name, field) in metadata.iter_mut() {
            // Useful for field evolution logic for maps, which only support one sub-field type
            if let Some(parent_type) = parent_type {
                field.parent_type = parent_type.to_string();
            }

            if flatten {
                if let Some(parent_field) = parent_field {
                    field.out_field_name = format!(
                        "{}_{}",
                        parent_field,
                        Helpers::clean_field_name(field_name.to_string())
                    );
                } else {
                    field.out_field_name = Helpers::clean_field_name(field_name.to_string());
                }
            } else {
                field.out_field_name = Helpers::clean_field_name(field_name.to_string());
            }

            if field.determined_type == *"" {
                let mut highest_type = "".to_string();
                let mut highest_count = 0;

                if !field.types.is_empty() {
                    // Don't allow NULL type if we discovered any other types
                    if field.types.len() > 1 {
                        field.types.remove("NULL");
                    }

                    // force to record type over map or array if ever present
                    if field.types.contains_key("record") {
                        field.determined_type = "record".to_string();
                    } else {
                        for (data_type, data_type_count) in field.types.iter() {
                            // println!("eavluating type {} with count {}", data_type, data_type_count);

                            if highest_count < *data_type_count {
                                // Prefer primitive types to logical types or types
                                // that cause frequent false positives (demoted types).
                                // - if there's multiple discovered types
                                // - and the most common type is a demoted type
                                // - select the next most common, non-date type
                                if field.types.len() == 1
                                    || (field.types.len() > 1
                                        && !demoted_types.contains(&data_type.as_str()))
                                {
                                    highest_type = data_type.to_string();
                                    highest_count = *data_type_count;
                                }
                            }
                        }

                        field.determined_type = highest_type;
                    }
                }
            }

            if !field.determined_type.is_empty()
                && vec!["map", "array", "record"].contains(&field.determined_type.as_str())
                && field.fields.len() > 0
            {
                if field.determined_type_values == "".to_string()
                    && (field.determined_type == "array" || field.determined_type == "map")
                {
                    // field.determined_type_values = "".to_string();

                    // Ignore sub-fields for Avro array, the values are just enumerated, their not fields themselves.
                    // Else we'd create a field list with string keys for each array value
                    // e.g. [1,5,3,7,4,3,5]
                    // would incorrectly become ['a0' => 1, 'a1' => 5, ...]

                    let mut type_count = BTreeMap::new();

                    // @todo - not intended to build avro type array here
                    //         however, 'array' type is a special case... how to handle?

                    // Get avro arrays items primitive data type
                    for (_sub_field, sub_value) in field.fields.iter() {
                        for (data_type, data_type_count) in sub_value.types.iter() {
                            // Prefer primitive types to logical types or types
                            // that cause frequent false positives (demoted types).
                            // - if there's multiple discovered types
                            // - and the most common type is a demoted type
                            // - select the next most common, non-date type
                            //                                if (!in_array($dataType, $demotedTypes)) {
                            if type_count.len() <= 1
                                || (type_count.len() > 1
                                    && !demoted_types.contains(&data_type.as_str()))
                            {
                                if type_count.get(data_type).is_none() {
                                    type_count.insert(data_type.to_string(), *data_type_count);
                                } else {
                                    *type_count.get_mut(data_type).unwrap() += data_type_count;
                                }
                            }
                        }
                    }

                    let mut values_type: &String = &"".to_string();

                    if !type_count.is_empty() {
                        values_type = type_count
                            .iter()
                            .max_by(|a, b| a.1.cmp(b.1))
                            .map(|(k, _v)| k)
                            .unwrap();
                    }

                    // println!("HIGHEST TYPE: {}", values_type);

                    field.determined_type_values = values_type.to_string();

                    if field.determined_type == *"array" {
                        field.fields.clear();
                    }
                }

                // println!("Field {} determined type is {}", field_name, field.determined_type);
                // println!("Field {} values type is {}", field_name, field.determined_type_values);

                if field.determined_type != *"array" {
                    let _fo = "";
                    AnalyseSchema::determine_field_types(
                        &mut field.fields,
                        Some(&field.determined_type),
                        Some(&field.out_field_name),
                        flatten,
                    );
                }
            }
        }

        // println!("Metadata {:?}", metadata);
    }

    pub fn merge_metadata(
        foo: &mut HashMap<String, Metadata>,
        bar: &mut HashMap<String, Metadata>,
    ) {
        for (key, value) in bar.drain() {
            foo.entry(key.clone())
                .and_modify(|metadata| {
                    let value = value.clone();

                    metadata.count += value.count;
                    for (t, count) in value.types {
                        *metadata.types.entry(t).or_insert(0) += count;
                    }
                    if metadata.parent_type.is_empty() {
                        metadata.parent_type = value.parent_type.clone();
                    }
                    for (field, field_metadata) in *value.fields {
                        *metadata.fields.entry(field).or_insert(field_metadata) =
                            field_metadata.clone();
                    }
                    metadata.date_candidate =
                        value.date_candidate.or(metadata.date_candidate.take());
                    for (evolution_key, evolution_value) in *value.evolution {
                        *metadata
                            .evolution
                            .entry(evolution_key)
                            .or_insert(evolution_value) = evolution_value.clone();
                    }
                    metadata.enabled = metadata.enabled || value.enabled;
                    if !value.determined_type.is_empty() {
                        metadata.determined_type = value.determined_type.clone();
                    }
                    if !value.determined_type_values.is_empty() {
                        metadata.determined_type_values = value.determined_type_values.clone();
                    }
                })
                .or_insert(value);
        }
    }
}

#[cfg(test)]
mod is_valid_date_tests {
    use crate::discover::date_formats::DateFormats;
    use crate::discover::AnalyseSchema;
    use chrono::{DateTime, NaiveDate, NaiveDateTime};
    use serde_json::Value;

    #[test]
    fn test_valid_date_formats() {
        let foo: AnalyseSchema = AnalyseSchema { i: 0 };

        let date_str = "2022-01-07T08:28:07.000Z";
        let json_value: Value = date_str.into();
        let value = json_value.as_str().unwrap();

        assert_eq!(Some("Iso8601"), foo.is_valid_date(value));

        let date_str = "2022-01-05T08:30:12.000Z";
        assert_eq!(Some("Iso8601"), foo.is_valid_date(date_str));

        let fmt = DateFormats::from_str("Iso8601").unwrap();
        assert_eq!("%Y-%m-%dT%H:%M:%S.%fZ", fmt.as_str());
        NaiveDateTime::parse_from_str(date_str, fmt.as_str()).unwrap();

        let date_str = "2022-01-07T08:28:07Z";
        assert_eq!(Some("AtomZ"), foo.is_valid_date(date_str));

        let date_str = "2022-02-22T22:22:22";
        assert_eq!(Some("Atom"), foo.is_valid_date(date_str));

        let mut date_str = "2021-01-03 02:30:00";
        assert_eq!(Some("Mysql"), foo.is_valid_date(date_str));

        date_str = "Tue, 22 Feb 2022 22:22:22 GMT";
        assert_eq!(Some("Rfc850"), foo.is_valid_date(date_str));

        date_str = "2022-02-22";
        assert_eq!(Some("DateOnly"), foo.is_valid_date(date_str));
    }

    #[test]
    fn test_invalid_date_format() {
        let foo: AnalyseSchema = AnalyseSchema { i: 0 };
        let date_str = "2022-22-22";
        assert_eq!(None, foo.is_valid_date(date_str));
    }

    #[test]
    fn test_empty_date_string() {
        let foo: AnalyseSchema = AnalyseSchema { i: 0 };
        let date_str = "";
        assert_eq!(None, foo.is_valid_date(date_str));
    }

    #[test]
    fn test_non_date_string() {
        let foo: AnalyseSchema = AnalyseSchema { i: 0 };
        let date_str = "not a date";
        assert_eq!(None, foo.is_valid_date(date_str));
    }

    #[test]
    fn test_valid_date_time() {
        let foo: AnalyseSchema = AnalyseSchema { i: 0 };
        let date_str = "2022-02-22T22:22:22Z";
        let _dt = DateTime::parse_from_rfc3339(date_str).unwrap();
        assert_eq!(Some("AtomZ"), foo.is_valid_date(date_str));
    }

    #[test]
    fn test_valid_date() {
        let foo: AnalyseSchema = AnalyseSchema { i: 0 };
        let date_str = "2022-02-22";
        let _nd = NaiveDate::parse_from_str(date_str, "%Y-%m-%d").unwrap();
        assert_eq!(Some("DateOnly"), foo.is_valid_date(date_str));
    }
}

#[cfg(test)]
mod discover_date_formats_tests {
    use crate::discover::date_formats::DateFormats;
    use crate::discover::AnalyseSchema;
    use chrono::{DateTime, NaiveDate, NaiveDateTime};
    use serde_json::Value;

    #[test]
    fn test_valid_date_formats() {
        let foo: AnalyseSchema = AnalyseSchema { i: 0 };

        let date_str = "2022-01-05T08:30:12.000Z";
        assert_eq!(Some("Iso8601"), foo.is_valid_date(date_str));

        let fmt = DateFormats::from_str("Iso8601").unwrap();
        assert_eq!("%Y-%m-%dT%H:%M:%S.%fZ", fmt.as_str());
        NaiveDateTime::parse_from_str(date_str, fmt.as_str()).unwrap();

        let date_str = "2022-01-07T08:28:07Z";
        assert_eq!(Some("AtomZ"), foo.is_valid_date(date_str));

        let date_str = "2022-02-22T22:22:22";
        assert_eq!(Some("Atom"), foo.is_valid_date(date_str));

        let mut date_str = "2021-01-03 02:30:00";
        assert_eq!(Some("Mysql"), foo.is_valid_date(date_str));

        date_str = "Tue, 22 Feb 2022 22:22:22 GMT";
        assert_eq!(Some("Rfc850"), foo.is_valid_date(date_str));

        date_str = "2022-02-22";
        assert_eq!(Some("DateOnly"), foo.is_valid_date(date_str));
    }
}

#[cfg(test)]
mod tests {
    use serial_test::serial;
    use std::collections::HashMap;
    use std::fs::{remove_file, File, OpenOptions};
    use std::io::{Seek, Write};

    use crate::discover::{AnalyseSchema, Metadata};
    use crate::helpers::configuration::Config;
    use parquet::data_type::AsBytes;
    use rand::Rng;
    use serde_json::Value;
    use std::path::Path;

    #[test]
    #[serial]
    fn test_discover_arrays_maps() {
        let mut foo: AnalyseSchema = AnalyseSchema { i: 0 };

        // let field = r#"
        // {
        //        "abc3": {"0": "a", "1": "b", "2": "c"}
        // }"#;

        let field = r#"
        {
                "abc1": [2, 3, 4, 6, 7, 4, 3, 6, 7, 9],
                "abc2": ["a", "b", "c"],
                "abc3": {"0": "a", "1": "b", "2": "c"},
                "abc4": {"1": "a", "0": "b", "2": "c"},
                "abc5": {"a": 123, "b": 456, "c": 789},
                "abc6": ["abc", 123, null, 123.456],
                "abc7": {"a": "abc", "b": 456, "c": 4.4}
        }"#;

        let json: Value = serde_json::from_str(field).unwrap();

        let record_line = serde_json::to_string(&json).unwrap();

        let _data_dir = Config::get_data_dir();

        let mut rng = rand::thread_rng();
        let random_tmp_file_name = rng.gen::<i32>();

        let mut test_file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .create_new(true)
            .open(format!("./{}", random_tmp_file_name))
            .unwrap();

        // let mut test_file = File::create_new(format!("./{}", random_tmp_file_name)).unwrap();

        test_file.write(record_line.as_bytes()).unwrap();

        test_file.rewind().unwrap();

        // let mut in_file = MemFile::create(rng.gen::<i32>(), CreateOptions::new()).unwrap();
        let in_file = File::open(format!("./{}", random_tmp_file_name)).unwrap();

        // let mut buf_reader = BufReader::new(in_file);

        let mut metadata = HashMap::new();
        metadata.insert("foo".to_string(), Metadata::new().unwrap());

        AnalyseSchema::infer_json_schema(
            &mut foo,
            in_file,
            Some(1),
            &mut metadata.get_mut("foo").unwrap().fields,
        )
        .unwrap();

        AnalyseSchema::determine_field_types(&mut metadata, None, None, false);

        remove_file(Path::new(&format!("./{}", random_tmp_file_name))).unwrap();

        println!("{:?}", metadata);
        println!("{:?}", metadata.get("foo").unwrap().fields);
        println!(
            "{:?}",
            metadata.get("foo").unwrap().fields.get("abc1").unwrap()
        );
        println!(
            "{:?}",
            metadata.get("foo").unwrap().fields.get("abc2").unwrap()
        );
        // println!("{:?}", new_meta.get("foo").unwrap().fields.get("abc2").unwrap().determined_type);
        // println!("{:?}", new_meta.get("foo").unwrap().fields.get("abc2").unwrap().determined_type_values);

        assert_eq!(
            metadata
                .get("foo")
                .unwrap()
                .fields
                .get("abc1")
                .unwrap()
                .determined_type,
            "array"
        );
        assert_eq!(
            metadata
                .get("foo")
                .unwrap()
                .fields
                .get("abc1")
                .unwrap()
                .determined_type_values,
            "integer"
        );
        assert_eq!(
            metadata
                .get("foo")
                .unwrap()
                .fields
                .get("abc2")
                .unwrap()
                .determined_type,
            "array"
        );
        assert_eq!(
            metadata
                .get("foo")
                .unwrap()
                .fields
                .get("abc2")
                .unwrap()
                .determined_type_values,
            "string"
        );
        assert_eq!(
            metadata
                .get("foo")
                .unwrap()
                .fields
                .get("abc3")
                .unwrap()
                .determined_type,
            "map"
        );
        assert_eq!(
            metadata
                .get("foo")
                .unwrap()
                .fields
                .get("abc3")
                .unwrap()
                .determined_type_values,
            "string"
        );
        assert_eq!(
            metadata
                .get("foo")
                .unwrap()
                .fields
                .get("abc4")
                .unwrap()
                .determined_type,
            "map"
        );
        assert_eq!(
            metadata
                .get("foo")
                .unwrap()
                .fields
                .get("abc4")
                .unwrap()
                .determined_type_values,
            "string"
        );
        assert_eq!(
            metadata
                .get("foo")
                .unwrap()
                .fields
                .get("abc5")
                .unwrap()
                .determined_type,
            "map"
        );
        assert_eq!(
            metadata
                .get("foo")
                .unwrap()
                .fields
                .get("abc5")
                .unwrap()
                .determined_type_values,
            "integer"
        );
        assert_eq!(
            metadata
                .get("foo")
                .unwrap()
                .fields
                .get("abc6")
                .unwrap()
                .determined_type,
            "record"
        );
        assert_eq!(
            metadata
                .get("foo")
                .unwrap()
                .fields
                .get("abc7")
                .unwrap()
                .determined_type,
            "record"
        );
    }

    #[test]
    #[serial]
    fn test_discover_demoted_types() {
        let mut foo: AnalyseSchema = AnalyseSchema { i: 0 };

        // let field = r#"
        // {
        //         "boolean": [1, 0, 1, 1]
        // }"#;

        let field = r#"
        {
                "boolean": [1, 0, 1, 1],
                "boolean2": [false],
                "date": ["2022-08-08", "2022-08-09"],
                "timestamp": [1660331829, 1660331829],
                "timestamp_milli": [1660331874804, 1660331874805],
                "abc3": [0, 1, 2, 3, 4],
                "abc4": [0, 1, 2, 3]
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

        // let mut buf_reader = BufReader::new(in_file);

        let mut metadata = HashMap::new();

        let mut newMeta =
            AnalyseSchema::infer_json_schema(&mut foo, in_file, Some(1), &mut metadata).unwrap();

        AnalyseSchema::determine_field_types(&mut newMeta, None, None, false);

        remove_file(Path::new(&format!("./{}", random_tmp_file_name)));

        assert_eq!(
            newMeta
                .get("")
                .unwrap()
                .fields
                .get("boolean")
                .unwrap()
                .determined_type,
            "array"
        );
        assert_eq!(
            newMeta
                .get("")
                .unwrap()
                .fields
                .get("boolean")
                .unwrap()
                .determined_type_values,
            "boolean"
        );
        assert_eq!(
            newMeta
                .get("")
                .unwrap()
                .fields
                .get("boolean2")
                .unwrap()
                .determined_type,
            "array"
        );
        assert_eq!(
            newMeta
                .get("")
                .unwrap()
                .fields
                .get("boolean2")
                .unwrap()
                .determined_type_values,
            "boolean"
        );
        // assert_eq!(newMeta.get("").unwrap().fields.get("date").unwrap().determined_type, "array");
        // assert_eq!(newMeta.get("").unwrap().fields.get("date").unwrap().determined_type_values, "date");
        assert_eq!(
            newMeta
                .get("")
                .unwrap()
                .fields
                .get("timestamp")
                .unwrap()
                .determined_type,
            "array"
        );
        assert_eq!(
            newMeta
                .get("")
                .unwrap()
                .fields
                .get("timestamp")
                .unwrap()
                .determined_type_values,
            "integer"
        );
        assert_eq!(
            newMeta
                .get("")
                .unwrap()
                .fields
                .get("timestamp_milli")
                .unwrap()
                .determined_type,
            "array"
        );
        // assert_eq!(newMeta.get("").unwrap().fields.get("timestamp_milli").unwrap().determined_type_values, "timestamp_milli");
        assert_eq!(
            newMeta
                .get("")
                .unwrap()
                .fields
                .get("abc3")
                .unwrap()
                .determined_type,
            "array"
        );
        assert_eq!(
            newMeta
                .get("")
                .unwrap()
                .fields
                .get("abc3")
                .unwrap()
                .determined_type_values,
            "integer"
        );
        assert_eq!(
            newMeta
                .get("")
                .unwrap()
                .fields
                .get("abc4")
                .unwrap()
                .determined_type,
            "array"
        );
        assert_eq!(
            newMeta
                .get("")
                .unwrap()
                .fields
                .get("abc4")
                .unwrap()
                .determined_type_values,
            "integer"
        );
    }

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

        test_file.write(record_line.as_bytes()).unwrap();

        test_file.rewind().unwrap();

        let in_file = File::open(format!("./{}", random_tmp_file_name)).unwrap();
        // let mut in_file = MemFile::create(rng.gen::<i32>(), CreateOptions::new()).unwrap();

        let mut metadata = HashMap::new();

        let mut newMeta =
            AnalyseSchema::infer_json_schema(&mut foo, in_file, Some(1), &mut metadata).unwrap();

        AnalyseSchema::determine_field_types(&mut newMeta, None, None, false);

        remove_file(Path::new(&format!("./{}", random_tmp_file_name)));

        assert_eq!(
            newMeta
                .get("")
                .unwrap()
                .fields
                .get("sheep")
                .unwrap()
                .determined_type,
            "string"
        );
        assert_eq!(
            newMeta
                .get("")
                .unwrap()
                .fields
                .get("arable")
                .unwrap()
                .determined_type,
            "boolean"
        );

        assert_eq!(
            newMeta
                .get("")
                .unwrap()
                .fields
                .get("crank")
                .unwrap()
                .determined_type,
            "record"
        );
        assert_eq!(
            newMeta
                .get("")
                .unwrap()
                .fields
                .get("crank")
                .unwrap()
                .fields
                .get("voltage")
                .unwrap()
                .determined_type,
            "array"
        );
        assert_eq!(
            newMeta
                .get("")
                .unwrap()
                .fields
                .get("crank")
                .unwrap()
                .fields
                .get("voltage")
                .unwrap()
                .determined_type_values,
            "integer"
        );

        assert_eq!(
            newMeta
                .get("")
                .unwrap()
                .fields
                .get("crank")
                .unwrap()
                .fields
                .get("engine")
                .unwrap()
                .determined_type,
            "record"
        );
        assert_eq!(
            newMeta
                .get("")
                .unwrap()
                .fields
                .get("crank")
                .unwrap()
                .fields
                .get("engine")
                .unwrap()
                .fields
                .get("rebuild_dates")
                .unwrap()
                .determined_type,
            "array"
        );

        // println!("{:?}", newMeta.get("").unwrap().fields.get("crank_torques").unwrap());

        assert_eq!(
            newMeta
                .get("")
                .unwrap()
                .fields
                .get("crank_torques")
                .unwrap()
                .determined_type,
            "record"
        );

        assert_eq!(
            newMeta
                .get("")
                .unwrap()
                .fields
                .get("crank_torques")
                .unwrap()
                .fields
                .get("item_0")
                .unwrap()
                .determined_type,
            "array"
        );
        assert_eq!(
            newMeta
                .get("")
                .unwrap()
                .fields
                .get("crank_torques")
                .unwrap()
                .fields
                .get("item_0")
                .unwrap()
                .determined_type_values,
            "integer"
        );
        assert_eq!(
            newMeta
                .get("")
                .unwrap()
                .fields
                .get("crank_torques")
                .unwrap()
                .fields
                .get("item_1")
                .unwrap()
                .determined_type,
            "array"
        );
        assert_eq!(
            newMeta
                .get("")
                .unwrap()
                .fields
                .get("crank_torques")
                .unwrap()
                .fields
                .get("item_1")
                .unwrap()
                .determined_type_values,
            "integer"
        );

        assert_eq!(
            newMeta
                .get("")
                .unwrap()
                .fields
                .get("metadata")
                .unwrap()
                .fields
                .get("tags")
                .unwrap()
                .determined_type,
            "record"
        );
        assert_eq!(
            newMeta
                .get("")
                .unwrap()
                .fields
                .get("metadata")
                .unwrap()
                .fields
                .get("tags")
                .unwrap()
                .fields
                .get("item_0")
                .unwrap()
                .determined_type,
            "map"
        );

        // // assert_eq!(newMeta.get("").unwrap().fields.get("date").unwrap().determined_type, "array");
        // // assert_eq!(newMeta.get("").unwrap().fields.get("date").unwrap().determined_type_values, "date");
        // assert_eq!(newMeta.get("").unwrap().fields.get("timestamp").unwrap().determined_type, "array");
        // // assert_eq!(newMeta.get("").unwrap().fields.get("timestamp").unwrap().determined_type_values, "timestamp");
        // assert_eq!(newMeta.get("").unwrap().fields.get("timestamp_milli").unwrap().determined_type, "array");
        // // assert_eq!(newMeta.get("").unwrap().fields.get("timestamp_milli").unwrap().determined_type_values, "timestamp_milli");
        // assert_eq!(newMeta.get("").unwrap().fields.get("abc3").unwrap().determined_type, "array");
        // assert_eq!(newMeta.get("").unwrap().fields.get("abc3").unwrap().determined_type_values, "integer");
        // assert_eq!(newMeta.get("").unwrap().fields.get("abc4").unwrap().determined_type, "array");
        // assert_eq!(newMeta.get("").unwrap().fields.get("abc4").unwrap().determined_type_values, "integer");
    }
}
