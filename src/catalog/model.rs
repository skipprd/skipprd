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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SemanticModel {
	pub namespace: String,
	pub fields: Vec<SemanticField>,
	#[serde(default)]
	pub dimensions: Vec<String>,
	#[serde(default)]
	pub metrics: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CatalogField {
	pub entity: String,
	pub name: String,
	pub description: Option<String>,
	pub synonyms: Option<Vec<String>>, 
	pub pii_sensitivity: Option<String>,
	pub units_or_format: Option<String>,
	#[serde(default)]
	pub role: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DataCatalog {
	pub namespace: String,
	pub description: Option<String>,
	#[serde(default)]
	pub dimensions: Vec<String>,
	#[serde(default)]
	pub metrics: Vec<String>,
	pub fields: Vec<CatalogField>,
}
