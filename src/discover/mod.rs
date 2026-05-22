#![allow(dead_code)]
use std::any::Any;

use std::collections::HashMap;

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
            if field_metadata.determined_type == SkipprDataType::Array
                && field_metadata.determined_type_values == Some(SkipprDataType::Record)
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
    if discoverd_data_type == SkipprDataType::Date {
        if let Some(meta) = metadata.get_mut(field) {
            meta.date_parser_kind =
                Some(AnalyseSchema::derive_date_parser_kind(value, meta.timezone));
        }
    }

    discoverd_data_type.to_string()
}

/**
 * Reduced metadata for output schema, only includes fields that are required to generate an output schema
 *
 * The main motivation for this, is to be very clear that the output metadata is generated from the input metadata.
 * This is currently only done via flatten_metadata(), concevable this may evolve.
 */
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct OutputMetadata {
    #[serde(default)]
    pub(crate) source_field_name: String,
    pub(crate) out_field_name: String,
    pub(crate) determined_type: SkipprDataType,
    #[serde(with = "serde_opt_data_type")]
    pub(crate) determined_type_values: Option<SkipprDataType>,
    #[serde(default)]
    pub(crate) field_id: i32,
    #[serde(default)]
    pub(crate) schema_id: u64,
    #[serde(default)]
    pub(crate) lineage_id: String,
    #[serde(default = "crate::lineage::default_nullable")]
    pub(crate) nullable: bool,
    #[serde(default)]
    pub(crate) default_value: Option<serde_json::Value>,
    pub(crate) fields: Box<HashMap<String, OutputMetadata>>,
}

impl OutputMetadata {
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self {
            out_field_name: "".to_string(),
            source_field_name: "".to_string(),
            determined_type: SkipprDataType::Unknown,
            determined_type_values: None,
            field_id: 0,
            schema_id: 0,
            lineage_id: "".to_string(),
            nullable: true,
            default_value: None,
            fields: Box::new(HashMap::new()),
        }
    }

    pub fn out_field_name(&self) -> &str {
        &self.out_field_name
    }

    pub fn source_field_name(&self) -> &str {
        &self.source_field_name
    }

    pub fn determined_type(&self) -> &SkipprDataType {
        &self.determined_type
    }

    pub fn determined_type_values(&self) -> Option<&SkipprDataType> {
        self.determined_type_values.as_ref()
    }

    pub fn nullable(&self) -> bool {
        self.nullable
    }

    pub fn field_id(&self) -> i32 {
        self.field_id
    }

    pub fn schema_id(&self) -> u64 {
        self.schema_id
    }

    pub fn lineage_id(&self) -> &str {
        &self.lineage_id
    }

    pub fn default_value(&self) -> Option<&serde_json::Value> {
        self.default_value.as_ref()
    }

    pub fn child_fields(&self) -> impl Iterator<Item = (&String, &OutputMetadata)> {
        self.fields.iter()
    }

    pub fn fields_clone(&self) -> Box<HashMap<String, OutputMetadata>> {
        self.fields.clone()
    }

    pub fn from_metadata(metadata: &Metadata) -> OutputMetadata {
        let mut output_metadata = OutputMetadata::new();

        output_metadata.out_field_name = metadata.out_field_name.clone();
        output_metadata.source_field_name = metadata.source_field_name.clone();
        output_metadata.determined_type = metadata.determined_type.clone();
        output_metadata.determined_type_values = metadata.determined_type_values.clone();
        output_metadata.field_id = metadata.field_id;
        output_metadata.schema_id = metadata.schema_id;
        output_metadata.lineage_id = metadata.lineage_id.clone();
        output_metadata.nullable = metadata.nullable;
        output_metadata.default_value = metadata.default_value.clone();

        let mut fields = HashMap::new();
        for (field, md) in metadata.fields.iter() {
            fields.insert(field.clone(), OutputMetadata::from_metadata(md));
        }
        output_metadata.fields = Box::new(fields);

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
    #[serde(default)]
    pub metadata_version: u32,
    #[serde(default)]
    pub schema_id: u64,
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
            metadata_version: CURRENT_METADATA_VERSION,
            schema_id: DEFAULT_SCHEMA_ID,
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
            metadata_version: CURRENT_METADATA_VERSION,
            schema_id: DEFAULT_SCHEMA_ID,
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

    pub fn migrate_persisted_metadata(&mut self) -> bool {
        let mut changed = self.metadata_version < CURRENT_METADATA_VERSION;
        if self.schema_id == 0 {
            self.schema_id = DEFAULT_SCHEMA_ID;
            changed = true;
        }
        for (namespace, metadata) in self.metadata.iter_mut() {
            if Metadata::migrate_tree(namespace, metadata, self.schema_id, Vec::new()) {
                changed = true;
            }
        }
        if self.metadata_version < CURRENT_METADATA_VERSION {
            self.metadata_version = CURRENT_METADATA_VERSION;
            changed = true;
        }
        changed
    }
}

const CURRENT_METADATA_VERSION: u32 = 2;
const DEFAULT_SCHEMA_ID: u64 = 1;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Metadata {
    pub(crate) count: i32,
    pub(crate) types: HashMap<SkipprDataType, u32>,
    #[serde(with = "serde_opt_data_type")]
    pub(crate) parent_type: Option<SkipprDataType>,
    pub(crate) fields: Box<HashMap<String, Metadata>>,
    pub(crate) date_candidate: Option<DateCandidate>,
    pub(crate) date_parser_kind: Option<DateParserKind>,
    pub(crate) timezone: bool,
    pub(crate) evolution: Box<HashMap<String, Evolution>>,
    pub(crate) enabled: bool,
    #[serde(default)]
    pub(crate) source_field_name: String,
    pub(crate) out_field_name: String,
    pub(crate) determined_type: SkipprDataType,
    #[serde(with = "serde_opt_data_type")]
    pub(crate) determined_type_values: Option<SkipprDataType>,
    pub(crate) repetition_count: i32,
    #[serde(default)]
    pub(crate) field_id: i32,
    #[serde(default)]
    pub(crate) schema_id: u64,
    #[serde(default)]
    pub(crate) lineage_id: String,
    #[serde(default = "crate::lineage::default_nullable")]
    pub(crate) nullable: bool,
    #[serde(default)]
    pub(crate) default_value: Option<Value>,
}

impl Metadata {
    #[inline]
    #[must_use]
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            count: 0,
            types: Default::default(),
            parent_type: None,
            fields: Box::new(Default::default()),
            date_candidate: None,
            date_parser_kind: None,
            timezone: false,
            evolution: Box::new(Default::default()),
            enabled: true,
            source_field_name: "".to_string(),
            out_field_name: "".to_string(),
            determined_type: SkipprDataType::Unknown,
            determined_type_values: None,
            repetition_count: 5,
            field_id: 0,
            schema_id: 0,
            lineage_id: "".to_string(),
            nullable: true,
            default_value: None,
        })
    }

    pub fn new_with_type(data_type: SkipprDataType, field_name: &str) -> Self {
        let mut m = Self::new().unwrap();
        m.determined_type = data_type;
        m.source_field_name = field_name.to_string();
        m.out_field_name = field_name.to_string();
        m.enabled = true;
        m
    }

    pub fn set_field(&mut self, name: &str, metadata: Metadata) {
        self.fields.insert(name.to_string(), metadata);
    }

    pub fn field_count(&self) -> usize {
        self.fields.len()
    }

    pub fn nullable(&self) -> bool {
        self.nullable
    }

    pub fn default_value(&self) -> Option<&Value> {
        self.default_value.as_ref()
    }

    pub fn field_id(&self) -> i32 {
        self.field_id
    }

    pub fn schema_id(&self) -> u64 {
        self.schema_id
    }

    /// Resolve `out_field_name` and `determined_type` for all child fields.
    pub fn finalize_field_types(&mut self, flatten: bool) {
        AnalyseSchema::determine_field_types(&mut self.fields, None, flatten);
    }

    /// Returns (field_name, determined_type_name, nullable) for each child field.
    pub fn field_details(&self) -> Vec<(String, String, bool)> {
        self.fields
            .iter()
            .map(|(key, m)| {
                let name = if m.out_field_name.is_empty() {
                    key.clone()
                } else {
                    m.out_field_name.clone()
                };

                let type_name = if m.determined_type == SkipprDataType::Unknown {
                    Self::infer_type_from_counters(&m.types)
                } else {
                    m.determined_type.as_str().to_string()
                };

                (name, type_name, m.nullable)
            })
            .collect()
    }

    pub fn migrate_tree(
        namespace: &str,
        metadata: &mut Metadata,
        schema_id: u64,
        mut path: Vec<String>,
    ) -> bool {
        let mut changed = false;
        let name = if metadata.out_field_name.is_empty() {
            path.last().cloned().unwrap_or_default()
        } else {
            metadata.out_field_name.clone()
        };
        let source_field_name = path.last().cloned().unwrap_or_else(|| name.clone());
        if metadata.source_field_name.is_empty() && !source_field_name.is_empty() {
            metadata.source_field_name = source_field_name;
            changed = true;
        }
        if !name.is_empty() && path.last() != Some(&name) {
            path.push(name);
        }
        if metadata.schema_id == 0 {
            metadata.schema_id = schema_id;
            changed = true;
        }
        if metadata.field_id == 0 && !path.is_empty() {
            metadata.field_id = crate::lineage::deterministic_field_id(namespace, &path);
            changed = true;
        }
        if metadata.lineage_id.is_empty() && !path.is_empty() {
            metadata.lineage_id = format!("{}:{}", namespace, path.join("."));
            changed = true;
        }
        for (field_name, child) in metadata.fields.iter_mut() {
            let mut child_path = path.clone();
            if child.out_field_name.is_empty() {
                child_path.push(field_name.clone());
            }
            if Metadata::migrate_tree(namespace, child, schema_id, child_path) {
                changed = true;
            }
        }
        changed
    }

    fn infer_type_from_counters(types: &HashMap<SkipprDataType, u32>) -> String {
        types
            .iter()
            .filter(|(dt, _)| **dt != SkipprDataType::Null)
            .max_by_key(|(_, count)| *count)
            .map(|(dt, _)| dt.as_str().to_string())
            .unwrap_or_else(|| "string".to_string())
    }

    pub fn flatten_metadata(metadata: &Metadata, flattened: &mut OutputMetadata) {
        Self::_flatten_metadata(metadata, flattened, "".to_string());
    }

    fn _flatten_metadata(metadata: &Metadata, flattened: &mut OutputMetadata, field_path: String) {
        for (_key, val) in metadata.fields.iter() {
            // Special handling for array elements
            if val.determined_type == SkipprDataType::Array && val.fields.contains_key("0") {
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

                        if sub_val.determined_type == SkipprDataType::Record
                            || sub_val.determined_type == SkipprDataType::Map
                            || sub_val.determined_type == SkipprDataType::Array
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
                            el.source_field_name = sub_val.source_field_name.clone();
                            el.determined_type = sub_val.determined_type.clone();
                            el.determined_type_values = sub_val.determined_type_values.clone();
                            el.field_id = sub_val.field_id;
                            el.schema_id = sub_val.schema_id;
                            el.lineage_id = sub_val.lineage_id.clone();
                            el.nullable = sub_val.nullable;
                            el.default_value = sub_val.default_value.clone();

                            flattened.fields.insert(field_element_path.clone(), el);
                        }
                    }
                }
            } else if val.determined_type == SkipprDataType::Array && val.fields.is_empty() {
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
                el.source_field_name = val.source_field_name.clone();
                el.determined_type = val.determined_type.clone();
                el.determined_type_values = val.determined_type_values.clone();
                el.field_id = val.field_id;
                el.schema_id = val.schema_id;
                el.lineage_id = val.lineage_id.clone();
                el.nullable = val.nullable;
                el.default_value = val.default_value.clone();

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

                if val.determined_type == SkipprDataType::Record
                    || val.determined_type == SkipprDataType::Map
                    || val.determined_type == SkipprDataType::Array
                {
                    Self::_flatten_metadata(val, flattened, new_field_path);
                } else {
                    let mut el = OutputMetadata::new();
                    el.out_field_name = new_field_path.clone();
                    el.source_field_name = val.source_field_name.clone();
                    el.determined_type = val.determined_type.clone();
                    el.determined_type_values = val.determined_type_values.clone();
                    el.field_id = val.field_id;
                    el.schema_id = val.schema_id;
                    el.lineage_id = val.lineage_id.clone();
                    el.nullable = val.nullable;
                    el.default_value = val.default_value.clone();

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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SkipprDataType {
    Record,
    Map,
    Array,
    Date,
    String,
    Long,
    Integer,
    Short,
    Byte,
    Double,
    Float,
    Decimal,
    Boolean,
    TimestampMilli,
    Timestamp,
    Time,
    Binary,
    Uuid,
    Fixed,
    Json,
    Null,
    Unknown,
}

impl serde::Serialize for SkipprDataType {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for SkipprDataType {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = <std::string::String as serde::Deserialize>::deserialize(deserializer)?;
        Ok(SkipprDataType::from_str(&s))
    }
}

impl std::fmt::Display for SkipprDataType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Default for SkipprDataType {
    fn default() -> Self {
        SkipprDataType::Unknown
    }
}

pub(crate) mod serde_opt_data_type {
    use super::SkipprDataType;
    use serde::{self, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(
        value: &Option<SkipprDataType>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(dt) => serializer.serialize_str(dt.as_str()),
            None => serializer.serialize_str(""),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<SkipprDataType>, D::Error> {
        let s = String::deserialize(deserializer)?;
        if s.is_empty() {
            Ok(None)
        } else {
            let dt = SkipprDataType::from_str(&s);
            if dt == SkipprDataType::Unknown {
                Ok(None)
            } else {
                Ok(Some(dt))
            }
        }
    }
}

impl SkipprDataType {
    pub fn from_str(data_type: &str) -> Self {
        match data_type {
            "record" | "struct" => SkipprDataType::Record,
            "map" => SkipprDataType::Map,
            "array" => SkipprDataType::Array,
            "date" => SkipprDataType::Date,
            "string" => SkipprDataType::String,
            "long" | "bigint" => SkipprDataType::Long,
            "int" | "integer" => SkipprDataType::Integer,
            "short" | "smallint" => SkipprDataType::Short,
            "byte" | "tinyint" => SkipprDataType::Byte,
            "double" => SkipprDataType::Double,
            "float" => SkipprDataType::Float,
            "decimal" | "numeric" => SkipprDataType::Decimal,
            "boolean" => SkipprDataType::Boolean,
            "timestamp_milli" => SkipprDataType::TimestampMilli,
            "timestamp" => SkipprDataType::Timestamp,
            "time" => SkipprDataType::Time,
            "binary" => SkipprDataType::Binary,
            "uuid" => SkipprDataType::Uuid,
            "fixed" => SkipprDataType::Fixed,
            "json" => SkipprDataType::Json,
            "null" | "NULL" => SkipprDataType::Null,
            _ => SkipprDataType::Unknown,
        }
    }

    pub fn from_string(s: &str) -> Option<SkipprDataType> {
        match s.to_lowercase().as_str() {
            "string" => Some(SkipprDataType::String),
            "integer" | "int" => Some(SkipprDataType::Integer),
            "long" | "bigint" => Some(SkipprDataType::Long),
            "short" | "smallint" => Some(SkipprDataType::Short),
            "byte" | "tinyint" => Some(SkipprDataType::Byte),
            "double" => Some(SkipprDataType::Double),
            "float" => Some(SkipprDataType::Float),
            "decimal" | "numeric" => Some(SkipprDataType::Decimal),
            "boolean" => Some(SkipprDataType::Boolean),
            "date" => Some(SkipprDataType::Date),
            "timestamp" => Some(SkipprDataType::Timestamp),
            "timestamp_milli" => Some(SkipprDataType::TimestampMilli),
            "time" => Some(SkipprDataType::Time),
            "binary" => Some(SkipprDataType::Binary),
            "uuid" => Some(SkipprDataType::Uuid),
            "fixed" => Some(SkipprDataType::Fixed),
            "json" => Some(SkipprDataType::Json),
            "array" => Some(SkipprDataType::Array),
            "map" => Some(SkipprDataType::Map),
            "record" | "struct" => Some(SkipprDataType::Record),
            "null" => Some(SkipprDataType::Null),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            SkipprDataType::Record => "record",
            SkipprDataType::Map => "map",
            SkipprDataType::Array => "array",
            SkipprDataType::Date => "date",
            SkipprDataType::String => "string",
            SkipprDataType::Long => "long",
            SkipprDataType::Integer => "integer",
            SkipprDataType::Short => "short",
            SkipprDataType::Byte => "byte",
            SkipprDataType::Double => "double",
            SkipprDataType::Float => "float",
            SkipprDataType::Decimal => "decimal",
            SkipprDataType::Boolean => "boolean",
            SkipprDataType::TimestampMilli => "timestamp_milli",
            SkipprDataType::Timestamp => "timestamp",
            SkipprDataType::Time => "time",
            SkipprDataType::Binary => "binary",
            SkipprDataType::Uuid => "uuid",
            SkipprDataType::Fixed => "fixed",
            SkipprDataType::Json => "json",
            SkipprDataType::Null => "null",
            SkipprDataType::Unknown => "unknown",
        }
    }
}

const DATE_FIELD_VALIDATION_MIN_SAMPLE: i32 = 100;

/// True when `s` is a plain fixed-point decimal (optional sign, digits, single dot, fractional digits).
/// Excludes scientific notation and multi-dot strings (e.g. versions).
fn looks_like_fixed_point_decimal(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.contains('e') || s.contains('E') {
        return false;
    }
    let mut parts = s.split('.');
    let whole = parts.next().unwrap_or("");
    let frac = match parts.next() {
        Some(f) => f,
        None => return false,
    };
    if parts.next().is_some() {
        return false;
    }
    if frac.is_empty() {
        return false;
    }
    if !frac.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let whole_trim = whole.trim_start_matches('+').trim_start_matches('-');
    if whole_trim.is_empty() {
        return false;
    }
    whole_trim.chars().all(|c| c.is_ascii_digit())
}

fn get_type(value: &str) -> SkipprDataType {
    let _foo = "";

    match value.parse::<i32>() {
        Ok(_bool) => {
            return SkipprDataType::Integer;
        }
        Err(..) => {
            let timmed_value = value.trim_matches('"');
            let json_value: Result<i32, _> = serde_json::from_str(timmed_value);
            match json_value {
                Ok(_) => {
                    return SkipprDataType::Integer;
                }
                Err(_) => {}
            }
        }
    }

    match value.parse::<i64>() {
        Ok(_bool) => {
            return SkipprDataType::Long;
        }
        Err(..) => {
            let timmed_value = value.trim_matches('"');
            let json_value: Result<i128, _> = serde_json::from_str(timmed_value);
            match json_value {
                Ok(_) => {
                    return SkipprDataType::Long;
                }
                Err(_) => {}
            }
        }
    }

    // Fixed-point decimals ("50.00", JSON number 50.25 rendered as "50.25") must be Decimal so
    // sinks like Snowflake emit NUMBER(p,s) instead of DOUBLE.
    let trimmed_for_decimal = value.trim_matches('"');
    if looks_like_fixed_point_decimal(trimmed_for_decimal) {
        return SkipprDataType::Decimal;
    }

    match &value.parse::<f32>() {
        Ok(_bool) => {
            return SkipprDataType::Double;
        }
        Err(_) => {
            let timmed_value = value.trim_matches('"');
            let json_value: Result<f32, _> = serde_json::from_str(timmed_value);
            match json_value {
                Ok(_) => {
                    return SkipprDataType::Double;
                }
                Err(_) => {}
            }
        }
    }

    let v: Value = serde_json::from_str(value).unwrap_or_default();
    match v.is_array().then_some(true) {
        Some(_bool) => {
            return SkipprDataType::Array;
        }
        None => {}
    }

    match v.is_object().then_some(true) {
        Some(_bool) => {
            return SkipprDataType::Array;
        }
        None => {}
    }

    match value.parse::<bool>() {
        Ok(_bool) => {
            return SkipprDataType::Boolean;
        }
        Err(_string) => {}
    }

    match value.parse::<String>() {
        Ok(_bool) => {
            return SkipprDataType::String;
        }
        Err(_string) => {}
    }

    SkipprDataType::Unknown
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
        namespace_override: Option<&str>,
    ) -> u64 {
        let counts = self.infer_json_schema_from_iterator(
            str,
            metadata,
            max_read_records,
            namespace_override,
        );
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
        namespace_override: Option<&str>,
    ) -> u64 {
        let mut parse_namespace_cache: HashMap<String, String> = HashMap::new();

        let mut counts = 0;

        let mut _skpr_namespace: String = "".to_string();
        let pipeline_name = namespace_override
            .map(|s| s.to_string())
            .unwrap_or_else(|| Config::get_pipeline_name());

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
                    let mut record = v;
                    counts += 1;

                    self.analyse_payload(
                        &mut record,
                        &mut metadata.get_mut(&_skpr_namespace).unwrap().fields,
                    );
                }
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
                if field_metadata.determined_type == SkipprDataType::Array
                    && field_metadata.determined_type_values == Some(SkipprDataType::Record)
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
    ) -> SkipprDataType {
        self.init_discovered_type(metadata, field);

        let mut data_type = self.get_logical_type(field, value, metadata, true);

        if data_type == SkipprDataType::Array
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

                    let mut _logical_type = SkipprDataType::Unknown;

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

            if type_count.contains_key(&SkipprDataType::Record) {
                // If the value is actually an array (JSON array) but contains records,
                // it should be identified as an array of records, not just a record
                if value.is_array() {
                    data_type = SkipprDataType::Array;
                } else {
                    data_type = SkipprDataType::Record;
                }
            } else if is_sequential {
                // array of sequential int keys is an avro array
                data_type = SkipprDataType::Array;
            } else
            // if type_count.len() > 1
            {
                data_type = SkipprDataType::Record;
            }

            // Set the initial repetition_count for arrays of records
            if data_type == SkipprDataType::Array && value.is_array() {
                // Check if this is an array of records by examining the first element
                let array_values = value.as_array().unwrap();
                if !array_values.is_empty() && array_values[0].is_object() {
                    let array_length = array_values.len() as i32;
                    if let Some(field_metadata) = metadata.get_mut(field) {
                        field_metadata.determined_type_values = Some(SkipprDataType::Record);
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
    ) -> SkipprDataType {
        // let value: &mut String = &mut json_value.as_str().unwrap().to_string();
        let value: &mut String = &mut json_value.to_string();

        let mut data_type = get_type(value);

        if data_type == SkipprDataType::String || data_type == SkipprDataType::Integer {
            data_type = self.check_string_or_int(value);
        }

        if matches!(data_type, SkipprDataType::Integer | SkipprDataType::Long) && allow_date {
            let mut valid_timestamp = false;

            if data_type == SkipprDataType::Integer {
                valid_timestamp = self.is_valid_timestamp(value);
                if valid_timestamp {
                    data_type = SkipprDataType::Timestamp;
                }
            } else if data_type == SkipprDataType::Long {
                valid_timestamp = self.is_valid_timestamp_milli(value);
                if valid_timestamp {
                    data_type = SkipprDataType::TimestampMilli;
                }
            }

            if valid_timestamp {
                self.increment_date_field_candidate_count(field, metadata, &"".to_string());
            }
        }

        if data_type == SkipprDataType::String && allow_date {
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
                    data_type = SkipprDataType::Date;
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
                data_type = SkipprDataType::Date;
            }
        }

        // @todo - we don't support int bool anymore
        if data_type == SkipprDataType::Integer || data_type == SkipprDataType::String {
            match parse_bool(value) {
                Err(_i32) => {
                    // println!("Not float");
                }
                Ok(_bool) => {
                    // println!("Is float");
                    return SkipprDataType::Boolean;
                }
            }
        }
        // if data_type != "double" && is_bool(filter_var(value, FILTER_VALIDATE_BOOLEAN, FILTER_NULL_ON_FAILURE)) {
        //     data_type = "boolean".to_string();
        // }

        if data_type == SkipprDataType::Null {
            // most systems won't support null
            data_type = SkipprDataType::String;
        }
        // @todo - logical interpretation based on field name

        data_type
    }

    pub fn check_string_or_int(&self, value: &mut String) -> SkipprDataType {
        let mut data_type = get_type(value);

        // check is_32_bit_signed_int or is_64_bit_signed_int

        if data_type == SkipprDataType::String {
            if self.is_32_bit_signed_int(value) {
                data_type = SkipprDataType::Integer;
            } else if self.is_64_bit_signed_int(value) {
                data_type = SkipprDataType::Long;
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
        let ts = v.as_i64().unwrap();
        if ts < 10_000_000_000 {
            match ts.checked_mul(1000) {
                Some(millis) => millis.into(),
                None => v,
            }
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
        data_type: &SkipprDataType,
        value: &mut String,
    ) {
        if metadata.get(field).unwrap().types.get(data_type).is_none() {
            metadata
                .get_mut(field)
                .unwrap()
                .types
                .insert(data_type.clone(), 1);
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
                .insert(data_type.clone(), new_count);
        }

        // I found in practice theres too many false possitives for array values types of timestamp
        // @todo - probably better handeled in determine_field_types, not sure why it isn't already working
        // if metadata.get(field).unwrap().parent_type != "array" {
        if *data_type == SkipprDataType::Integer {
            let valid_timestamp = self.is_valid_timestamp(value);
            if valid_timestamp {
                self.set_discovered_occurrence(metadata, field, &SkipprDataType::Timestamp, value);
            }
        } else if *data_type == SkipprDataType::Long {
            let valid_timestamp = self.is_valid_timestamp_milli(value);
            if valid_timestamp {
                self.set_discovered_occurrence(
                    metadata,
                    field,
                    &SkipprDataType::TimestampMilli,
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
                field.parent_type = Some(SkipprDataType::from_str(parent_type));
            }

            if field.source_field_name.is_empty() {
                field.source_field_name = field_name.to_string();
            }
            field.out_field_name = Helpers::clean_field_name(field_name.to_string());

            if field.determined_type == SkipprDataType::Unknown {
                let mut highest_type = SkipprDataType::Unknown;
                let mut highest_count = 0;

                if !field.types.is_empty() {
                    // Don't allow NULL type if we discovered any other types
                    if field.types.len() > 1 {
                        field.types.remove(&SkipprDataType::Null);
                    }

                    // force to record type over map or array if ever present
                    if field.types.contains_key(&SkipprDataType::Record) {
                        field.determined_type = SkipprDataType::Record;
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
                                highest_type = data_type.clone();
                                highest_count = *data_type_count;
                                // }
                            }
                        }

                        field.determined_type = highest_type;
                    }
                }
            }

            if field.determined_type != SkipprDataType::Unknown
                && matches!(
                    field.determined_type,
                    SkipprDataType::Map | SkipprDataType::Array | SkipprDataType::Record
                )
                && field.fields.len() > 0
            {
                if field.determined_type_values.is_none()
                    && (field.determined_type == SkipprDataType::Array
                        || field.determined_type == SkipprDataType::Map)
                {
                    // field.determined_type_values = "".to_string();

                    // Ignore sub-fields for Avro array, the values are just enumerated, their not fields themselves.
                    // Else we'd create a field list with string keys for each array value
                    // e.g. [1,5,3,7,4,3,5]
                    // would incorrectly become ['a0' => 1, 'a1' => 5, ...]

                    let mut type_count: HashMap<SkipprDataType, u32> = HashMap::new();

                    // @todo - not intended to build avro type array here
                    //         however, 'array' type is a special case... how to handle?

                    // Get avro arrays items primitive data type
                    for (_sub_field, sub_value) in field.fields.iter() {
                        for (data_type, data_type_count) in sub_value.types.iter() {
                            if type_count.get(data_type).is_none() {
                                type_count.insert(data_type.clone(), *data_type_count);
                            } else {
                                *type_count.get_mut(data_type).unwrap() += data_type_count;
                            }
                        }
                    }

                    let mut values_type: Option<SkipprDataType> = None;

                    if !type_count.is_empty() {
                        values_type = type_count
                            .iter()
                            .max_by(|a, b| a.1.cmp(b.1))
                            .map(|(k, _v)| k.clone());
                    }

                    // println!("HIGHEST TYPE: {:?}", values_type);

                    field.determined_type_values = values_type;

                    if field.determined_type == SkipprDataType::Array
                        && field.determined_type_values != Some(SkipprDataType::Record)
                        && field.determined_type_values != Some(SkipprDataType::Array)
                    {
                        field.fields.clear();
                    }
                }

                // println!("Field {} determined type is {}", field_name, field.determined_type);
                // println!("Field {} values type is {}", field_name, field.determined_type_values);

                if field.determined_type != SkipprDataType::Array
                    || (field.determined_type == SkipprDataType::Array
                        && matches!(
                            field.determined_type_values,
                            Some(SkipprDataType::Record) | Some(SkipprDataType::Array)
                        ))
                {
                    AnalyseSchema::determine_field_types(
                        &mut field.fields,
                        Some(field.determined_type.as_str()),
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
                    if metadata.parent_type.is_none() {
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
                    if value.determined_type != SkipprDataType::Unknown {
                        metadata.determined_type = value.determined_type.clone();
                    }
                    if value.determined_type_values.is_some() {
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
        assert_eq!(
            dummy.check_string_or_int(&mut value),
            SkipprDataType::Integer
        );

        let mut value = "-2147483648".to_string(); // min i32
        assert_eq!(
            dummy.check_string_or_int(&mut value),
            SkipprDataType::Integer
        );
    }

    #[test]
    fn test_64_bit_signed_int() {
        let dummy = AnalyseSchema { i: 0 };
        let mut value = "9223372036854775807".to_string(); // max i64
        assert_eq!(dummy.check_string_or_int(&mut value), SkipprDataType::Long);

        let mut value = "-9223372036854775808".to_string(); // min i64
        assert_eq!(dummy.check_string_or_int(&mut value), SkipprDataType::Long);
    }

    #[test]
    fn test_non_integer() {
        let dummy = AnalyseSchema { i: 0 };
        let mut value = "Hello".to_string();
        assert_eq!(
            dummy.check_string_or_int(&mut value),
            SkipprDataType::String
        );
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

    use crate::discover::{get_type, SkipprDataType};

    #[test]
    fn test_get_type_int() {
        let expected_type = SkipprDataType::Boolean;

        let subject = 123;
        assert_ne!(get_type(&mut subject.to_string()), expected_type);
    }

    #[test]
    fn test_get_type_true_int() {
        let expected_type = SkipprDataType::Boolean;

        let subject = 1;
        assert_ne!(get_type(&mut subject.to_string()), expected_type);
    }

    #[test]
    fn test_get_type_false_int() {
        let expected_type = SkipprDataType::Boolean;

        let subject = 0;
        assert_ne!(get_type(&mut subject.to_string()), expected_type);
    }

    #[test]
    fn test_get_type_true_bool() {
        let expected_type = SkipprDataType::Boolean;

        let subject = true;
        assert_eq!(get_type(&mut subject.to_string()), expected_type);
    }

    #[test]
    fn test_get_type_false_bool() {
        let expected_type = SkipprDataType::Boolean;

        let subject = false;
        assert_eq!(get_type(&mut subject.to_string()), expected_type);
    }

    #[test]
    fn test_get_type_true_str() {
        let expected_type = SkipprDataType::Boolean;

        let subject = "true";
        assert_eq!(get_type(&mut subject.to_string()), expected_type);
    }

    #[test]
    fn test_get_type_false_str() {
        let expected_type = SkipprDataType::Boolean;

        let subject = "false";
        assert_eq!(get_type(&mut subject.to_string()), expected_type);
    }

    #[test]
    fn test_get_type_true_upper_str() {
        let expected_type = SkipprDataType::String;

        let subject = "True";
        assert_eq!(get_type(&mut subject.to_string()), expected_type);
    }

    #[test]
    fn test_get_type_false_upper_str() {
        let expected_type = SkipprDataType::String;

        let subject = "False";
        assert_eq!(get_type(&mut subject.to_string()), expected_type);
    }

    #[test]
    fn test_get_type_yes_str() {
        let expected_type = SkipprDataType::String;

        let subject = "yes";
        assert_eq!(get_type(&mut subject.to_string()), expected_type);
    }

    #[test]
    fn test_get_type_no_str() {
        let expected_type = SkipprDataType::String;

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

    use crate::discover::{AnalyseSchema, Metadata, SkipprDataType};
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

        AnalyseSchema::infer_json_schema(&_foo, &mut record_line, Some(1), &mut _fields, None);

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
            SkipprDataType::Array
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("abc1")
                .unwrap()
                .determined_type_values,
            Some(SkipprDataType::Integer)
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("abc2")
                .unwrap()
                .determined_type,
            SkipprDataType::Array
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("abc2")
                .unwrap()
                .determined_type_values,
            Some(SkipprDataType::String)
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
            SkipprDataType::Array
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("abc7")
                .unwrap()
                .determined_type,
            SkipprDataType::Record
        );

        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("abc8")
                .unwrap()
                .determined_type,
            SkipprDataType::Array
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("abc8")
                .unwrap()
                .determined_type_values,
            Some(SkipprDataType::Record)
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

        AnalyseSchema::infer_json_schema(&_foo, &mut record_line, Some(1), &mut _fields, None);

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
            SkipprDataType::Array
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("boolean") // I recall we stopped infering bool ints as boolean, it was too error prone and probably trying to be too smart
                .unwrap()
                .determined_type_values,
            Some(SkipprDataType::Integer)
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("boolean2")
                .unwrap()
                .determined_type,
            SkipprDataType::Array
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("boolean2")
                .unwrap()
                .determined_type_values,
            Some(SkipprDataType::Boolean)
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
            SkipprDataType::Array
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("timestamp")
                .unwrap()
                .determined_type_values,
            Some(SkipprDataType::Timestamp)
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("timestamp_milli")
                .unwrap()
                .determined_type,
            SkipprDataType::Array
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("timestamp_milli")
                .unwrap()
                .determined_type_values,
            Some(SkipprDataType::TimestampMilli)
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("abc3")
                .unwrap()
                .determined_type,
            SkipprDataType::Array
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("abc3")
                .unwrap()
                .determined_type_values,
            Some(SkipprDataType::Integer)
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("abc4")
                .unwrap()
                .determined_type,
            SkipprDataType::Array
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("abc4")
                .unwrap()
                .determined_type_values,
            Some(SkipprDataType::Integer)
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

        AnalyseSchema::infer_json_schema(&_foo, &mut record_line, Some(1), &mut _fields, None);

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
            SkipprDataType::String
        );
        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("arable")
                .unwrap()
                .determined_type,
            SkipprDataType::Boolean
        );

        assert_eq!(
            _fields
                .get("default")
                .unwrap()
                .fields
                .get("crank")
                .unwrap()
                .determined_type,
            SkipprDataType::Record
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
            SkipprDataType::Array
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
            Some(SkipprDataType::Integer)
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
            SkipprDataType::Record
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
            SkipprDataType::Array
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
            SkipprDataType::Array
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
            Some(SkipprDataType::Array)
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
            SkipprDataType::Array
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
            SkipprDataType::Record
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
            parent_type: Some(SkipprDataType::Record),
            fields: Box::new(HashMap::new()),
            date_candidate: None,
            date_parser_kind: None,
            timezone: false,
            evolution: Box::new(HashMap::new()),
            enabled: true,
            out_field_name: "child".to_string(),
            determined_type: SkipprDataType::String,
            determined_type_values: None,
            repetition_count: 1,
            ..Metadata::new().unwrap()
        };

        fields.insert("child".to_string(), metadata_child.clone());

        let metadata = Metadata {
            count: 1,
            types: HashMap::new(),
            parent_type: None,
            fields: fields,
            date_candidate: None,
            date_parser_kind: None,
            timezone: false,
            evolution: Box::new(HashMap::new()),
            enabled: true,
            out_field_name: "parent".to_string(),
            determined_type: SkipprDataType::Record,
            determined_type_values: None,
            repetition_count: 1,
            ..Metadata::new().unwrap()
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
                parent_type: None,
                fields: Box::new(HashMap::new()),
                date_candidate: None,
                date_parser_kind: None,
                timezone: false,
                evolution: Box::new(HashMap::new()),
                enabled: true,
                out_field_name: "".into(),
                determined_type: SkipprDataType::Unknown,
                determined_type_values: None,
                repetition_count: 1,
                ..Metadata::new().unwrap()
            },
        );
        metadata.get_mut("schema").unwrap().fields.insert(
            "contacts".into(),
            Metadata {
                count: 1,
                types: HashMap::new(),
                parent_type: None,
                fields: Box::new(HashMap::new()),
                date_candidate: None,
                date_parser_kind: None,
                timezone: false,
                evolution: Box::new(HashMap::new()),
                enabled: true,
                out_field_name: "contacts".into(),
                determined_type: SkipprDataType::Array,
                determined_type_values: None,
                repetition_count: 2,
                ..Metadata::new().unwrap()
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
                    parent_type: None,
                    fields: Box::new(HashMap::new()),
                    date_candidate: None,
                    date_parser_kind: None,
                    timezone: false,
                    evolution: Box::new(HashMap::new()),
                    enabled: true,
                    out_field_name: "0".into(),
                    determined_type: SkipprDataType::String,
                    determined_type_values: None,
                    repetition_count: 1,
                    ..Metadata::new().unwrap()
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
                    parent_type: None,
                    fields: Box::new(HashMap::new()),
                    date_candidate: None,
                    date_parser_kind: None,
                    timezone: false,
                    evolution: Box::new(HashMap::new()),
                    enabled: true,
                    out_field_name: "name".into(),
                    determined_type: SkipprDataType::String,
                    determined_type_values: None,
                    repetition_count: 1,
                    ..Metadata::new().unwrap()
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
                    parent_type: None,
                    fields: Box::new(HashMap::new()),
                    date_candidate: None,
                    date_parser_kind: None,
                    timezone: false,
                    evolution: Box::new(HashMap::new()),
                    enabled: true,
                    out_field_name: "tel".into(),
                    determined_type: SkipprDataType::Integer,
                    determined_type_values: None,
                    repetition_count: 1,
                    ..Metadata::new().unwrap()
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
            SkipprDataType::String
        );
        assert_eq!(
            flattened
                .fields
                .get("contacts_0_tel")
                .unwrap()
                .determined_type,
            SkipprDataType::Integer
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
            SkipprDataType::String
        );
        assert_eq!(
            flattened
                .fields
                .get("contacts_1_tel")
                .unwrap()
                .determined_type,
            SkipprDataType::Integer
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
        array_field.determined_type = SkipprDataType::Array;
        array_field.determined_type_values = Some(SkipprDataType::Double);
        array_field.enabled = true;

        fields.insert("x_axis_linear_mean".to_string(), array_field.clone());

        // Create the parent record
        let mut metadata = Metadata::new().unwrap();
        metadata.out_field_name = "imu".to_string();
        metadata.determined_type = SkipprDataType::Record;
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
        assert_eq!(field.determined_type, SkipprDataType::Array);
        assert_eq!(field.determined_type_values, Some(SkipprDataType::Double));
    }
}

/// End-to-end roundtrip tests: JSON -> discover -> evolve -> Arrow -> Hive.
#[cfg(test)]
mod tests_roundtrip {
    use super::*;
    use crate::converters::skippr_arrow::convert_skippr_to_arrow;
    use crate::converters::skippr_hive::SkipprHive;
    use crate::discover::evolution::Evolution;
    use crate::ingest::ingest::discover_ingest;
    use serde_json::json;

    #[test]
    fn persisted_metadata_migration_sets_ids_and_defaults() {
        let mut root = Metadata::new().unwrap();
        let mut field = Metadata::new_with_type(SkipprDataType::String, "customer_id");
        field.nullable = false;
        root.fields.insert("customer_id".to_string(), field);
        let mut pipeline = PipelineMetadata {
            name: "pipeline".to_string(),
            metadata: HashMap::from([("orders".to_string(), root)]),
            sql: None,
            enabled: true,
            flattened: false,
            metadata_version: 0,
            schema_id: 0,
        };

        assert!(pipeline.migrate_persisted_metadata());
        let migrated = pipeline
            .metadata
            .get("orders")
            .unwrap()
            .fields
            .get("customer_id")
            .unwrap();
        assert_eq!(pipeline.metadata_version, 2);
        assert_eq!(pipeline.schema_id, 1);
        assert_ne!(migrated.field_id, 0);
        assert_eq!(migrated.schema_id, 1);
        assert_eq!(migrated.lineage_id, "orders:customer_id");
        assert!(!migrated.nullable);
        assert!(migrated.default_value.is_none());
        assert!(!pipeline.migrate_persisted_metadata());
    }

    fn discover_field(
        field: &str,
        value: &serde_json::Value,
        metadata: &mut HashMap<String, Metadata>,
    ) {
        let mut updated = "no".to_string();
        discover_ingest(field, value, None, None, metadata, &mut updated);
    }

    fn build_output(metadata: &HashMap<String, Metadata>) -> Box<HashMap<String, OutputMetadata>> {
        let mut out = HashMap::new();
        for (k, v) in metadata.iter() {
            if v.enabled {
                out.insert(k.clone(), OutputMetadata::from_metadata(v));
            }
        }
        Box::new(out)
    }

    fn build_hive_root(metadata: &HashMap<String, Metadata>) -> OutputMetadata {
        let mut root = OutputMetadata::new();
        root.determined_type = SkipprDataType::Record;
        let mut fields = HashMap::new();
        for (k, v) in metadata.iter() {
            fields.insert(k.clone(), OutputMetadata::from_metadata(v));
        }
        root.fields = Box::new(fields);
        root
    }

    #[test]
    fn flat_string_to_integer_evolution() {
        let mut metadata: HashMap<String, Metadata> = HashMap::new();
        discover_field("x", &json!("hello"), &mut metadata);
        assert_eq!(
            metadata.get("x").unwrap().determined_type,
            SkipprDataType::String
        );

        let mut updated = "no".to_string();
        let r = Evolution::evolve_field(
            &"x".to_string(),
            &json!(42),
            None,
            None,
            &mut metadata,
            &mut updated,
            false,
        );
        assert!(r.is_ok());
        assert!(!metadata.get("x").unwrap().evolution.is_empty());

        let arrow = convert_skippr_to_arrow(build_output(&metadata));
        assert!(arrow.is_ok());
        assert!(arrow.unwrap().fields().len() >= 1);
    }

    #[test]
    fn flat_string_to_record_evolution() {
        let mut metadata: HashMap<String, Metadata> = HashMap::new();
        discover_field("data", &json!("simple_string"), &mut metadata);

        let mut updated = "no".to_string();
        let r = Evolution::evolve_field(
            &"data".to_string(),
            &json!({"name": "alice", "age": 30}),
            None,
            None,
            &mut metadata,
            &mut updated,
            false,
        );
        assert!(r.is_ok());
        assert!(metadata.contains_key("data_record"));
        assert_eq!(
            metadata.get("data_record").unwrap().determined_type,
            SkipprDataType::Record
        );

        let arrow = convert_skippr_to_arrow(build_output(&metadata));
        assert!(arrow.is_ok());
    }

    #[test]
    fn nested_child_type_change() {
        let mut metadata: HashMap<String, Metadata> = HashMap::new();
        discover_field("outer", &json!({"inner": "hello"}), &mut metadata);
        assert_eq!(
            metadata.get("outer").unwrap().determined_type,
            SkipprDataType::Record
        );
        assert_eq!(
            metadata
                .get("outer")
                .unwrap()
                .fields
                .get("inner")
                .unwrap()
                .determined_type,
            SkipprDataType::String
        );

        let mut updated = "no".to_string();
        if let Some(outer_mut) = metadata.get_mut("outer") {
            let _ = Evolution::evolve_field(
                &"inner".to_string(),
                &json!(3.14),
                Some("outer"),
                Some("record"),
                &mut outer_mut.fields,
                &mut updated,
                false,
            );
        }

        let arrow = convert_skippr_to_arrow(build_output(&metadata));
        assert!(arrow.is_ok());
    }

    #[test]
    fn array_discovery_and_arrow() {
        let mut metadata: HashMap<String, Metadata> = HashMap::new();
        discover_field("tags", &json!(["red", "green", "blue"]), &mut metadata);
        assert_eq!(
            metadata.get("tags").unwrap().determined_type,
            SkipprDataType::Array
        );

        let arrow = convert_skippr_to_arrow(build_output(&metadata));
        assert!(arrow.is_ok());
    }

    #[test]
    fn multiple_evolutions_same_field() {
        let mut metadata: HashMap<String, Metadata> = HashMap::new();
        discover_field("val", &json!("text"), &mut metadata);

        let mut updated = "no".to_string();
        let r1 = Evolution::evolve_field(
            &"val".to_string(),
            &json!(42i64),
            None,
            None,
            &mut metadata,
            &mut updated,
            false,
        );
        assert!(r1.is_ok());

        let r2 = Evolution::evolve_field(
            &"val".to_string(),
            &json!(3.14),
            None,
            None,
            &mut metadata,
            &mut updated,
            false,
        );
        assert!(r2.is_ok());

        assert!(metadata.get("val").unwrap().evolution.len() >= 2);

        let arrow = convert_skippr_to_arrow(build_output(&metadata));
        assert!(arrow.is_ok());
    }

    #[test]
    fn hive_schema_generation() {
        let mut metadata: HashMap<String, Metadata> = HashMap::new();
        discover_field("name", &json!("alice"), &mut metadata);
        discover_field("age", &json!(30), &mut metadata);
        discover_field("score", &json!(99.5), &mut metadata);
        discover_field("active", &json!(true), &mut metadata);

        let root = build_hive_root(&metadata);
        let hive = SkipprHive::convert_skippr_to_hive(&root);
        assert!(hive.is_ok());
        assert!(hive.unwrap().len() >= 4);
    }

    #[test]
    fn full_pipeline_discover_evolve_arrow_hive() {
        let mut metadata: HashMap<String, Metadata> = HashMap::new();
        discover_field("id", &json!(1), &mut metadata);
        discover_field("name", &json!("alice"), &mut metadata);
        discover_field("meta", &json!("some_string"), &mut metadata);

        let mut updated = "no".to_string();
        let _ = Evolution::evolve_field(
            &"meta".to_string(),
            &json!({"key": "value", "count": 5}),
            None,
            None,
            &mut metadata,
            &mut updated,
            false,
        );

        let arrow = convert_skippr_to_arrow(build_output(&metadata));
        assert!(arrow.is_ok());
        assert!(arrow.unwrap().fields().len() >= 3);

        let root = build_hive_root(&metadata);
        let hive = SkipprHive::convert_skippr_to_hive(&root);
        assert!(hive.is_ok());
    }
}

/// Property-based fuzz tests for schema discovery.
#[cfg(test)]
mod tests_discover_proptest {
    use super::*;
    use crate::ingest::ingest::discover_ingest;
    use proptest::prelude::*;
    use serde_json::json;

    fn arb_json_leaf() -> impl Strategy<Value = serde_json::Value> {
        prop_oneof![
            Just(json!(null)),
            any::<bool>().prop_map(|b| json!(b)),
            any::<i32>().prop_map(|i| json!(i)),
            any::<i64>().prop_map(|i| json!(i)),
            (-1e15f64..1e15f64).prop_map(|f| json!(f)),
            "[a-zA-Z0-9_ ]{0,30}".prop_map(|s| json!(s)),
        ]
    }

    fn arb_json_value() -> BoxedStrategy<serde_json::Value> {
        let leaf = arb_json_leaf();
        leaf.prop_recursive(4, 32, 8, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..5)
                    .prop_map(|v| serde_json::Value::Array(v)),
                prop::collection::hash_map("[a-z]{1,6}", inner, 0..5).prop_map(|m| {
                    let obj: serde_json::Map<std::string::String, serde_json::Value> =
                        m.into_iter().collect();
                    serde_json::Value::Object(obj)
                }),
            ]
        })
        .boxed()
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(1000))]

        #[test]
        fn discover_ingest_never_panics(value in arb_json_value()) {
            let mut metadata: HashMap<std::string::String, Metadata> = HashMap::new();
            let mut updated = "no".to_string();
            let _ = discover_ingest("test_field", &value, None, None, &mut metadata, &mut updated);
        }

        #[test]
        fn discovered_type_is_valid(value in arb_json_value()) {
            let mut metadata: HashMap<std::string::String, Metadata> = HashMap::new();
            let mut updated = "no".to_string();
            discover_ingest("test_field", &value, None, None, &mut metadata, &mut updated);

            if let Some(md) = metadata.get("test_field") {
                let valid_types = [
                    SkipprDataType::String,
                    SkipprDataType::Integer,
                    SkipprDataType::Long,
                    SkipprDataType::Double,
                    SkipprDataType::Decimal,
                    SkipprDataType::Boolean,
                    SkipprDataType::Date,
                    SkipprDataType::Timestamp,
                    SkipprDataType::TimestampMilli,
                    SkipprDataType::Array,
                    SkipprDataType::Record,
                    SkipprDataType::Map,
                    SkipprDataType::Null,
                    SkipprDataType::Unknown,
                ];
                assert!(
                    valid_types.contains(&md.determined_type),
                    "discovered type {:?} should be a valid SkipprDataType",
                    md.determined_type
                );
            }
        }

        #[test]
        fn discover_then_evolve_never_panics(
            initial in arb_json_leaf(),
            breaking in arb_json_value(),
        ) {
            let mut metadata: HashMap<std::string::String, Metadata> = HashMap::new();
            let mut updated = "no".to_string();
            discover_ingest("f", &initial, None, None, &mut metadata, &mut updated);

            let _ = crate::discover::evolution::Evolution::evolve_field(
                &"f".to_string(),
                &breaking,
                None,
                None,
                &mut metadata,
                &mut updated,
                false,
            );
        }
    }
}
