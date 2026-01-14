use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum SemanticFieldRole {
	Id,
	Timestamp,
	Categorical,
	Metric,
	FreeText,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SemanticField {
	pub name: String,
	pub role: SemanticFieldRole,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FieldStatsLite {
	// Subset of stats safe and useful for catalog embedding
	pub total: u64,
	pub nulls: u64,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub min_numeric: Option<f64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub max_numeric: Option<f64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub min_len: Option<u64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub max_len: Option<u64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub approx_distinct: Option<u64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub histogram_bins: Option<Vec<u64>>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub histogram_min: Option<f64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub histogram_max: Option<f64>,
	pub last_updated_epoch_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatasetStats {
	// Dataset-level summary counters and time window
	pub approx_total_rows: u64,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub earliest_ts: Option<i64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub latest_ts: Option<i64>,
	// Quick map of null counts by field for convenience
	#[serde(default)]
	pub nulls_by_field: std::collections::HashMap<String, u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SemanticModel {
	/// Canonical dataset identifier: "<catalog>.<database>.<table>".
	pub dataset_id: String,
	#[serde(default)]
	pub catalog: String,
	#[serde(default)]
	pub database: String,
	#[serde(default)]
	pub table: String,
	pub fields: Vec<SemanticField>,
	#[serde(default)]
	pub dimensions: Vec<String>,
	#[serde(default)]
	pub metrics: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CatalogField {
	pub entity: String,
	pub name: String,
	/// Engine-native column type (e.g. Glue/Athena type string).
	#[serde(rename = "type", skip_serializing_if = "Option::is_none")]
	pub data_type: Option<String>,
	pub description: Option<String>,
	pub synonyms: Option<Vec<String>>,
	pub pii_sensitivity: Option<String>,
	pub units_or_format: Option<String>,
	#[serde(default)]
	pub role: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub stats: Option<FieldStatsLite>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DataCatalog {
	/// Canonical dataset identifier: "<catalog>.<database>.<table>".
	pub dataset_id: String,
	#[serde(default)]
	pub catalog: String,
	#[serde(default)]
	pub database: String,
	#[serde(default)]
	pub table: String,
	pub description: Option<String>,
	#[serde(default)]
	pub dimensions: Vec<String>,
	#[serde(default)]
	pub metrics: Vec<String>,
	pub fields: Vec<CatalogField>,
	// Shallow index of parent path -> immediate children for navigation (flat fields remain canonical)
	#[serde(default)]
	pub structure_index: std::collections::HashMap<String, Vec<String>>,
	// Dataset-level stats snapshot
	#[serde(skip_serializing_if = "Option::is_none")]
	pub dataset_stats: Option<DatasetStats>,
}

