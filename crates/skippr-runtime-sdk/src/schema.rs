use std::collections::HashMap;

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

pub mod serde_opt_data_type {
    use super::SkipprDataType;
    use serde::{Deserialize, Deserializer, Serializer};

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

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct OutputMetadata {
    #[serde(default)]
    pub source_field_name: String,
    pub out_field_name: String,
    pub determined_type: SkipprDataType,
    #[serde(with = "serde_opt_data_type")]
    pub determined_type_values: Option<SkipprDataType>,
    #[serde(default)]
    pub field_id: i32,
    #[serde(default)]
    pub schema_id: u64,
    #[serde(default)]
    pub lineage_id: String,
    #[serde(default = "default_nullable")]
    pub nullable: bool,
    #[serde(default)]
    pub default_value: Option<serde_json::Value>,
    pub fields: Box<HashMap<String, OutputMetadata>>,
}

fn default_nullable() -> bool {
    true
}

impl Default for OutputMetadata {
    fn default() -> Self {
        Self::new()
    }
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
}
