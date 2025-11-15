use crate::catalog::model::{SemanticField, SemanticFieldRole, SemanticModel};
use crate::helpers::configuration::Config;
use crate::discover::stats::NamespaceStats;
use crate::catalog::stats_from_catalog::namespace_stats_from_catalog_json;

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

pub async fn infer_semantic_model_async(namespace: &str) -> SemanticModel {
	// Prefer stats embedded in Catalog; fallback to separate stats object if present
	let ns_stats: Option<NamespaceStats> = {
		let pipeline = Config::get_pipeline_name();
		if let Some(entry) = crate::sql::registry::find_entry(&pipeline, namespace).await {
			if !entry.catalog_key.is_empty() {
				if let Ok(val) = crate::helpers::s3::get_json(&entry.catalog_key).await {
					namespace_stats_from_catalog_json(namespace, &val)
				} else { None }
			} else { None }
		} else { None }
	};
	let mut fields: Vec<SemanticField> = Vec::new();
	if let Some(ns) = ns_stats {
		for (fname, fstats) in ns.fields.iter() {
			let role = classify_field(fname, Some(fstats));
			fields.push(SemanticField { name: fname.clone(), role });
		}
	}
	let mut model = SemanticModel { namespace: namespace.to_string(), fields, dimensions: Vec::new(), metrics: Vec::new() };
	for f in &model.fields {
		match f.role {
			SemanticFieldRole::Id | SemanticFieldRole::Timestamp | SemanticFieldRole::Categorical => model.dimensions.push(f.name.clone()),
			SemanticFieldRole::Metric => model.metrics.push(f.name.clone()),
			SemanticFieldRole::FreeText => {}
		}
	}
	model
}

#[allow(dead_code)]
pub fn infer_semantic_model(namespace: &str) -> SemanticModel {
	// Synchronous wrapper for legacy call sites
	if tokio::runtime::Handle::try_current().is_ok() {
		// Already in a runtime: spawn a oneshot and block on it without creating a new runtime
		let (tx, rx) = std::sync::mpsc::channel();
		let ns_owned = namespace.to_string();
		tokio::spawn(async move {
			let res = infer_semantic_model_async(&ns_owned).await;
			let _ = tx.send(res);
		});
		return rx.recv().unwrap_or(SemanticModel { namespace: namespace.to_string(), fields: Vec::new(), dimensions: Vec::new(), metrics: Vec::new() });
	}
	// No runtime active: create a lightweight one
	match tokio::runtime::Builder::new_current_thread().enable_all().build() {
		Ok(rt) => rt.block_on(infer_semantic_model_async(namespace)),
		Err(_) => SemanticModel { namespace: namespace.to_string(), fields: Vec::new(), dimensions: Vec::new(), metrics: Vec::new() },
	}
}
