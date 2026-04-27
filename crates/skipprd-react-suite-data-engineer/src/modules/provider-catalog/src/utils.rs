use react_core::discover::stats::FieldStats;
use serde::Serialize;

use crate::types::{FieldStatsLite, SemanticFieldRole};

/// Serialize a value through YAML round-trip to produce a JSON `Value` suitable
/// for storage. This preserves the YAML-native formatting while producing JSON.
pub fn yaml_to_json_value<T: Serialize>(value: &T) -> Result<serde_json::Value, String> {
    let yaml = serde_yaml::to_string(value).map_err(|e| e.to_string())?;
    let yaml_value =
        serde_yaml::from_str::<serde_yaml::Value>(&yaml).unwrap_or(serde_yaml::Value::Null);
    serde_json::to_value(yaml_value).map_err(|e| e.to_string())
}

pub fn classify_field(_name: &str, stats: Option<&FieldStats>) -> SemanticFieldRole {
    if let Some(s) = stats {
        if s.min_numeric.is_some() || s.max_numeric.is_some() {
            return SemanticFieldRole::Metric;
        }
        if s.max_len.unwrap_or(0) > 64 {
            return SemanticFieldRole::FreeText;
        }
        return SemanticFieldRole::Categorical;
    }
    SemanticFieldRole::Categorical
}

pub fn to_stats_lite(s: &FieldStats) -> FieldStatsLite {
    FieldStatsLite {
        total: s.total,
        nulls: s.nulls,
        min_numeric: s.min_numeric,
        max_numeric: s.max_numeric,
        min_len: s.min_len,
        max_len: s.max_len,
        approx_distinct: s.approx_distinct,
        histogram_bins: s.histogram_bins.clone(),
        histogram_min: s.histogram_min,
        histogram_max: s.histogram_max,
        last_updated_epoch_ms: s.last_updated_epoch_ms,
    }
}
