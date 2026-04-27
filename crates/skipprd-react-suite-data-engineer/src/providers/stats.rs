use react_core::discover::stats::FieldStats;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

fn current_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DatasetFieldStats {
    pub dataset_id: String,
    pub fields: HashMap<String, FieldStats>,
    pub last_updated_epoch_ms: u64,
}

impl DatasetFieldStats {
    pub fn new(dataset_id: &str) -> Self {
        Self {
            dataset_id: dataset_id.to_string(),
            fields: HashMap::new(),
            last_updated_epoch_ms: current_millis(),
        }
    }

    pub fn update_field(&mut self, field: &str, value: &serde_json::Value) {
        let entry = self
            .fields
            .entry(field.to_string())
            .or_insert_with(FieldStats::default);
        entry.update_value(value);
        self.last_updated_epoch_ms = current_millis();
    }
}
