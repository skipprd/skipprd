#![allow(dead_code)]
use std::any::Any;

use std::collections::{BTreeMap, HashMap};

#[allow(unused_imports)]
use chrono::{DateTime, TimeZone, Utc};
use once_cell::sync::Lazy;

use serde_derive::{Deserialize, Serialize};

use serde_json::Value;

use crate::helpers::Helpers;
pub(crate) mod date_formats;
use crate::discover::date_formats::DateFormats;

pub mod evolution;
mod filter_float;
pub mod stats;

mod filter_bool;
use crate::discover::filter_bool::parse_bool;
mod filter_parse_int;

use crate::helpers::configuration::Config;

use crate::ingest::ingest::IngestRecord;

use crate::serdes::json::SerdeJson;

use crate::discover::evolution::Evolution;
use crate::helpers::timed_rwlock::TimedRwLock;

pub static NUM_ANALYSED_RECORDS: Lazy<TimedRwLock<u64>> =
    Lazy::new(|| TimedRwLock::new("num_analyised_records".to_string(), 0));

thread_local! {
    static LAST_SUCCESSFUL_EVOLUTION: std::cell::RefCell<HashMap<String, String>> = std::cell::RefCell::new(HashMap::new());
}

#[derive(Default, Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct DateCandidate {
    pub(crate) check_count: i32,
    pub(crate) valid_count: i32,
    pub(crate) field: String,
    pub(crate) format: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub enum DateParserKind {
    ZNoMsT,
    ZNoMsSpace,
    ZMsT,
    ZMsSpace,
    OffNoMsT,
    OffNoMsSpace,
    OffMsT,
    OffMsSpace,
    NaiveMysql,
    NaiveDateOnly,
    RFC3339,
}

#[allow(dead_code)]
pub fn discover_ingest(
    field: &str,
    value: &mut Value,
    _parent_field: Option<&str>,
    parent_data_type: Option<&str>,
    metadata: &mut HashMap<String, Metadata>,
    updated_schema: &mut String,
) -> String {
    let _foo: AnalyseSchema = AnalyseSchema { i: 0 };

    if value.is_null() || (value.is_string() && value.as_str().unwrap_or_default().is_empty()) {
        return "".to_string();
    }

    AnalyseSchema::analyse_field(&_foo, &field.to_string(), &mut value.clone(), metadata);

    // If this is an array of records, make sure repetition_count matches the array length
    if value.is_array() {
        if let Some(field_metadata) = metadata.get_mut(field) {
            if field_metadata.is_type(SkipprDataType::Array)
                && field_metadata.is_values_type(SkipprDataType::Record)
            {
                let array_length = value.as_array().unwrap().len() as i32;
                if array_length > field_metadata.repetition_count {
                    field_metadata.repetition_count = array_length;
                }
            }
        }
    }

    let flatten = Config::get_transform_flatten_events();

    AnalyseSchema::determine_field_types(metadata, parent_data_type, flatten);

    let discoverd_data_type = metadata.get(field).unwrap().determined_type.clone();

    // println!(
    //     "Discovered new field: '{}' of type: '{}'",
    //     field, discoverd_data_type
    // );

    *updated_schema = "yes".to_string();

    // Derive parser kind once per field to avoid repeated string scans in ingest
    if discoverd_data_type == "date" {
        if let Some(meta) = metadata.get_mut(field) {
            meta.date_parser_kind =
                Some(AnalyseSchema::derive_date_parser_kind(value, meta.timezone));
        }
    }

    discoverd_data_type
}

/**
 * Reduced metadata for output schema, only includes fields that are required to generate an output schema
 *
 * The main motivation for this, is to be very clear that the output metadata is generated from the input metadata.
 * This is currently only done via flatten_metadata(), concevable this may evolve.
 */
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct OutputMetadata {
    pub(crate) out_field_name: String,
    pub(crate) determined_type: String,
    pub(crate) determined_type_values: String,
    pub(crate) fields: Box<HashMap<String, OutputMetadata>>,
}

impl OutputMetadata {
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self {
            out_field_name: "".to_string(),
            determined_type: "".to_string(),
            determined_type_values: "".to_string(),
            fields: Box::new(HashMap::new()),
        }
    }

    /**
     * Recusively convert Metadata to OutputMetadata
     */
    pub fn from_metadata(metadata: &Metadata) -> OutputMetadata {
        let mut output_metadata = OutputMetadata::new();

        output_metadata.out_field_name = metadata.out_field_name.clone();
        output_metadata.determined_type = metadata.determined_type.clone();
        output_metadata.determined_type_values = metadata.determined_type_values.clone();

        let mut fields = HashMap::new();
        for (field, metadata) in metadata.fields.iter() {
            fields.insert(field.clone(), OutputMetadata::from_metadata(metadata));
        }
        let fields_outer: Box<HashMap<String, OutputMetadata>> = Box::new(fields);

        output_metadata.fields = fields_outer;

        output_metadata
    }

    /**
     * Recusively convert OutputMetadata to Metadata while flattening the fields
     */
    pub fn from_flatterened_metadata(metadata: &Metadata) -> OutputMetadata {
        let mut flatterened_metadata: OutputMetadata = OutputMetadata::new();

        Metadata::flatten_metadata(metadata, &mut flatterened_metadata);

        flatterened_metadata
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct PipelineMetadata {
    pub name: String,
    pub metadata: HashMap<String, Metadata>,
    pub sql: Option<Vec<String>>,
    pub enabled: bool,
    #[serde(default)]
    pub flattened: bool,
}

impl crate::discover::PipelineMetadata {
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        let pipeline_name = Config::get_pipeline_name();
        let flatten = Config::truth_value(
            &Config::get_transform_config()
                .flatten_events
                .or(Some("no".to_string()))
                .unwrap(),
        );

        Self {
            name: pipeline_name,
            metadata: HashMap::new(),
            sql: None,
            enabled: true,
            flattened: flatten,
        }
    }

    pub fn from_metadata(metadata: HashMap<String, Metadata>) -> Result<Self, bool> {
        let pipeline_name = Config::get_pipeline_name();
        let flatten = Config::truth_value(
            &Config::get_transform_config()
                .flatten_events
                .or(Some("no".to_string()))
                .unwrap(),
        );

        Ok(Self {
            name: pipeline_name,
            metadata: metadata,
            sql: None,
            enabled: true,
            flattened: flatten,
        })
    }

    pub fn append_sql(&mut self, sql_str: String) {
        let sql = self.sql.as_mut();
        match sql {
            Some(pipeline_sql) => {
                pipeline_sql.push(sql_str);
            }
            None => {
                self.sql = Some(vec![sql_str]);
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Metadata {
    pub(crate) count: i32,
    pub(crate) types: HashMap<String, u32>,
    pub(crate) parent_type: String,
    pub(crate) fields: Box<HashMap<String, Metadata>>,
    pub(crate) date_candidate: Option<DateCandidate>,
    pub(crate) date_parser_kind: Option<DateParserKind>,
    pub(crate) timezone: bool,
    pub(crate) evolution: Box<HashMap<String, Evolution>>,
    pub(crate) enabled: bool,
    pub(crate) out_field_name: String,
    pub(crate) determined_type: String,
    pub(crate) determined_type_values: String,
    pub(crate) repetition_count: i32, // New field to track array repetition count
}

impl Metadata {
    #[inline]
    #[must_use]
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            count: 0,
            types: Default::default(),
            parent_type: "".to_string(),
            fields: Box::new(Default::default()),
            date_candidate: None,
            date_parser_kind: None,
            timezone: false,
            evolution: Box::new(Default::default()),
            enabled: true,
            out_field_name: "".to_string(),
            determined_type: "".to_string(),
            determined_type_values: "".to_string(),
            repetition_count: 5,
        })
    }

    /// Gets the data type as a SkipprDataType enum
    pub fn data_type(&self) -> SkipprDataType {
        SkipprDataType::from_str(&self.determined_type)
    }

    /// Sets the data type using a SkipprDataType enum
    pub fn set_data_type(&mut self, data_type: SkipprDataType) {
        self.determined_type = data_type.as_str().to_string();
    }

    /// Gets the values data type as a SkipprDataType enum
    pub fn values_data_type(&self) -> SkipprDataType {
        SkipprDataType::from_str(&self.determined_type_values)
    }

    /// Sets the values data type using a SkipprDataType enum
    pub fn set_values_data_type(&mut self, data_type: SkipprDataType) {
        self.determined_type_values = data_type.as_str().to_string();
    }

    /// Check if the data type matches a specific type
    pub fn is_type(&self, data_type: SkipprDataType) -> bool {
        self.data_type() == data_type
    }

    /// Check if the values data type matches a specific type
    pub fn is_values_type(&self, data_type: SkipprDataType) -> bool {
        self.values_data_type() == data_type
    }

    pub fn flatten_metadata(metadata: &Metadata, flattened: &mut OutputMetadata) {
        Self::_flatten_metadata(metadata, flattened, "".to_string());
    }

    fn _flatten_metadata(metadata: &Metadata, flattened: &mut OutputMetadata, field_path: String) {
        for (_key, val) in metadata.fields.iter() {
            // Special handling for array elements
            if val.is_type(SkipprDataType::Array) && val.fields.contains_key("0") {
                let array_template = val.fields.get("0").unwrap();
                let repetition_count = val.repetition_count; // Use repetition_count instead of count

                // Process array elements (fields under "0" key)
                for i in 0..repetition_count {
                    // Create base path for this array element
                    let element_path = if field_path.is_empty() {
                        if metadata.out_field_name.is_empty() {
                            format!("{}_{}", val.out_field_name, i)
                        } else {
                            format!("{}_{}_{}", metadata.out_field_name, val.out_field_name, i)
                        }
                    } else {
                        format!("{}_{}_{}", field_path, val.out_field_name, i)
                    };

                    // Process each field in the array template
                    for (_sub_key, sub_val) in array_template.fields.iter() {
                        // Create the field path for this array element's field
                        let field_element_path =
                            format!("{}_{}", element_path, sub_val.out_field_name);

                        if sub_val.determined_type == "record"
                            || sub_val.determined_type == "map"
                            || sub_val.determined_type == "array"
                        {
                            // Recursively process complex types
                            let mut temp_metadata = sub_val.clone();
                            temp_metadata.out_field_name = sub_val.out_field_name.clone();
                            Self::_flatten_metadata(
                                &temp_metadata,
                                flattened,
                                element_path.clone(),
                            );
                        } else {
                            // Add primitive type to flattened fields
                            let mut el = OutputMetadata::new();
                            el.out_field_name = field_element_path.clone();
                            el.determined_type = sub_val.determined_type.clone();
                            el.determined_type_values = sub_val.determined_type_values.clone();

                            flattened.fields.insert(field_element_path.clone(), el);
                        }
                    }
                }
            } else if val.determined_type == "array" && val.fields.is_empty() {
                // Handle primitive arrays that don't have a '0' field
                // These are arrays of primitive types like double, int, etc.
                let new_field_path = if field_path.is_empty() {
                    if metadata.out_field_name.is_empty() {
                        val.out_field_name.clone()
                    } else {
                        format!("{}_{}", metadata.out_field_name, val.out_field_name)
                    }
                } else {
                    format!("{}_{}", field_path, val.out_field_name)
                };

                // Add the primitive array to the flattened fields
                let mut el = OutputMetadata::new();
                el.out_field_name = new_field_path.clone();
                el.determined_type = val.determined_type.clone();
                el.determined_type_values = val.determined_type_values.clone();

                flattened.fields.insert(new_field_path.clone(), el);
            } else {
                // Standard path for non-array fields
                let new_field_path = if field_path.is_empty() {
                    if metadata.out_field_name.is_empty() {
                        val.out_field_name.clone()
                    } else {
                        format!("{}_{}", metadata.out_field_name, val.out_field_name)
                    }
                } else {
                    format!("{}_{}", field_path, val.out_field_name)
                };

                if val.determined_type == "record"
                    || val.determined_type == "map"
                    || val.determined_type == "array"
                {
                    Self::_flatten_metadata(val, flattened, new_field_path);
                } else {
                    let mut el = OutputMetadata::new();
                    el.out_field_name = new_field_path.clone();
                    el.determined_type = val.determined_type.clone();
                    el.determined_type_values = val.determined_type_values.clone();

                    flattened.fields.insert(new_field_path.clone(), el);
                }
            }
        }
    }

    /**
     * Get the field name to use in the output schema
     * @deprecated - this looks dumb, we pass the metadata and search for a property that we could have accessed directly.
     *             - Unless we need to add conditions in the future, this is a waste of time.
     * @param {string} field
     * @returns {string}
     */
    pub fn get_field_out_field_name(metadata: &HashMap<String, Metadata>, field: &str) -> String {
        use std::cell::RefCell;
        use std::collections::HashMap;

        // Thread-local cache for field name transformations
        thread_local! {
            static FIELD_NAME_CACHE: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
        }

        // Check cache first
        let cached_name = FIELD_NAME_CACHE.with(|cache| cache.borrow().get(field).cloned());

        if let Some(cached) = cached_name {
            return cached;
        }

        // Cache miss, perform the lookup
        let out_field_name = match metadata.get(field) {
            Some(metadata) => {
                if !metadata.out_field_name.is_empty() {
                    metadata.out_field_name.clone()
                } else {
                    field.to_string()
                }
            }
            None => field.to_string(),
        };

        // Store result in cache
        FIELD_NAME_CACHE.with(|cache| {
            let mut cache_ref = cache.borrow_mut();
            cache_ref.insert(field.to_string(), out_field_name.clone());
        });

        out_field_name
    }

    // Get nested metadata from field notation
    // Supports both flat and dot notation, e.g. `foo.bar` and `foo_bar`
    pub fn get_nested_metadata_from_field_notation<'a>(
        metadata: &'a mut Metadata,
        field_str: &str,
    ) -> Option<&'a mut Metadata> {
        if field_str.contains('.') {
            let mut fields: Vec<&str> = field_str.split('.').collect();
            return Metadata::get_nested_metadata_from_dot_notation(metadata, &mut fields);
        }

        Metadata::get_nested_metadata_from_flat_notation(metadata, field_str)
    }

    fn get_nested_metadata_from_flat_notation<'a>(
        metadata: &'a mut Metadata,
        field_str: &str,
    ) -> Option<&'a mut Metadata> {
        if metadata.out_field_name == field_str {
            return Some(metadata);
        }

        for nested_metadata in metadata.fields.values_mut() {
            if let Some(found_metadata) =
                Metadata::get_nested_metadata_from_flat_notation(nested_metadata, field_str)
            {
                return Some(found_metadata);
            }
        }

        None
    }

    fn get_nested_metadata_from_dot_notation<'a>(
        metadata: &'a mut Metadata,
        fields: &mut Vec<&str>,
    ) -> Option<&'a mut Metadata> {
        let mut current_metadata = metadata;
        // remove and return first element from fields
        let field = fields.remove(0);

        if let Some(next_metadata) = current_metadata.fields.get_mut(field) {
            if fields.len() == 0 {
                return Some(next_metadata);
            }
            current_metadata =
                match Metadata::get_nested_metadata_from_dot_notation(next_metadata, fields) {
                    Some(found_metadata) => found_metadata,
                    None => return None,
                }
        } else {
            return None;
        }

        Some(current_metadata)
    }

    pub fn remove_nested_metadata_from_dot_notation<'a>(
        metadata: &'a mut Metadata,
        field_str: &str,
    ) -> Option<&'a mut Metadata> {
        let mut fields: Vec<&str> = Vec::new();

        if field_str.contains('.') {
            fields = field_str.split('.').collect();
        } else {
            fields.push(field_str);
        }

        let current_metadata = metadata;

        let field = fields.remove(0);

        if current_metadata.fields.get_mut(field).is_some() {
            if fields.len() == 0 {
                let _last = current_metadata.fields.remove(field);
                return None;
            } else {
                let next_metadata = current_metadata.fields.get_mut(field).unwrap();
                Metadata::remove_nested_metadata_from_dot_notation(
                    next_metadata,
                    fields.join(".").as_str(),
                )
            }
            // current_metadata = match Metadata::remove_nested_metadata_from_dot_notation(next_metadata, fields.join(".").as_str()) {
            // Some(found_metadata) => return found_metadata,
            // None => return None,
            // }
        } else {
            return None;
        }
    }

    /**
     * Look up a metadata entry by its output field name
     * This is useful when we need to find the original metadata for a transformed field name
     * @param {HashMap<String, Metadata>} metadata
     * @param {String} out_field_name
     * @returns {Option<&Metadata>}
     */
    pub fn get_metadata_by_out_field_name<'a>(
        metadata: &'a HashMap<String, Metadata>,
        out_field_name: &str,
    ) -> Option<(&'a String, &'a Metadata)> {
        // First try a direct match with the output field name
        for (key, meta) in metadata.iter() {
            if meta.out_field_name == out_field_name {
                return Some((key, meta));
            }
        }

        None
    }
}

#[derive(Debug, Copy, Clone)]
pub struct AnalyseSchema {
    #[allow(dead_code)]
    pub i: i32,
    // pub discovered_field_occurrence: HashMap<String, i32>,
    // pub continue_: HashSet<String>,
    // pub data_type_masks: HashMap<String, String>,
    // pub data_type_drops: HashMap<String, String>,
    // pub data_type_casts: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SkipprTypes {
    String,
    Integer,
    Long,
    Double,
    Boolean,
    Date,
    Timestamp,
    TimestampMilli,
    Array,
    Map,
    Record,
    Null,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipprDataType {
    Record,
    Map,
    Array,
    Date,
    String,
    Long,
    Integer,
    Double,
    Boolean,
    TimestampMilli,
    Timestamp,
    Null,
    Unknown,
}

impl SkipprDataType {
    /// Convert a string data type to the corresponding enum variant
    pub fn from_str(data_type: &str) -> Self {
        match data_type {
            "record" => SkipprDataType::Record,
            "map" => SkipprDataType::Map,
            "array" => SkipprDataType::Array,
            "date" => SkipprDataType::Date,
            "string" => SkipprDataType::String,
            "long" => SkipprDataType::Long,
            "int" | "integer" => SkipprDataType::Integer,
            "double" => SkipprDataType::Double,
            "boolean" => SkipprDataType::Boolean,
            "timestamp_milli" => SkipprDataType::TimestampMilli,
            "timestamp" => SkipprDataType::Timestamp,
            "null" => SkipprDataType::Null,
            _ => SkipprDataType::Unknown,
        }
    }

    /// Convert enum variant to string representation
    pub fn as_str(&self) -> &'static str {
        match self {
            SkipprDataType::Record => "record",
            SkipprDataType::Map => "map",
            SkipprDataType::Array => "array",
            SkipprDataType::Date => "date",
            SkipprDataType::String => "string",
            SkipprDataType::Long => "long",
            SkipprDataType::Integer => "integer",
            SkipprDataType::Double => "double",
            SkipprDataType::Boolean => "boolean",
            SkipprDataType::TimestampMilli => "timestamp_milli",
            SkipprDataType::Timestamp => "timestamp",
            SkipprDataType::Null => "null",
            SkipprDataType::Unknown => "unknown",
        }
    }

    /// Convert SkipprDataType to SkipprTypes
    pub fn to_skippr_type(&self) -> Option<SkipprTypes> {
        match self {
            SkipprDataType::Record => Some(SkipprTypes::Record),
            SkipprDataType::Map => Some(SkipprTypes::Map),
            SkipprDataType::Array => Some(SkipprTypes::Array),
            SkipprDataType::Date => Some(SkipprTypes::Date),
            SkipprDataType::String => Some(SkipprTypes::String),
            SkipprDataType::Long => Some(SkipprTypes::Long),
            SkipprDataType::Integer => Some(SkipprTypes::Integer),
            SkipprDataType::Double => Some(SkipprTypes::Double),
            SkipprDataType::Boolean => Some(SkipprTypes::Boolean),
            SkipprDataType::TimestampMilli => Some(SkipprTypes::TimestampMilli),
            SkipprDataType::Timestamp => Some(SkipprTypes::Timestamp),
            SkipprDataType::Null => Some(SkipprTypes::Null),
            SkipprDataType::Unknown => None,
        }
    }

    /// Convert from SkipprTypes to SkipprDataType
    pub fn from_skippr_type(skippr_type: &SkipprTypes) -> Self {
        match skippr_type {
            SkipprTypes::Record => SkipprDataType::Record,
            SkipprTypes::Map => SkipprDataType::Map,
            SkipprTypes::Array => SkipprDataType::Array,
            SkipprTypes::Date => SkipprDataType::Date,
            SkipprTypes::String => SkipprDataType::String,
            SkipprTypes::Long => SkipprDataType::Long,
            SkipprTypes::Integer => SkipprDataType::Integer,
            SkipprTypes::Double => SkipprDataType::Double,
            SkipprTypes::Boolean => SkipprDataType::Boolean,
            SkipprTypes::TimestampMilli => SkipprDataType::TimestampMilli,
            SkipprTypes::Timestamp => SkipprDataType::Timestamp,
            SkipprTypes::Null => SkipprDataType::Null,
        }
    }
}

// to string
impl SkipprTypes {
    pub(crate) fn to_string(&self) -> String {
        match self {
            SkipprTypes::String => "string".to_string(),
            SkipprTypes::Integer => "integer".to_string(),
            SkipprTypes::Long => "long".to_string(),
            SkipprTypes::Double => "double".to_string(),
            SkipprTypes::Boolean => "boolean".to_string(),
            SkipprTypes::Date => "date".to_string(),
            SkipprTypes::Timestamp => "timestamp".to_string(),
            SkipprTypes::TimestampMilli => "timestamp_milli".to_string(),
            SkipprTypes::Array => "array".to_string(),
            SkipprTypes::Map => "map".to_string(),
            SkipprTypes::Record => "record".to_string(),
            SkipprTypes::Null => "null".to_string(),
        }
    }

    pub(crate) fn from_string(s: &str) -> Option<SkipprTypes> {
        match s.to_lowercase().as_str() {
            "string" => Some(SkipprTypes::String),
            "integer" => Some(SkipprTypes::Integer),
            "int" => Some(SkipprTypes::Integer),
            "long" => Some(SkipprTypes::Long),
            "bigint" => Some(SkipprTypes::Long),
            "double" => Some(SkipprTypes::Double),
            "boolean" => Some(SkipprTypes::Boolean),
            "date" => Some(SkipprTypes::Date),
            "timestamp" => Some(SkipprTypes::Timestamp),
            "timestamp_milli" => Some(SkipprTypes::TimestampMilli),
            "array" => Some(SkipprTypes::Array),
            "map" => Some(SkipprTypes::Map),
            "record" => Some(SkipprTypes::Record),
            "struct" => Some(SkipprTypes::Record),
            "null" => Some(SkipprTypes::Null),
            _ => None,
        }
    }
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

    match &value.parse::<f32>() {
        Ok(_bool) => {
            return "double".to_string();
        }
        Err(_) => {
            let timmed_value = value.trim_matches('"');
            let json_value: Result<f32, _> = serde_json::from_str(timmed_value);
            match json_value {
                Ok(_) => {
                    return "double".to_string();
                }
                Err(_) => {}
            }
        }
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

    match value.parse::<bool>() {
        Ok(_bool) => {
            return "boolean".to_string();
        }
        Err(_string) => {}
    }

    match value.parse::<String>() {
        Ok(_bool) => {
            return "string".to_string();
        }
        Err(_string) => {}
    }

    "unknown".to_string()
}

const MIN_DISCOVERY_RECORDS: i32 = 100;

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
        &self,
        str: &mut String,
        max_read_records: Option<u64>,
        metadata: &mut HashMap<std::string::String, Metadata>,
    ) -> u64 {
        // self.infer_json_schema_from_iterator(ValueIter::new(reader, max_read_records))
        let counts = self.infer_json_schema_from_iterator(str, metadata, max_read_records);
        counts
    }

    // pub fn infer_json_schema_from_iterator<I>(&mut self, value_iter: I) -> Result<HashMap<std::string::String, Metadata>, ArrowError>
    //     where
    //         I: Iterator<Item = Result<Value, ArrowError>>,
    // {
    pub fn infer_json_schema_from_iterator(
        &self,
        str: &mut String,
        metadata: &mut HashMap<std::string::String, Metadata>,
        max_read_records: Option<u64>,
    ) -> u64 {
        let mut parse_namespace_cache: HashMap<String, String> = HashMap::new();

        let mut counts = 0;

        let mut _skpr_namespace: String = "".to_string();
        let pipeline_name = Config::get_pipeline_name();

        let _flatten = Config::truth_value(
            &Config::get_transform_config()
                .flatten_events
                .or(Some("no".to_string()))
                .unwrap(),
        );

        let mut records: Vec<Value> = SerdeJson::deserialize(str);

        let entity_field_dot = match Config::get_transform_config().record_field_path {
            Some(ref field) => field.clone(),
            None => "".to_string(),
        };

        if !entity_field_dot.is_empty() {
            records = match Helpers::process_values(&records, &entity_field_dot) {
                Some(records) => records,
                None => Vec::new(),
            };
        }

        let mut unwrapped_records: Vec<Value> = Vec::new();

        for record in records {
            match record.as_object() {
                Some(_v) => unwrapped_records.push(record),
                None => {
                    match record.as_array() {
                        Some(v) => {
                            for item in v {
                                // println!("Item: {}", item);
                                unwrapped_records.push(item.clone());
                            }
                        }
                        None => {
                            continue;
                        }
                    }
                }
            };
        }

        // println!("Unwrapped records: {:?}", unwrapped_records.len());

        for v in unwrapped_records {
            _skpr_namespace = Helpers::parse_namespace_field(
                &v,
                pipeline_name.clone(),
                &mut parse_namespace_cache,
            );

            if !metadata.contains_key(&_skpr_namespace) {
                metadata.insert(_skpr_namespace.clone(), Metadata::new().unwrap());
            }

            if counts >= max_read_records.unwrap_or(1000) {
                return counts;
            }

            if v.is_null() {
                continue;
            }

            match v.type_id() {
                _value => {
                    let mut ingest_record = IngestRecord {
                        source_namespace: "".to_string(),
                        source_partition: "".to_string(),
                        skpr_event_ts: 0,
                        skpr_namespace: _skpr_namespace.clone(),
                        skpr_partition: "".to_string(),
                        record: v,
                    };

                    counts += 1;

                    // println!("Analyzing count: {}, record: {}", counts, ingest_record.record);
                    // println!("Analyzing count: {}", counts);

                    self.analyse_payload(
                        &mut ingest_record.record,
                        &mut metadata
                            .get_mut(&ingest_record.skpr_namespace)
                            .unwrap()
                            .fields,
                    );
                } // Remove this unreachable pattern
            };
        }

        counts
    }

    // pub fn analyse_payload(&mut self, message: &HashMap<String, String>, metadata: &mut HashMap<String, Metadata>) {
    pub fn analyse_payload(&self, message: &Value, metadata: &mut HashMap<String, Metadata>) {
        // let mut helpers = Helpers { clean_field_cache: Default::default() };

        for (field, value) in message.as_object().unwrap() {
            // let field = Helpers::clean_field_name(field.to_string());

            self.init_discovered_type(metadata, &field);

            let mut json_value: Value;

            json_value = value.clone();

            if value.as_str().is_some()
                && serde_json::from_str(value.as_str().unwrap()).unwrap_or(false)
                && serde_json::from_str(value.as_str().unwrap()).unwrap()
            {
                json_value = serde_json::from_str(value.as_str().unwrap()).unwrap();
            }

            self.analyse_field(&field, &mut json_value, metadata);
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
            || metadata.get_mut(field).unwrap().count < MIN_DISCOVERY_RECORDS
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
            // Update repetition_count only for arrays of records
            if let Some(field_metadata) = metadata.get_mut(field) {
                if field_metadata.determined_type == "array"
                    && field_metadata.determined_type_values == "record"
                {
                    let array_length = value.as_array().unwrap().len() as i32;
                    if array_length > field_metadata.repetition_count {
                        field_metadata.repetition_count = array_length;
                    }
                }
            }

            // let mut i = 0;
            for sub_value in value.as_array().unwrap() {
                let mut sv = sub_value.clone();
                self.analyse_field(
                    // &Helpers::clean_field_name(i.to_string()),
                    &0.to_string(),
                    &mut sv,
                    metadata
                        .get_mut(&field.to_string())
                        .unwrap()
                        .fields
                        .as_mut(),
                );
                // i += 1;
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
                    // if type_count.len() == 2
                    //     && type_count.contains_key("integer")
                    //     && type_count.contains_key("boolean")
                    // {
                    //     type_count.remove("boolean");
                    // }
                }
            }

            if value.as_array().is_some() {
                // println!("{:?} as array", field);

                let mut _i = 0;

                for sub_value in value.as_array().unwrap() {
                    let mut sv: Value = serde_json::from_str(&sub_value.to_string()).unwrap();

                    let mut _logical_type = "".to_string();

                    _logical_type = self.resolve_field_type(
                        metadata
                            .get_mut(&field.to_string())
                            .unwrap()
                            .fields
                            .as_mut(),
                        &0.to_string(),
                        &mut sv,
                    );

                    // if logical_type != "record" {
                    //     logical_type = self.get_logical_type(&i.to_string(), &mut sv, metadata, false);
                    // }

                    _i += 1;
                    // if (field == "trip") {
                    // println!("#### trip sub_field NAME {:?}", sub_field);
                    // println!("#### trip sub field value {}", sv);
                    // println!("#### trip sub field value type {}", logical_type);
                    // }

                    type_count.insert(_logical_type, "hit");

                    // Special handling of bools in array/map of ints
                    // [1,2,3] may discover as schema [bool, int, int] and therefore
                    // parent field resolve type as `record`.
                    // When in fact we'd want to discover schema as [int, int, int] and
                    // parent field resolve as `array`.
                    // if type_count.len() == 2
                    //     && type_count.contains_key("integer")
                    //     && type_count.contains_key("boolean")
                    // {
                    //     type_count.remove("boolean");
                    // }
                }
            }

            // let demoted_types = vec!["boolean".to_string(), "date".to_string(), "timestamp".to_string(), "timestamp_milli".to_string()];

            // if type_count.len() > 1 {
            //     for (type_1, count) in type_count.clone() {
            //         if demoted_types.contains(&type_1) {
            //             type_count.remove(&type_1);
            //         }
            //     }
            // }

            if type_count.contains_key("record") {
                // If the value is actually an array (JSON array) but contains records,
                // it should be identified as an array of records, not just a record
                if value.is_array() {
                    data_type = "array".to_string();
                } else {
                    data_type = "record".to_string();
                }
            } else if is_sequential {
                // array of sequential int keys is an avro array
                data_type = "array".to_string();
            } else
            // if type_count.len() > 1
            {
                data_type = "record".to_string();
            }

            // Set the initial repetition_count for arrays of records
            if data_type == "array" && value.is_array() {
                // Check if this is an array of records by examining the first element
                let array_values = value.as_array().unwrap();
                if !array_values.is_empty() && array_values[0].is_object() {
                    let array_length = array_values.len() as i32;
                    if let Some(field_metadata) = metadata.get_mut(field) {
                        field_metadata.determined_type_values = "record".to_string();
                        field_metadata.repetition_count =
                            array_length.max(field_metadata.repetition_count);
                    }
                }
            }

            // NOTE:
            //  - maps sometimes become records, any previously loaded data will be invalid.
            //       which has to be handled by evolution. Resulting in the original map field (e.g. `foo`)
            //       and a new field `foo_record`. The user might reasonably expect to add fields and already conside the map a record.
            //  - Also, maps seemed to make ingesting slower with nested data... but not when flattening data.
            //  - Also, I'm not sure how to query a map in datafusion. Athena is fine. I just don't have confidence the complexity was worth it.
            //  - At the time of writing, Maps are fully supported however and the intention is to maintain that support so users can opt-in to maps.
            // else if !is_sequential {
            // associative array is an avro map
            // data_type = "map".to_string();
            // }
            // Array of Arrays? Use a Record for the parent.
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

        if data_type == *"string" || data_type == *"integer" {
            // String really an int?
            data_type = self.check_string_or_int(value);

            if allow_date {
                let mut valid_timestamp = false;

                if data_type == *"integer" {
                    valid_timestamp = self.is_valid_timestamp(value);
                    if valid_timestamp {
                        data_type = "timestamp".to_string();
                    }
                } else if data_type == *"long" {
                    valid_timestamp = self.is_valid_timestamp(value);
                    if valid_timestamp {
                        data_type = "timestamp_milli".to_string();
                    }
                }

                if valid_timestamp {
                    // self.set_date_field_candidate(field, metadata, &"".to_string());
                    self.increment_date_field_candidate_count(field, metadata, &"".to_string());
                }
            }

            // if self.is_float(value) {
            //     data_type = "double".to_string();
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

                if let Some(format) = AnalyseSchema::is_valid_date(value_str) {
                    data_type = "date".to_string();
                    // self.set_date_field_candidate(field, metadata, &format);
                    self.increment_date_field_candidate_count(field, metadata, &format.to_string());
                    // If the string clearly includes a timezone indicator, mark metadata timezone as present
                    if value_str.contains('Z')
                        || value_str.rfind('+').is_some()
                        || value_str.rfind('-').map(|i| i > 10).unwrap_or(false)
                    {
                        if let Some(meta) = metadata.get_mut(field) {
                            meta.timezone = true;
                        }
                    }
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

        // @todo - we don't support int bool anymore
        if data_type == *"integer" || data_type == *"string" {
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

        // check is_32_bit_signed_int or is_64_bit_signed_int

        if data_type == *"string" {
            if self.is_32_bit_signed_int(value) {
                data_type = "integer".to_string();
            } else if self.is_64_bit_signed_int(value) {
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

            let _ = metadata.get_mut(field).unwrap().date_candidate.insert(can);
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

    #[allow(dead_code)]
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

    #[allow(dead_code)]
    pub fn is_float(&self, test: &mut String) -> bool {
        let type_string = test.parse::<f64>();
        match type_string {
            Ok(_) => true,
            Err(_) => false,
        }
    }

    fn is_32_bit_signed_int(&self, value: &mut String) -> bool {
        // let value = value as i32;
        const MIN: i32 = -2147483648;
        const MAX: i32 = 2147483647;

        let mut result = false;
        let val = match value.parse::<i32>() {
            Ok(val) => val,
            Err(_) => {
                return false;
            }
        };

        if val >= MIN && val <= MAX {
            result = true;
        }

        result
    }

    fn is_64_bit_signed_int(&self, value: &mut String) -> bool {
        let value = match value.parse::<i64>() {
            Ok(val) => val,
            Err(_) => {
                return false;
            }
        };

        const MIN: i64 = -9223372036854775808;
        const MAX: i64 = 9223372036854775807;

        let mut result = false;

        if value >= MIN && value <= MAX {
            result = true;
        }

        result
    }

    pub fn is_valid_timestamp_milli(&self, timestamp: &mut String) -> bool {
        match timestamp.parse::<i64>() {
            Ok(millis) => {
                // Add a heuristic check for millisecond timestamps
                // Only consider values with 13 digits (milliseconds since epoch)
                let timestamp_len = timestamp.len();
                if timestamp_len != 13 {
                    return false;
                }

                // Timestamps earlier than 2010-01-01 are less likely to be timestamps
                // and more likely to be just large integers
                let min_timestamp_ms = 1262304000000; // 2010-01-01 00:00:00 UTC in milliseconds
                if millis < min_timestamp_ms {
                    return false;
                }

                let date = Utc.timestamp_millis_opt(millis).unwrap();
                let min_date = Utc.with_ymd_and_hms(1970, 1, 1, 0, 0, 0).unwrap();
                let max_date = Utc.with_ymd_and_hms(2040, 1, 1, 0, 0, 0).unwrap();
                if date >= min_date && date <= max_date {
                    return true;
                }
                false
            }
            Err(_) => false,
        }
    }

    pub fn is_valid_timestamp(&self, timestamp: &mut String) -> bool {
        match timestamp.parse::<i64>() {
            Ok(seconds) => {
                // Add a heuristic check for timestamps
                // Only treat values after 2010-01-01 and with reasonable length as timestamps (10-11 digits for seconds)
                let timestamp_len = timestamp.len();
                if timestamp_len < 10 || timestamp_len > 11 {
                    return false;
                }

                // Timestamps earlier than 2010-01-01 are less likely to be timestamps
                // and more likely to be just large integers
                let min_timestamp = 1262304000; // 2010-01-01 00:00:00 UTC
                if seconds < min_timestamp {
                    return false;
                }

                match DateTime::from_timestamp(seconds, 0) {
                    Some(date) => {
                        // Use Unix epoch as minimum date instead of 2001
                        let min_date = Utc.with_ymd_and_hms(1970, 1, 1, 0, 0, 0).unwrap();
                        let max_date = Utc.with_ymd_and_hms(2040, 1, 1, 0, 0, 0).unwrap();
                        if date >= min_date && date < max_date {
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

    pub(crate) fn is_valid_date(value: &str) -> Option<&str> {
        // First check for ISO8601 format with milliseconds specifically for format tests
        if value.contains('T') && value.contains('Z') && value.contains('.') {
            let format = DateFormats::Iso8601;
            if let Ok(_) = Helpers::parse_date_from_string(value, format.as_str()) {
                return Some(format.name());
            }
        }

        // Then check for ISO8601 format without milliseconds
        if value.contains('T') && value.contains('Z') && !value.contains('.') {
            let format = DateFormats::Iso8601_2;
            if let Ok(_) = Helpers::parse_date_from_string(value, format.as_str()) {
                return Some(format.name());
            }
        }
        // Fractional seconds with Z
        if value.contains('T') && value.contains('Z') && value.contains('.') {
            let format = DateFormats::Iso8601;
            if let Ok(_) = Helpers::parse_date_from_string(value, format.as_str()) {
                return Some(format.name());
            }
        }
        // Check space separated with Z
        if value.contains(' ') && value.ends_with('Z') {
            let format = DateFormats::Iso8601SpaceZ;
            if let Ok(_) = Helpers::parse_date_from_string(value, format.as_str()) {
                return Some(format.name());
            }
        }
        // Fractional seconds space separated Z (e.g., 2025-05-29 07:07:00.123Z)
        if value.contains(' ') && value.ends_with('Z') && value.contains('.') {
            // Not explicitly listed, but chrono accepts with %f
            let fmt = "%Y-%m-%d %H:%M:%S.%fZ";
            if let Ok(_) = Helpers::parse_date_from_string(value, fmt) {
                return Some("Iso8601_SpaceZ");
            }
        }
        // Check space separated with offset
        if value.contains(' ')
            && (value.contains('+') || value.rfind('-').map(|i| i > 10).unwrap_or(false))
        {
            let format = DateFormats::Iso8601SpaceOffset;
            if let Ok(_) = Helpers::parse_date_from_string(value, format.as_str()) {
                return Some(format.name());
            }
        }
        // Fractional seconds with offset
        if value.contains('T')
            && (value.contains('+') || value.rfind('-').map(|i| i > 10).unwrap_or(false))
            && value.contains('.')
        {
            let format = DateFormats::Iso8601_4; // %f%z
            if let Ok(_) = Helpers::parse_date_from_string(value, format.as_str()) {
                return Some(format.name());
            }
        }
        if value.contains(' ')
            && (value.contains('+') || value.rfind('-').map(|i| i > 10).unwrap_or(false))
            && value.contains('.')
        {
            let fmt = "%Y-%m-%d %H:%M:%S.%f%z";
            if let Ok(_) = Helpers::parse_date_from_string(value, fmt) {
                return Some("Iso8601_SpaceOffset");
            }
        }

        // Check all remaining formats
        for format in DateFormats::iterator() {
            // Skip the formats we've already checked
            if *format == DateFormats::Iso8601 || *format == DateFormats::Iso8601_2 {
                continue;
            }

            let found_format = match Helpers::parse_date_from_string(value, format.as_str()) {
                Ok(_) => {
                    return Some(format.name());
                }
                Err(_) => None,
            };

            if found_format.is_some() {
                return found_format;
            }
        }

        // println!("Value {} is not a date of format that's known", value);

        None
    }

    fn derive_date_parser_kind(
        sample_value: &serde_json::Value,
        expect_timezone: bool,
    ) -> DateParserKind {
        let s = match sample_value.as_str() {
            Some(x) => x,
            None => return DateParserKind::RFC3339,
        };
        let has_ms = s.contains('.');
        let sep_t = s.contains('T');
        let has_z = s.ends_with('Z');
        let has_off = s.contains('+') || s.rfind('-').map(|i| i > 10).unwrap_or(false);
        if expect_timezone {
            match (has_ms, sep_t, has_z, has_off) {
                (false, true, true, _) => DateParserKind::ZNoMsT,
                (false, false, true, _) => DateParserKind::ZNoMsSpace,
                (true, true, true, _) => DateParserKind::ZMsT,
                (true, false, true, _) => DateParserKind::ZMsSpace,
                (false, true, _, true) => DateParserKind::OffNoMsT,
                (false, false, _, true) => DateParserKind::OffNoMsSpace,
                (true, true, _, true) => DateParserKind::OffMsT,
                (true, false, _, true) => DateParserKind::OffMsSpace,
                _ => DateParserKind::RFC3339,
            }
        } else {
            if s.len() == 19
                && s.as_bytes()[4] == b'-'
                && s.as_bytes()[7] == b'-'
                && (s.as_bytes()[10] == b' ' || s.as_bytes()[10] == b'T')
            {
                return DateParserKind::NaiveMysql;
            }
            if s.len() == 10 {
                return DateParserKind::NaiveDateOnly;
            }
            DateParserKind::RFC3339
        }
    }

    pub(crate) fn coerce_to_milli_seconds(v: Value) -> Value {
        if v.as_i64().unwrap() < 10000000000 {
            let millis = v.as_i64().unwrap() * 1000;
            // println!("field: {}, value: {}, v: {}", field, value, millis);
            millis.into()
        } else {
            v
        }
    }

    // pub fn apply_evolution_factory(
    //     &self,
    //     field: &mut std::string::String,
    //     _value: &mut String,
    //     evolution: String,
    //     data_type: &mut String,
    //     new_value: String,
    // ) {
    //     match &*evolution {
    //         "cast" => *data_type = new_value,
    //         "new" => *field = new_value,
    //         "rename" => *field = new_value,
    //         "merge" => *field = new_value,
    //         "default" => {}
    //         _ => {}
    //     }
    // }

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
            let new_meta = Metadata::new().unwrap();

            metadata.insert(field.clone(), new_meta);
        }
    }

    pub fn set_discovered_occurrence(
        &self,
        metadata: &mut HashMap<String, Metadata>,
        field: &String,
        data_type: &String,
        value: &mut String,
    ) {
        if metadata
            .get(field)
            .unwrap()
            .types
            .get(&data_type.to_string())
            .is_none()
        {
            metadata
                .get_mut(field)
                .unwrap()
                .types
                .insert(data_type.to_string(), 1);
            // array.get_mut(field).unwrap().evolution.insert(
            //     data_type.to_string(),
            //     Evolution {
            //         type_string: "".to_string(),
            //         new_field: "".to_string(),
            //         sovled: false,
            //     },
            // );

            // @todo
            // if !Config::analysing
            //     && Config::run_mode == Config::RUN_MODE_SYNC
            //     && Config::mutable_mode == Config::MUTABLE_MODE_EVOLVE
            // {
            //     // auto-accept new fields and types when syncing in 'evolve' mode
            //     array.get_mut(field).unwrap().determined_type = data_type.to_string();
            // }
        } else {
            let new_count: u32 = metadata
                .get_mut(field)
                .unwrap()
                .types
                .get(data_type)
                .unwrap()
                + 1;
            metadata
                .get_mut(field)
                .unwrap()
                .types
                .insert(data_type.to_string(), new_count);
        }

        // I found in practice theres too many false possitives for array values types of timestamp
        // @todo - probably better handeled in determine_field_types, not sure why it isn't already working
        // if metadata.get(field).unwrap().parent_type != "array" {
        if data_type == "integer" {
            let valid_timestamp = self.is_valid_timestamp(value);
            if valid_timestamp {
                self.set_discovered_occurrence(metadata, field, &"timestamp".to_string(), value);
            }
        } else if data_type == "long" {
            let valid_timestamp = self.is_valid_timestamp_milli(value);
            if valid_timestamp {
                self.set_discovered_occurrence(
                    metadata,
                    field,
                    &"timestamp_milli".to_string(),
                    value,
                );
            }
        }
        // }
    }

    pub fn determine_field_types(
        metadata: &mut HashMap<String, Metadata>,
        parent_type: Option<&str>,
        flatten: bool,
    ) {
        // let demoted_types = vec!["boolean", "date", "timestamp", "timestamp_milli"];

        for (field_name, field) in metadata.iter_mut() {
            // Useful for field evolution logic for maps, which only support one sub-field type
            if let Some(parent_type) = parent_type {
                field.parent_type = parent_type.to_string();
            }

            field.out_field_name = Helpers::clean_field_name(field_name.to_string());

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

                                // hacky, support inference on they fly when we only infer on one record.
                                // much more likely to be an integer than a boolean
                                // if field.types.len() == 1
                                //     && field.types.contains_key("boolean")
                                //     && field.types.get("boolean").unwrap() == &1
                                // {
                                //     highest_type = "integer".to_string();
                                //     highest_count = *data_type_count;
                                //     break;
                                // }

                                // if field.types.len() == 1
                                //     || (field.types.len() > 1
                                //         && !demoted_types.contains(&data_type.as_str()))
                                // {
                                highest_type = data_type.to_string();
                                highest_count = *data_type_count;
                                // }
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

                            // hacky, support inference on they fly when we only infer on one record.
                            // much more likely to be an integer than a boolean
                            // if sub_value.types.len() == 1
                            //     && sub_value.types.contains_key("boolean")
                            //     && sub_value.types.get("boolean").unwrap() == &1
                            //
                            // {
                            //     type_count.insert("integer".to_string(), *data_type_count);
                            //     break;
                            // }

                            // if type_count.len() <= 1
                            //     || (type_count.len() > 1
                            //         && !demoted_types.contains(&data_type.as_str()))
                            // {
                            if type_count.get(data_type).is_none() {
                                type_count.insert(data_type.to_string(), *data_type_count);
                            } else {
                                *type_count.get_mut(data_type).unwrap() += data_type_count;
                            }
                            // }
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

                    if field.determined_type == *"array" && field.determined_type_values != "record"
                    {
                        field.fields.clear();
                    }
                }

                // println!("Field {} determined type is {}", field_name, field.determined_type);
                // println!("Field {} values type is {}", field_name, field.determined_type_values);

                if field.determined_type != *"array"
                    || (field.determined_type == *"array"
                        && field.determined_type_values == "record")
                {
                    AnalyseSchema::determine_field_types(
                        &mut field.fields,
                        Some(&field.determined_type),
                        flatten,
                    );
                }
            }
        }

        // println!("Metadata {:?}", metadata);
    }

    #[allow(dead_code)]
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
mod check_string_or_int_tests {
    use super::*;

    #[test]
    fn test_32_bit_signed_int() {
        let dummy = AnalyseSchema { i: 0 };
        let mut value = "2147483647".to_string(); // max i32
        assert_eq!(dummy.check_string_or_int(&mut value), "integer");

        let mut value = "-2147483648".to_string(); // min i32
        assert_eq!(dummy.check_string_or_int(&mut value), "integer");
    }

    #[test]
    fn test_64_bit_signed_int() {
        let dummy = AnalyseSchema { i: 0 };
        let mut value = "9223372036854775807".to_string(); // max i64
        assert_eq!(dummy.check_string_or_int(&mut value), "long");

        let mut value = "-9223372036854775808".to_string(); // min i64
        assert_eq!(dummy.check_string_or_int(&mut value), "long");
    }

    #[test]
    fn test_non_integer() {
        let dummy = AnalyseSchema { i: 0 };
        let mut value = "Hello".to_string();
        assert_eq!(dummy.check_string_or_int(&mut value), "string"); // Assuming get_type returns "string"
    }
}

#[cfg(test)]
mod is_32_int_tests {
    use super::*;

    #[test]
    fn test_within_range() {
        let dummy = AnalyseSchema { i: 0 };
        let mut value = "2147483647".to_string(); // max i32
        assert!(dummy.is_32_bit_signed_int(&mut value));

        let mut value = "-2147483648".to_string(); // min i32
        assert!(dummy.is_32_bit_signed_int(&mut value));
    }

    #[test]
    fn test_out_of_range() {
        let dummy = AnalyseSchema { i: 0 };
        let mut value = "2147483648".to_string(); // just above max i32
        assert!(!dummy.is_32_bit_signed_int(&mut value));

        let mut value = "-2147483649".to_string(); // just below min i32
        assert!(!dummy.is_32_bit_signed_int(&mut value));
    }

    #[test]
    fn test_invalid_input() {
        let dummy = AnalyseSchema { i: 0 };
        let mut value = "not a number".to_string();
        assert!(!dummy.is_32_bit_signed_int(&mut value));
    }
}

#[cfg(test)]
mod is_64_int_tests {
    use super::*;

    //-9223372036854775808, 9223372036854775807

    #[test]
    fn test_within_range() {
        let dummy = AnalyseSchema { i: 0 };
        let mut value = "9223372036854775807".to_string(); // max i64
        assert!(dummy.is_64_bit_signed_int(&mut value));

        let mut value = "-9223372036854775808".to_string(); // min i64
        assert!(dummy.is_64_bit_signed_int(&mut value));
    }

    #[test]
    fn test_out_of_range() {
        let dummy = AnalyseSchema { i: 0 };
        let mut value = "9223372036854775808".to_string(); // just above max i64
        assert!(!dummy.is_64_bit_signed_int(&mut value));

        let mut value = "-9223372036854775809".to_string(); // just below min i64
        assert!(!dummy.is_64_bit_signed_int(&mut value));
    }

    #[test]
    fn test_invalid_input() {
        let dummy = AnalyseSchema { i: 0 };
        let mut value = "not a number".to_string();
        assert!(!dummy.is_64_bit_signed_int(&mut value));
    }
}

#[cfg(test)]
mod get_type_bool_tests {

    use crate::discover::get_type;

    #[test]
    fn test_get_type_int() {
        let expected_type = "boolean".to_string();

        let subject = 123;
        assert_ne!(get_type(&mut subject.to_string()), expected_type);
    }

    #[test]
    fn test_get_type_true_int() {
        let expected_type = "boolean".to_string();

        let subject = 1;
        assert_ne!(get_type(&mut subject.to_string()), expected_type);
    }

    #[test]
    fn test_get_type_false_int() {
        let expected_type = "boolean".to_string();

        let subject = 0;
        assert_ne!(get_type(&mut subject.to_string()), expected_type);
    }

    #[test]
    fn test_get_type_true_bool() {
        let expected_type = "boolean".to_string();

        let subject = true;
        assert_eq!(get_type(&mut subject.to_string()), expected_type);
    }

    #[test]
    fn test_get_type_false_bool() {
        let expected_type = "boolean".to_string();

        let subject = false;
        assert_eq!(get_type(&mut subject.to_string()), expected_type);
    }

    #[test]
    fn test_get_type_true_str() {
        let expected_type = "boolean".to_string();

        let subject = "true";
        assert_eq!(get_type(&mut subject.to_string()), expected_type);
    }

    #[test]
    fn test_get_type_false_str() {
        let expected_type = "boolean".to_string();

        let subject = "false";
        assert_eq!(get_type(&mut subject.to_string()), expected_type);
    }

    #[test]
    fn test_get_type_true_upper_str() {
        let expected_type = "string".to_string();

        let subject = "True";
        assert_eq!(get_type(&mut subject.to_string()), expected_type);
    }

    #[test]
    fn test_get_type_false_upper_str() {
        let expected_type = "string".to_string();

        let subject = "False";
        assert_eq!(get_type(&mut subject.to_string()), expected_type);
    }

    #[test]
    fn test_get_type_yes_str() {
        let expected_type = "string".to_string();

        let subject = "yes";
        assert_eq!(get_type(&mut subject.to_string()), expected_type);
    }

    #[test]
    fn test_get_type_no_str() {
        let expected_type = "string".to_string();

        let subject = "no";
        assert_eq!(get_type(&mut subject.to_string()), expected_type);
    }
}

#[cfg(test)]
mod valid_timestamps_tests {
    use super::*;

    #[test]
    fn test_valid_timestamps() {
        let my_struct = AnalyseSchema { i: 0 };
        // Update test cases to match new validation rules
        // No longer validate "0" as timestamp since we have a minimum of January 1, 2010
        assert!(!my_struct.is_valid_timestamp(&mut "0".to_string())); // Now treated as an invalid timestamp (too early)
        assert!(my_struct.is_valid_timestamp(&mut "1577836800".to_string())); // Jan 1, 2020 (valid)
        assert!(my_struct.is_valid_timestamp(&mut "1262304000".to_string())); // Jan 1, 2010 (minimum timestamp, valid)
        assert!(my_struct.is_valid_timestamp(&mut "1609459200".to_string())); // Jan 1, 2021 (valid)
    }

    #[test]
    fn test_invalid_timestamps() {
        let my_struct = AnalyseSchema { i: 0 };
        assert!(!my_struct.is_valid_timestamp(&mut "-1".to_string())); // Before UNIX epoch
        assert!(!my_struct.is_valid_timestamp(&mut "2208988800".to_string())); // After 2040
        assert!(!my_struct.is_valid_timestamp(&mut "1262303999".to_string())); // One second before Jan 1, 2010 (invalid)
    }

    #[test]
    fn test_edge_cases() {
        let my_struct = AnalyseSchema { i: 0 };
        // Start and end of the allowed range
        assert!(!my_struct.is_valid_timestamp(&mut "0".to_string())); // Start of 1970 (now invalid)
        assert!(my_struct.is_valid_timestamp(&mut "1262304000".to_string())); // Jan 1, 2010 (minimum timestamp, valid)
        assert!(my_struct.is_valid_timestamp(&mut "2208988799".to_string())); // Just before 2040 (valid)
    }

    #[test]
    fn test_non_numeric_and_malformed_inputs() {
        let my_struct = AnalyseSchema { i: 0 };
        assert!(!my_struct.is_valid_timestamp(&mut "abc".to_string()));
        assert!(!my_struct.is_valid_timestamp(&mut "1970-01-01".to_string())); // Non-numeric
                                                                               // Add more non-numeric or malformed cases here
    }

    #[test]
    fn test_overflow_underflow_cases() {
        let my_struct = AnalyseSchema { i: 0 };
        // Test cases for potential overflow or underflow conditions
        assert!(!my_struct.is_valid_timestamp(&mut "99999999999999999999".to_string())); // Very large number
        assert!(!my_struct.is_valid_timestamp(&mut "-99999999999999999999".to_string()));
        // Very negative number
    }
}

#[cfg(test)]
mod is_valid_date_tests {
    use super::*;
    #[allow(unused_imports)]
    use chrono::{DateTime, NaiveDate, NaiveDateTime};
    use serial_test::serial;

    #[test]
    #[serial]
    fn test_valid_date_formats() {
        let _foo: AnalyseSchema = AnalyseSchema { i: 0 };

        let date_str = "2022-01-07T08:28:07.000Z";
        let json_value: Value = date_str.into();
        let value = json_value.as_str().unwrap();

        assert_eq!(Some("Iso8601"), AnalyseSchema::is_valid_date(value));

        let date_str = "2022-01-05T08:30:12.000Z";
        assert_eq!(Some("Iso8601"), AnalyseSchema::is_valid_date(date_str));

        let fmt = DateFormats::from_str("Iso8601").unwrap();
        assert_eq!("%Y-%m-%dT%H:%M:%S.%fZ", fmt.as_str());
        Helpers::parse_date_from_string(date_str, fmt.as_str()).unwrap();

        let date_str = "2022-01-07T08:28:07Z";
        assert_eq!(Some("Iso8601_2"), AnalyseSchema::is_valid_date(date_str));

        let date_str = "2022-02-22T22:22:22";
        assert_eq!(Some("Atom"), AnalyseSchema::is_valid_date(date_str));

        let mut date_str = "2021-01-03 02:30:00";
        assert_eq!(Some("Mysql"), AnalyseSchema::is_valid_date(date_str));

        date_str = "Tue, 22 Feb 2022 22:22:22 GMT";
        assert_eq!(Some("Rfc850"), AnalyseSchema::is_valid_date(date_str));

        date_str = "2022-02-22";
        assert_eq!(Some("DateOnly"), AnalyseSchema::is_valid_date(date_str));
    }

    #[test]
    #[serial]
    fn test_invalid_date_format() {
        let _foo: AnalyseSchema = AnalyseSchema { i: 0 };
        let date_str = "2022-22-22";
        assert_eq!(None, AnalyseSchema::is_valid_date(date_str));
    }

    #[test]
    #[serial]
    fn test_empty_date_string() {
        let _foo: AnalyseSchema = AnalyseSchema { i: 0 };
        let date_str = "";
        assert_eq!(None, AnalyseSchema::is_valid_date(date_str));
    }

    #[test]
    #[serial]
    fn test_non_date_string() {
        let _foo: AnalyseSchema = AnalyseSchema { i: 0 };
        let date_str = "not a date";
        assert_eq!(None, AnalyseSchema::is_valid_date(date_str));
    }

    #[test]
    #[serial]
    fn test_valid_date_time() {
        let _foo: AnalyseSchema = AnalyseSchema { i: 0 };
        let date_str = "2022-02-22T22:22:22Z";
        let _dt = DateTime::parse_from_rfc3339(date_str).unwrap();
        assert_eq!(Some("Iso8601_2"), AnalyseSchema::is_valid_date(date_str));
    }

    #[test]
    #[serial]
    fn test_valid_date() {
        let _foo: AnalyseSchema = AnalyseSchema { i: 0 };
        let date_str = "2022-02-22";
        let _nd = Helpers::parse_date_from_string(date_str, "%Y-%m-%d").unwrap();
        assert_eq!(Some("DateOnly"), AnalyseSchema::is_valid_date(date_str));
    }
}

#[cfg(test)]
mod discover_date_formats_tests {
    use super::*;
    #[allow(unused_imports)]
    use chrono::NaiveDateTime;
    use serial_test::serial;

    #[test]
    #[serial]
    fn test_valid_date_formats() {
        let _foo: AnalyseSchema = AnalyseSchema { i: 0 };

        let date_str = "2022-01-05T08:30:12.000Z";
        assert_eq!(Some("Iso8601"), AnalyseSchema::is_valid_date(date_str));

        let fmt = DateFormats::from_str("Iso8601").unwrap();
        assert_eq!("%Y-%m-%dT%H:%M:%S.%fZ", fmt.as_str());
        Helpers::parse_date_from_string(date_str, fmt.as_str()).unwrap();

        let date_str = "2022-01-07T08:28:07Z";
        assert_eq!(Some("Iso8601_2"), AnalyseSchema::is_valid_date(date_str));

        let date_str = "2022-02-22T22:22:22";
        assert_eq!(Some("Atom"), AnalyseSchema::is_valid_date(date_str));

        let mut date_str = "2021-01-03 02:30:00";
        assert_eq!(Some("Mysql"), AnalyseSchema::is_valid_date(date_str));

        date_str = "Tue, 22 Feb 2022 22:22:22 GMT";
        assert_eq!(Some("Rfc850"), AnalyseSchema::is_valid_date(date_str));

        date_str = "2022-02-22";
        assert_eq!(Some("DateOnly"), AnalyseSchema::is_valid_date(date_str));
    }
}

#[cfg(test)]
mod tests {
    use serial_test::serial;
    use std::collections::HashMap;
    #[allow(unused_imports)]
    use std::fs::{remove_file, File, OpenOptions};
    #[allow(unused_imports)]
    use std::io::{Seek, Write};

    use crate::discover::{AnalyseSchema, Metadata};
    use crate::helpers::configuration::Config;
    #[allow(unused_imports)]
    use parquet::data_type::AsBytes;
    #[allow(unused_imports)]
    use rand::Rng;
    use serde_json::Value;
    #[allow(unused_imports)]
    use std::path::Path;

    #[test]
    #[serial]
    fn test_discover_arrays_maps() {
        let _foo: AnalyseSchema = AnalyseSchema { i: 0 };

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
                "abc7": {"a": "abc", "b": 456, "c": 4.4},
                "abc8": [{"a": "abc", "b": 456, "c": 4.4},{"a": "abc", "b": 456, "c": 4.4}]
        }"#;

        let json: Value = serde_json::from_str(field).unwrap();

        let mut record_line = serde_json::to_string(&json).unwrap();

        let _data_dir = Config::get_data_dir();

        let _rng = rand::thread_rng(); // Removed mut since it's not needed

        // let random_tmp_file_name = rng.gen::<i32>();

        // let mut test_file = OpenOptions::new()
        //     .write(true)
        //     .truncate(true)
        //     .create_new(true)
        //     .open(format!("./{}", random_tmp_file_name))
        //     .unwrap();

        // let mut test_file = File::create_new(format!("./{}", random_tmp_file_name)).unwrap();

        // test_file.write(record_line.as_bytes()).unwrap();

        // test_file.rewind().unwrap();

        // let mut in_file = MemFile::create(rng.gen::<i32>(), CreateOptions::new()).unwrap();
        // let in_file = File::open(format!("./{}", random_tmp_file_name)).unwrap();

        // let mut buf_reader = BufReader::new(in_file);

        let mut _fields: Box<HashMap<String, Metadata>> = Box::new(HashMap::new()); // Removed mut since it's not needed

        AnalyseSchema::infer_json_schema(&_foo, &mut record_line, Some(1), &mut _fields);

        AnalyseSchema::determine_field_types(
            &mut _fields.get_mut("default").unwrap().fields,
            None,
            false,
        );

        // This line was trying to remove a file that doesn't exist
        // let _ = remove_file(Path::new(&format!("./{}", random_tmp_file_name)));

        // remove_file(Path::new(&format!("./{}", random_tmp_file_name))).unwrap();

        // println!("{:?}", _fields);
        // println!("{:?}", _fields.get("default").unwrap().fields);
        // println!(
        //     "{:?}",
        //     _fields.get("default").unwrap().fields.get("abc1").unwrap()
        // );
        // println!(
        //     "{:?}",
        //     _fields.get("default").unwrap().fields.get("abc2").unwrap()
        // );
        // println!("{:?}", new_meta.get("default").unwrap().fields.get("abc2").unwrap().determined_type);
        // println!("{:?}", new_meta.get("default").unwrap().fields.get("abc2").unwrap().determined_type_values);

        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("abc1")
                .unwrap()
                .determined_type,
            "array"
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("abc1")
                .unwrap()
                .determined_type_values,
            "integer"
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("abc2")
                .unwrap()
                .determined_type,
            "array"
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("abc2")
                .unwrap()
                .determined_type_values,
            "string"
        );
        // assert_eq!(_fields.get("default").unwrap().fields.get("abc3").unwrap().determined_type, "array");
        // assert_eq!(_fields.get("default").unwrap().fields.get("abc3").unwrap().determined_type_values, "string" );
        // assert_eq!(_fields.get("default").unwrap().fields.get("abc4").unwrap().determined_type, "array");
        // assert_eq!(_fields.get("default").unwrap().fields.get("abc4").unwrap().determined_type_values, "string");
        // assert_eq!(_fields.get("default").unwrap().fields.get("abc5").unwrap().determined_type, "array");
        // assert_eq!(_fields.get("default").unwrap().fields.get("abc5").unwrap().determined_type_values, "integer");
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("abc6")
                .unwrap()
                .determined_type,
            "array"
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("abc7")
                .unwrap()
                .determined_type,
            "record"
        );

        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("abc8")
                .unwrap()
                .determined_type,
            "array"
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("abc8")
                .unwrap()
                .determined_type_values,
            "record"
        );

        // match _fields.get("default").unwrap().fields.get("abc8").unwrap().fields.get("1") {
        //     Some(_) => {
        //         match _fields.get("default").unwrap().fields.get("abc8").unwrap().fields.get("1").unwrap().fields.get("a") {
        //             Some(_) => {
        //                 match _fields.get("default").unwrap().fields.get("abc8").unwrap().fields.get("1").unwrap().fields.get("b").unwrap().determined_type.as_str() {
        //                     "string" => {}
        //                     _ => panic!("Field abc8.1.b not string"),
        //                 }
        //             }
        //             None => panic!("Field abc8.1.a not found"),
        //         }
        //     }
        //     None => panic!("Field abc8.1 not found. {:?}", _fields.get("default").unwrap().fields.get("abc8").unwrap()),
        // }
        // println!("{:?}", _fields.get("default").unwrap().fields.get("abc8").unwrap().fields.get("1").unwrap());
        // assert_eq!(_fields.get("default").unwrap().fields.get("abc8").unwrap().fields.get("1").unwrap().fields.get("a").unwrap().determined_type, "string");
        // assert_eq!(_fields.get("default").unwrap().fields.get("abc8").unwrap().fields.get("1").unwrap().fields.get("b").unwrap().determined_type, "integer");
    }

    #[test]
    #[serial]
    fn test_discover_demoted_types() {
        let _foo: AnalyseSchema = AnalyseSchema { i: 0 };

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

        let mut record_line = serde_json::to_string(&json).unwrap();

        let _rng = rand::thread_rng(); // Removed mut since it's not needed

        // let random_tmp_file_name = rng.gen::<i32>();

        // let mut test_file = OpenOptions::new()
        //     .write(true)
        //     .truncate(true)
        //     .create_new(true)
        //     .open(format!("./{}", random_tmp_file_name))
        //     .unwrap();
        //
        // // let mut test_file = MemFile::create(rng.gen::<i32>(), CreateOptions::new()).unwrap();
        //
        // test_file.write(record_line.as_bytes()).unwrap();
        //
        // test_file.rewind().unwrap();

        // let in_file = File::open(format!("./{}", random_tmp_file_name)).unwrap();
        // let mut in_file = MemFile::create(rng.gen::<i32>(), CreateOptions::new()).unwrap();

        // let mut buf_reader = BufReader::new(in_file);

        let mut _fields: Box<HashMap<String, Metadata>> = Box::new(HashMap::new()); // Removed mut since it's not needed

        AnalyseSchema::infer_json_schema(&_foo, &mut record_line, Some(1), &mut _fields);

        AnalyseSchema::determine_field_types(
            &mut _fields.get_mut("default").unwrap().fields,
            None,
            false,
        );

        // This line was trying to remove a file that doesn't exist
        // let _ = remove_file(Path::new(&format!("./{}", random_tmp_file_name)));

        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("boolean")
                .unwrap()
                .determined_type,
            "array"
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("boolean") // I recall we stopped infering bool ints as boolean, it was too error prone and probably trying to be too smart
                .unwrap()
                .determined_type_values,
            "integer"
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("boolean2")
                .unwrap()
                .determined_type,
            "array"
        );
        assert_eq!(
            _fields
                .get("default")
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
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("timestamp")
                .unwrap()
                .determined_type,
            "array"
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("timestamp")
                .unwrap()
                .determined_type_values,
            "timestamp"
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("timestamp_milli")
                .unwrap()
                .determined_type,
            "array"
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("timestamp_milli")
                .unwrap()
                .determined_type_values,
            "timestamp_milli"
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("abc3")
                .unwrap()
                .determined_type,
            "array"
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("abc3")
                .unwrap()
                .determined_type_values,
            "integer"
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("abc4")
                .unwrap()
                .determined_type,
            "array"
        );
        assert_eq!(
            _fields
                .get("default")
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
        let _foo: AnalyseSchema = AnalyseSchema { i: 0 };

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

        let _rng = rand::thread_rng(); // Removed mut since it's not needed

        // let random_tmp_file_name = rng.gen::<i32>();

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
        //
        // let in_file = File::open(format!("./{}", random_tmp_file_name)).unwrap();
        // let mut in_file = MemFile::create(rng.gen::<i32>(), CreateOptions::new()).unwrap();

        let mut _fields: Box<HashMap<String, Metadata>> = Box::new(HashMap::new()); // Removed mut since it's not needed

        AnalyseSchema::infer_json_schema(&_foo, &mut record_line, Some(1), &mut _fields);

        AnalyseSchema::determine_field_types(
            &mut _fields.get_mut("default").unwrap().fields,
            None,
            false,
        );

        // remove_file(Path::new(&format!("./{}", random_tmp_file_name))).unwrap();

        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("sheep")
                .unwrap()
                .determined_type,
            "string"
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("arable")
                .unwrap()
                .determined_type,
            "boolean"
        );

        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("crank")
                .unwrap()
                .determined_type,
            "record"
        );
        assert_eq!(
            _fields
                .get("default")
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
            _fields
                .get("default")
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
            _fields
                .get("default")
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
            _fields
                .get("default")
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
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("crank_torques")
                .unwrap()
                .determined_type,
            "array" // Correctly identified as array
        );

        // Since crank_torques is now properly identified as an array,
        // we should check its determined_type_values instead of looking
        // for sub-fields indexed by "0", "1", etc.
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("crank_torques")
                .unwrap()
                .determined_type_values,
            "array" // The elements are arrays themselves
        );

        // Remove assertions that no longer apply with the new type determination logic
        // The following assertions expected a different metadata structure
        // that existed when arrays were incorrectly identified as records
        /*
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("crank_torques")
                .unwrap()
                .fields
                .get("0")
                .unwrap()
                .determined_type,
            "array"
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("crank_torques")
                .unwrap()
                .fields
                .get("0")
                .unwrap()
                .determined_type_values,
            "integer"
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("crank_torques")
                .unwrap()
                .fields
                .get("1")
                .is_none(),
            true);
        */
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("metadata")
                .unwrap()
                .fields
                .get("tags")
                .unwrap()
                .determined_type,
            "array"
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("metadata")
                .unwrap()
                .fields
                .get("tags")
                .unwrap()
                .fields
                .get("0")
                .unwrap()
                .determined_type,
            "record"
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

#[cfg(test)]
mod tests_flatten_metadata {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_flatten_metadata() {
        let mut fields: Box<HashMap<String, Metadata>> = Box::new(HashMap::new());

        let metadata_child = Metadata {
            count: 1,
            types: HashMap::new(),
            parent_type: "record".to_string(),
            fields: Box::new(HashMap::new()),
            date_candidate: None,
            date_parser_kind: None,
            timezone: false,
            evolution: Box::new(HashMap::new()),
            enabled: true,
            out_field_name: "child".to_string(),
            determined_type: "string".to_string(),
            determined_type_values: "".to_string(),
            repetition_count: 1, // New field to track array repetition count
        };

        fields.insert("child".to_string(), metadata_child.clone());

        let metadata = Metadata {
            count: 1,
            types: HashMap::new(),
            parent_type: "".to_string(),
            fields: fields,
            date_candidate: None,
            date_parser_kind: None,
            timezone: false,
            evolution: Box::new(HashMap::new()),
            enabled: true,
            out_field_name: "parent".to_string(),
            determined_type: "record".to_string(),
            determined_type_values: "".to_string(),
            repetition_count: 1, // New field to track array repetition count
        };

        let mut flattened: OutputMetadata = OutputMetadata::new();

        Metadata::flatten_metadata(&metadata, &mut flattened);

        println!("{:?}", flattened);

        assert_eq!(flattened.fields.len(), 1);
        assert_eq!(
            flattened.fields.get("parent_child").unwrap().out_field_name,
            "parent_child"
        );
        // assert!(flattened.contains_key("parent_child"));
    }

    // @todo - support flattening of arrays of structs?
    #[test]
    fn test_flatten_array_of_n_structs_metadata() {
        let _fields: Box<HashMap<String, Metadata>> = Box::new(HashMap::new());

        let mut metadata: HashMap<String, Metadata> = HashMap::new();

        metadata.insert(
            "schema".into(),
            Metadata {
                count: 1,
                types: HashMap::new(),
                parent_type: "".into(),
                fields: Box::new(HashMap::new()),
                date_candidate: None,
                date_parser_kind: None,
                timezone: false,
                evolution: Box::new(HashMap::new()),
                enabled: true,
                out_field_name: "".into(),
                determined_type: "".into(),
                determined_type_values: "".into(),
                repetition_count: 1, // New field to track array repetition count
            },
        );
        metadata.get_mut("schema").unwrap().fields.insert(
            "contacts".into(),
            Metadata {
                count: 1,
                types: HashMap::new(),
                parent_type: "".into(),
                fields: Box::new(HashMap::new()),
                date_candidate: None,
                date_parser_kind: None,
                timezone: false,
                evolution: Box::new(HashMap::new()),
                enabled: true,
                out_field_name: "contacts".into(),
                determined_type: "array".into(),
                determined_type_values: "".into(),
                repetition_count: 2, // Explicitly setting repetition_count to 2
            },
        );
        metadata
            .get_mut("schema")
            .unwrap()
            .fields
            .get_mut("contacts")
            .unwrap()
            .fields
            .insert(
                "0".into(),
                Metadata {
                    count: 2,
                    types: HashMap::new(),
                    parent_type: "".into(),
                    fields: Box::new(HashMap::new()),
                    date_candidate: None,
                    date_parser_kind: None,
                    timezone: false,
                    evolution: Box::new(HashMap::new()),
                    enabled: true,
                    out_field_name: "0".into(),
                    determined_type: "string".into(),
                    determined_type_values: "".into(),
                    repetition_count: 1, // New field to track array repetition count
                },
            );
        metadata
            .get_mut("schema")
            .unwrap()
            .fields
            .get_mut("contacts")
            .unwrap()
            .fields
            .get_mut("0")
            .unwrap()
            .fields
            .insert(
                "name".into(),
                Metadata {
                    count: 2,
                    types: HashMap::new(),
                    parent_type: "".into(),
                    fields: Box::new(HashMap::new()),
                    date_candidate: None,
                    date_parser_kind: None,
                    timezone: false,
                    evolution: Box::new(HashMap::new()),
                    enabled: true,
                    out_field_name: "name".into(),
                    determined_type: "string".into(),
                    determined_type_values: "".into(),
                    repetition_count: 1, // New field to track array repetition count
                },
            );
        metadata
            .get_mut("schema")
            .unwrap()
            .fields
            .get_mut("contacts")
            .unwrap()
            .fields
            .get_mut("0")
            .unwrap()
            .fields
            .insert(
                "tel".into(),
                Metadata {
                    count: 2,
                    types: HashMap::new(),
                    parent_type: "".into(),
                    fields: Box::new(HashMap::new()),
                    date_candidate: None,
                    date_parser_kind: None,
                    timezone: false,
                    evolution: Box::new(HashMap::new()),
                    enabled: true,
                    out_field_name: "tel".into(),
                    determined_type: "int".into(),
                    determined_type_values: "".into(),
                    repetition_count: 1, // New field to track array repetition count
                },
            );

        let mut flattened: OutputMetadata = OutputMetadata::new();

        Metadata::flatten_metadata(&metadata.get("schema").unwrap(), &mut flattened);

        println!("{:?}", flattened);

        // Should have 4 fields: contacts_0_name, contacts_0_tel, contacts_1_name, contacts_1_tel
        assert_eq!(flattened.fields.len(), 4);

        assert_eq!(
            flattened
                .fields
                .get("contacts_0_name")
                .unwrap()
                .out_field_name,
            "contacts_0_name"
        );
        assert_eq!(
            flattened
                .fields
                .get("contacts_0_tel")
                .unwrap()
                .out_field_name,
            "contacts_0_tel"
        );
        assert_eq!(
            flattened
                .fields
                .get("contacts_0_name")
                .unwrap()
                .determined_type,
            "string"
        );
        assert_eq!(
            flattened
                .fields
                .get("contacts_0_tel")
                .unwrap()
                .determined_type,
            "int"
        );

        assert_eq!(
            flattened
                .fields
                .get("contacts_1_name")
                .unwrap()
                .out_field_name,
            "contacts_1_name"
        );
        assert_eq!(
            flattened
                .fields
                .get("contacts_1_tel")
                .unwrap()
                .out_field_name,
            "contacts_1_tel"
        );
        assert_eq!(
            flattened
                .fields
                .get("contacts_1_name")
                .unwrap()
                .determined_type,
            "string"
        );
        assert_eq!(
            flattened
                .fields
                .get("contacts_1_tel")
                .unwrap()
                .determined_type,
            "int"
        );

        // assert!(flattened.contains_key("parent_record_child"));
    }

    #[test]
    fn test_flatten_primitive_array_field() {
        // Create a record with a primitive array field
        let mut fields: Box<HashMap<String, Metadata>> = Box::new(HashMap::new());

        // Create a primitive array field
        let mut array_field = Metadata::new().unwrap();
        array_field.out_field_name = "x_axis_linear_mean".to_string();
        array_field.determined_type = "array".to_string();
        array_field.determined_type_values = "double".to_string();
        array_field.enabled = true;

        fields.insert("x_axis_linear_mean".to_string(), array_field.clone());

        // Create the parent record
        let mut metadata = Metadata::new().unwrap();
        metadata.out_field_name = "imu".to_string();
        metadata.determined_type = "record".to_string();
        metadata.enabled = true;
        metadata.fields = fields;

        let mut flattened: OutputMetadata = OutputMetadata::new();

        // Flatten the metadata
        Metadata::flatten_metadata(&metadata, &mut flattened);

        // The flattened metadata should contain the primitive array field
        println!("Flattened: {:?}", flattened);

        // Check if the array field exists with the correct path
        let expected_field_name = "imu_x_axis_linear_mean";
        assert!(
            flattened.fields.contains_key(expected_field_name),
            "Flattened metadata does not contain the primitive array field"
        );

        // Verify the field properties
        let field = flattened.fields.get(expected_field_name).unwrap();
        assert_eq!(field.out_field_name, expected_field_name);
        assert_eq!(field.determined_type, "array");
        assert_eq!(field.determined_type_values, "double");
    }
}
