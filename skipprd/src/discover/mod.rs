use std::any::Any;
use std::borrow::{Borrow, BorrowMut};
use std::collections::HashMap;
use std::collections::HashSet;
use std::io::Read;
use std::option::Iter;
// use std::fs::{Metadata as OtherMetadata, Metadata};
use chrono::{DateTime, TimeZone, Utc};
use icu::list::Error::Data;
use icu::plurals::rules::reference::ast::RangeListItem;
use serde_json::Value;
use crate::arr::Arr;

// use std::simd::usizex2;
use crate::helpers;
use crate::helpers::Helpers;
mod date_formats;
use crate::discover::date_formats::DateFormats;
mod filter_float;
use crate::discover::filter_float::parse_float;
mod filter_bool;
use crate::discover::filter_bool::parse_bool;
mod filter_parse_int;
use crate::discover::filter_parse_int::php_filter_parse_int;

#[derive(Default)]
#[derive(Clone)]
pub struct DateCandidate {
    check_count: i32,
    valid_count: i32,
    field: String,
    format: String
}

#[derive(Clone)]
pub struct Evolution {
    type_string: String,
    new_value: String,
    sovled: bool,
}

#[derive(Clone)]
pub struct Metadata {
    pub(crate) count: i32,
    pub(crate) types: HashMap<String, u32>,
    pub(crate) parent_type: String,
    pub(crate) fields: Box<HashMap<String, Metadata>>,
    pub(crate) date_candidate: Option<DateCandidate>,
    pub(crate) evolution: Box<HashMap<String, Evolution>>,
    pub(crate) enabled: bool,
    pub(crate) determined_type: String
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

fn get_type(value: &mut String) -> String {

    match value.parse::<i32>() {
        Ok(bool) => {
            return "integer".to_string();
        },
        Err(..) => {}
    }

    match value.parse::<i64>() {
        Ok(bool) => {
            return "long".to_string();
        },
        Err(..) => {}
    }

    let v: Value = serde_json::from_str(value).unwrap_or_default();
    match v.is_array().then_some(true) {
        Some(bool) => {
            return "array".to_string();
        },
        None => {}
    }

    match v.is_object().then_some(true) {
        Some(bool) => {
            return "array".to_string();
        },
        None => {}
    }

    match parse_bool(value) {
        Err(i32) => {
            // println!("Not float");
        }
        Ok(bool) => {
            // println!("Is float");
            return "bool".to_string();
        }

    }

    match value.parse::<String>() {
        Ok(bool) => {
            return "string".to_string();
        },
        Err(String) => {}
    }

    "unknown".to_string()

}

const min_discovery_records: i32 = 100;

impl AnalyseSchema {


    // pub fn analyse_payload(&mut self, message: &HashMap<String, String>, metadata: &mut HashMap<String, Metadata>) {
    pub fn analyse_payload(&mut self, message: &Value, metadata: &mut HashMap<String, Metadata>) {
        self.i += 1;

        let mut helpers = Helpers { clean_field_cache: Default::default() };

        for (field, value ) in message.as_object().unwrap() {
            self.init_discovered_type(metadata, field);

            let mut jsonValue: Value;

            jsonValue = value.clone();

            if value.as_str().is_some() {
                if serde_json::from_str(value.as_str().unwrap()).unwrap_or(false) {
                    if serde_json::from_str(value.as_str().unwrap()).unwrap() {
                        jsonValue = serde_json::from_str(value.as_str().unwrap()).unwrap();
                    }
                }
            }

            let field = helpers.clean_field_name(field.to_string());

            self.analyse_field(&field, &mut jsonValue, metadata);
        }
    }

    pub fn analyse_field(&self, field: &String, value: &mut Value, metadata: &mut HashMap<String, Metadata>) {

        // Build mapping/Schema
        // if metadata.get(field).unwrap().count < Config::min_discovery_records. {
        if metadata.get_mut(field).is_none() || metadata.get_mut(field).unwrap().count < min_discovery_records {

            // if value.is_string() {
                self.resolve_field_type(metadata, field, value);
            // }

        }

        if value.is_object() {
            for (sub_field, mut sub_value) in value.as_object().unwrap() {
                let sf = sub_field.as_str();
                let mut sv = sub_value.clone();
                // let mut svv: Value = serde_json::from_str(sv.unwrap()).unwrap();
                self.analyse_field(sub_field, &mut sv, metadata.get_mut(field).unwrap().fields.as_mut());
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

    pub fn resolve_field_type(&self, metadata: &mut HashMap<String, Metadata>, field: &String, value: &mut Value) -> String {

        self.init_discovered_type(metadata, field);

        let mut data_type = self.get_logical_type(field, value, metadata, true);

        if data_type == "array" && value.is_array() {
            let mut type_count = HashMap::new();

            let is_sequential = Helpers::is_sequential_array_keys(value.as_array().unwrap());

            if value.as_object().is_some() {
                for (sub_field, sub_value) in value.as_object().unwrap() {
                    let mut sv: Value = serde_json::from_str(sub_field).unwrap();
                    let logical_type = self.get_logical_type(sub_field, &mut sv, metadata, false);

                    type_count.insert(logical_type, "hit");

                    // Special handling of bools in array/map of ints
                    // [1,2,3] may discover as schema [bool, int, int] and therefore
                    // parent field resolve type as `record`.
                    // When in fact we'd want to discover schema as [int, int, int] and
                    // parent field resolve as `array`.
                    if type_count.len() == 2 {
                        if type_count.contains_key("integer") && type_count.contains_key("boolean") {
                            type_count.remove("boolean");
                        }
                    }
                }
            }

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
        }

        self.set_discovered_occurrence(metadata, field, &data_type, &mut value.to_string());

        data_type
    }


    pub fn get_logical_type(&self, field: &String, josn_value: &mut Value, metadata: &mut HashMap<String, Metadata>, allow_date: bool) -> String {

        // let value: &mut String = &mut josn_value.as_str().unwrap().to_string();
        let value: &mut String = &mut josn_value.to_string();

        let mut data_type = get_type(value);

        if data_type == "string" || data_type == "integer" || data_type == "double" {
            // String really an int?
            data_type = self.check_string_or_int(value);

            if allow_date {
                let mut valid_timestamp = false;

                if data_type == "integer" {
                    valid_timestamp = self.is_valid_timestamp(value);
                } else if data_type == "long" {
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

        if data_type == "string" && allow_date {
            // Limit number of check type attempts for data as expensive operation.

            if metadata.get_mut(field).unwrap().date_candidate.as_mut().is_some() {
                if metadata.get_mut(field).unwrap().date_candidate.as_mut().unwrap().check_count < DATE_FIELD_VALIDATION_MIN_SAMPLE {
                    if let Some(format) = self.is_valid_date(value) {
                        data_type = "date".to_string();
                        // self.set_date_field_candidate(field, metadata, &format);
                        self.increment_date_field_candidate_count(field, metadata, &format);
                    }



                    // Already hit date field check limit. Force set type if valid date field.
                } else if metadata.get_mut(field).unwrap().date_candidate.as_mut().unwrap().valid_count >= DATE_FIELD_VALIDATION_MIN_SAMPLE {
                    data_type = "date".to_string();
                }
            }
        }

        // @todo
        // if data_type != "double" && is_bool(filter_var(value, FILTER_VALIDATE_BOOLEAN, FILTER_NULL_ON_FAILURE)) {
        //     data_type = "boolean".to_string();
        // }

        if data_type == "NULL" { // most systems won't support null
            data_type = "string".to_string();
        }
        // @todo - logical interpretation based on field name

        data_type
    }

    fn check_string_or_int(&self, value: &mut String) -> String {
        let mut data_type = get_type(value);

        if value.parse::<i32>().is_ok() && (&mut value.parse::<i32>().unwrap().to_string() == value) {

            if self.is32bitSignedInt(value) {
                data_type = "integer".to_string();
            } else if self.is64bitSignedInt(value) {
                data_type = "long".to_string();
            }
        }


        return data_type;
    }

    pub fn increment_date_field_candidate_count(&self, field: &str, metadata: &mut HashMap<String, Metadata>, format: &String) {

        if metadata.get(field).unwrap().date_candidate.is_none() {

            let can: DateCandidate = DateCandidate {check_count: 1, valid_count: 0, field: field.to_string(), format: format.to_string() };

            metadata.get_mut(field)
                .unwrap()
                .date_candidate
                .insert(can);
        } else {
           metadata.get_mut(field)
                .unwrap()
                .date_candidate
                .as_mut()
               .unwrap()
               .check_count += 1;

        }
    }

    pub fn set_date_field_candidate(&self, field: &String, metadata: &mut HashMap<String, Metadata>, format: &String) {
        if metadata.get_mut(field).unwrap().date_candidate.as_mut().is_some()
            && metadata.get_mut(field).unwrap().date_candidate.as_mut().unwrap().valid_count == 0 {
            metadata.get_mut(field).unwrap().date_candidate.as_mut().unwrap().valid_count = 1;
        } else {
            metadata.get_mut(field).unwrap().date_candidate.as_mut().unwrap().valid_count += 1;
        }

        if metadata.get_mut(field).unwrap().date_candidate.as_mut().unwrap().valid_count >= DATE_FIELD_VALIDATION_MIN_SAMPLE {
            metadata.get_mut(field).unwrap().date_candidate.as_mut().unwrap().field = field.clone();

            if format != "" {
                metadata.get_mut(field).unwrap().date_candidate.as_mut().unwrap().format = format.clone();
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
        let options = [
            -9223372036854775807,
            9223372036854775807
        ];
        let mut result = false;
        for i in options.iter() {
            if value == *i {
                result = true;
            }
        }
        result
    }

    fn is_valid_timestamp(&self, timestamp: &mut String) -> bool {
        let date = Utc.timestamp(timestamp.parse::<i64>().unwrap(), 0);
        let min_date = Utc.ymd(1970, 1, 1).and_hms(0, 0, 0);
        let max_date = Utc.ymd(2040, 1, 1).and_hms(0, 0, 0);
        if date >= min_date && date <= max_date {
            return true;
        }
        return false;
    }

    fn is_valid_date(&self, value: &mut String) -> Option<String> {
        let valid_formats = [
            DateFormats::Atom,
            DateFormats::Cookie,
            DateFormats::Iso8601,
            DateFormats::Rfc822,
            DateFormats::Rfc850,
            DateFormats::Rfc1036,
            DateFormats::Rfc1123,
            DateFormats::Rfc2822,
            DateFormats::Rfc3339,
            DateFormats::Rss,
            DateFormats::W3c,
        ];

        for format in valid_formats.iter() {
            match DateTime::parse_from_str(value, format.as_str()) {
                Ok(_) => return Some(format.as_str().to_string()),
                Err(_) => continue,
            }
        }

        None
    }

    pub fn apply_evolution_factory(
        &self,
        field: &mut std::string::String,
        value: &mut String,
        evolution: String,
        data_type: &mut String,
        new_value: String,
    )  {
        match &*evolution {
            "cast" => {
                return *data_type = new_value;
            }
            "new" => {
                return *field = new_value;
            }
            "rename" => {
                return *field = new_value;
            }
            "merge" => {
                return *field = new_value;
            }
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
            let mut newMeta = Metadata {
                count: 0,
                types: HashMap::new(),
                parent_type: "".to_string(),
                fields: Box::new(Default::default()),
                date_candidate: None,
                evolution: Box::new(Default::default()),
                enabled: true,
                determined_type: "".to_string(),
            };

            metadata.insert(field.clone(), newMeta);
        }
    }

    pub fn set_discovered_occurrence(&self,
                                     array: &mut HashMap<String, Metadata>,
                                     field: &String,
                                     data_type: &String,
                                     value: &mut String,
    ) {

        if array.get(field).unwrap().types.get(&data_type.to_string()).is_none() {
            array
                .get_mut(field)
                .unwrap()
                .types
                .insert(data_type.to_string(), 1);
            array
                .get_mut(field)
                .unwrap()
                .evolution
                .insert(
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

}
