use crate::schema::SkipprDataType;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SchemaAlterOp {
    Rename { from: String, to: String },
    Merge { src: String, dst: String },
    Drop { column: String },
    Promote { column: String, to: SkipprDataType },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SchemaAlterError {
    #[error("column '{0}' not found")]
    ColumnNotFound(String),
    #[error("column '{0}' already exists")]
    ColumnExists(String),
    #[error("cannot promote {from} to {to}; Iceberg-legal promotions are byte/short/integer→long, byte/short→integer, float→double, timestamp_milli→timestamp (Iceberg timestamp is already µs); widen to string with MERGE COLUMN")]
    IllegalPromote { from: String, to: String },
    #[error("{0}")]
    Failed(String),
}

pub fn is_iceberg_legal_promote(from: &SkipprDataType, to: &SkipprDataType) -> bool {
    use SkipprDataType::*;
    if from == to {
        return true;
    }
    matches!(
        (from, to),
        (Byte | Short | Integer, Long)
            | (Byte | Short, Integer)
            | (Float, Double)
            | (TimestampMilli, Timestamp)
    )
}
