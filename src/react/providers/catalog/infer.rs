use super::stats_from_catalog::namespace_stats_from_catalog_json;
use super::types::{SemanticField, SemanticFieldRole, SemanticModel};
use crate::discover::stats::NamespaceStats;
use std::sync::Arc;

fn classify_field(_name: &str, stats: Option<&crate::discover::stats::FieldStats>) -> SemanticFieldRole {
	// Name-agnostic: rely only on stats
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

pub async fn infer_semantic_model_async(
	storage: Arc<dyn crate::adapters::storage::StorageAdapter>,
	keyspace: Arc<dyn crate::react::providers::Keyspace>,
	scope: &crate::react::providers::RequestScope,
	namespace: &str,
) -> SemanticModel {
	// Prefer stats embedded in Catalog; fallback to separate stats object if present
	let ns_stats: Option<NamespaceStats> = {
		let key = keyspace.catalog_key(scope, namespace);
		match storage.get_json(&key).await {
			Ok(val) => namespace_stats_from_catalog_json(namespace, &val),
			Err(_) => None,
		}
	};
	let mut fields: Vec<SemanticField> = Vec::new();
	if let Some(ns) = ns_stats {
		for (fname, fstats) in ns.fields.iter() {
			let role = classify_field(fname, Some(fstats));
			fields.push(SemanticField { name: fname.clone(), role });
		}
	}
	let mut model = SemanticModel {
		dataset_id: namespace.to_string(),
		catalog: String::new(),
		database: String::new(),
		table: String::new(),
		fields,
		dimensions: Vec::new(),
		metrics: Vec::new(),
	};
	for f in &model.fields {
		match f.role {
			SemanticFieldRole::Id | SemanticFieldRole::Timestamp | SemanticFieldRole::Categorical => model.dimensions.push(f.name.clone()),
			SemanticFieldRole::Metric => model.metrics.push(f.name.clone()),
			SemanticFieldRole::FreeText => {}
		}
	}
	model
}

// NOTE: legacy synchronous wrapper removed. Use `infer_semantic_model_async(storage, keyspace, scope, namespace).await`.

