use serde_json::Value;

/// Original, unmodified JSON record as received from the source.
/// Used for schema discovery, evolution, and retry re-processing.
#[derive(Debug, Clone)]
pub struct SourceRecord(Value);

impl SourceRecord {
    pub fn new(value: Value) -> Self {
        Self(value)
    }
    pub fn inner(&self) -> &Value {
        &self.0
    }
    pub fn into_inner(self) -> Value {
        self.0
    }
}

/// Post-normalization record (type-coerced, potentially flattened).
/// Used for Arrow serialization and output.
#[derive(Debug, Clone)]
pub struct NormalizedRecord(Value);

impl NormalizedRecord {
    pub fn new(value: Value) -> Self {
        Self(value)
    }
    pub fn inner(&self) -> &Value {
        &self.0
    }
    pub fn into_inner(self) -> Value {
        self.0
    }
}
