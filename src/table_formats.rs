use std::collections::BTreeMap;

use serde_derive::{Deserialize, Serialize};

use crate::discover::SkipprDataType;
use crate::lineage::FieldLineage;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct NullableDefault {
    #[serde(default = "crate::lineage::default_nullable")]
    pub nullable: bool,
    #[serde(default)]
    pub default_value: Option<serde_json::Value>,
}

impl Default for NullableDefault {
    fn default() -> Self {
        Self {
            nullable: true,
            default_value: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum LogicalType {
    Record,
    Map,
    Array,
    Boolean,
    Int8,
    Int16,
    Int32,
    Int64,
    Float32,
    Float64,
    Decimal {
        precision: u8,
        scale: u8,
    },
    Utf8,
    Binary,
    Fixed {
        length: u32,
    },
    Uuid,
    Json,
    Date,
    Time,
    Timestamp {
        unit: TimeUnit,
        timezone: Option<String>,
    },
    Null,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum TimeUnit {
    Second,
    Millisecond,
    Microsecond,
    Nanosecond,
}

impl LogicalType {
    pub fn from_skippr_type(value: &SkipprDataType) -> Self {
        match value {
            SkipprDataType::Record => Self::Record,
            SkipprDataType::Map => Self::Map,
            SkipprDataType::Array => Self::Array,
            SkipprDataType::Date => Self::Date,
            SkipprDataType::String => Self::Utf8,
            SkipprDataType::Long => Self::Int64,
            SkipprDataType::Integer => Self::Int32,
            SkipprDataType::Short => Self::Int16,
            SkipprDataType::Byte => Self::Int8,
            SkipprDataType::Double => Self::Float64,
            SkipprDataType::Float => Self::Float32,
            SkipprDataType::Decimal => Self::Decimal {
                precision: 38,
                scale: 9,
            },
            SkipprDataType::Boolean => Self::Boolean,
            SkipprDataType::TimestampMilli => Self::Timestamp {
                unit: TimeUnit::Millisecond,
                timezone: None,
            },
            SkipprDataType::Timestamp => Self::Timestamp {
                unit: TimeUnit::Millisecond,
                timezone: None,
            },
            SkipprDataType::Time => Self::Time,
            SkipprDataType::Binary => Self::Binary,
            SkipprDataType::Uuid => Self::Uuid,
            SkipprDataType::Fixed => Self::Fixed { length: 16 },
            SkipprDataType::Json => Self::Json,
            SkipprDataType::Null => Self::Null,
            SkipprDataType::Unknown => Self::Unknown,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct LogicalField {
    pub field_id: i32,
    pub name: String,
    pub path: Vec<String>,
    pub logical_type: LogicalType,
    pub nullability: NullableDefault,
    #[serde(default)]
    pub children: Vec<LogicalField>,
}

impl LogicalField {
    pub fn from_lineage(lineage: &FieldLineage) -> Self {
        Self {
            field_id: lineage.field_id,
            name: lineage
                .output_path
                .last()
                .cloned()
                .unwrap_or_else(|| lineage.skippr_namespace.clone()),
            path: lineage.output_path.clone(),
            logical_type: LogicalType::from_skippr_type(&lineage.data_type),
            nullability: NullableDefault {
                nullable: lineage.nullable,
                default_value: None,
            },
            children: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct IcebergTableIdentifier {
    pub catalog: String,
    pub namespace: Vec<String>,
    pub table: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct IcebergPartitionField {
    pub source_field_id: i32,
    pub field_id: i32,
    pub name: String,
    pub transform: IcebergPartitionTransform,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum IcebergPartitionTransform {
    Identity,
    Year,
    Month,
    Day,
    Hour,
    Bucket { buckets: u32 },
    Truncate { width: u32 },
    Void,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct IcebergPartitionSpec {
    pub spec_id: i32,
    #[serde(default)]
    pub fields: Vec<IcebergPartitionField>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct IcebergSchema {
    pub schema_id: i32,
    #[serde(default)]
    pub identifier_field_ids: Vec<i32>,
    pub fields: Vec<LogicalField>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct IcebergTablePlan {
    pub identifier: IcebergTableIdentifier,
    pub schema: IcebergSchema,
    pub partition_spec: IcebergPartitionSpec,
    #[serde(default)]
    pub properties: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct IcebergCommitPlan {
    pub table: IcebergTableIdentifier,
    pub operation: IcebergCommitOperation,
    #[serde(default)]
    pub data_files: Vec<String>,
    #[serde(default)]
    pub equality_delete_files: Vec<String>,
    #[serde(default)]
    pub position_delete_files: Vec<String>,
    #[serde(default)]
    pub expected_snapshot_id: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum IcebergCommitOperation {
    Append,
    CdcFinalState,
}
