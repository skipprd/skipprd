use react_core::discover::stats::FieldStats;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

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
    #[serde(default)]
    pub exact_distinct_fields: HashSet<String>,
    pub last_updated_epoch_ms: u64,
}

impl DatasetFieldStats {
    pub fn new(dataset_id: &str) -> Self {
        Self {
            dataset_id: dataset_id.to_string(),
            fields: HashMap::new(),
            exact_distinct_fields: HashSet::new(),
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

pub fn finalize_provider_field_stats(stats: &mut FieldStats) {
    let provider_distinct = stats.approx_distinct;
    stats.finalize();
    if provider_distinct.is_some() {
        stats.approx_distinct = provider_distinct;
    }
}

pub fn parse_provider_u64(raw: &str) -> Option<u64> {
    let value = raw.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(parsed) = value.parse::<u64>() {
        return Some(parsed);
    }
    let unsigned = value.strip_prefix('+').unwrap_or(value);
    let (whole, fractional) = unsigned.split_once('.')?;
    if fractional.chars().all(|c| c == '0') {
        whole.parse::<u64>().ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_finalize_preserves_exact_distinct_count() {
        let mut stats = FieldStats::default();
        stats.total = 4;
        stats.nulls = 0;
        stats.approx_distinct = Some(4);

        finalize_provider_field_stats(&mut stats);

        assert_eq!(stats.approx_distinct, Some(4));
    }

    #[test]
    fn parse_provider_u64_accepts_integral_decimal_strings() {
        assert_eq!(parse_provider_u64("4"), Some(4));
        assert_eq!(parse_provider_u64("4.0"), Some(4));
        assert_eq!(parse_provider_u64("+4.000000"), Some(4));
        assert_eq!(parse_provider_u64("4.5"), None);
        assert_eq!(parse_provider_u64("-4"), None);
    }
}
