use crate::catalog::model::{SemanticField, SemanticFieldRole, SemanticModel};
use crate::helpers::configuration::Config;
use crate::discover::stats::NamespaceStats;

fn classify_field(name: &str, stats: Option<&crate::discover::stats::FieldStats>) -> SemanticFieldRole {
	let n = name.to_lowercase();
	if n.ends_with("_id") || n == "id" { return SemanticFieldRole::Id; }
	if n.contains("timestamp") || n.contains("event_time") || n.contains("created_at") { return SemanticFieldRole::Timestamp; }
	if let Some(s) = stats {
		if s.min_numeric.is_some() || s.max_numeric.is_some() {
			if let Some(d) = s.approx_distinct { if d <= 32 { return SemanticFieldRole::Categorical; } }
			return SemanticFieldRole::Metric;
		}
		if s.max_len.unwrap_or(0) > 64 { return SemanticFieldRole::FreeText; }
		if let Some(d) = s.approx_distinct { if d <= 64 { return SemanticFieldRole::Categorical; } }
		return SemanticFieldRole::FreeText;
	}
	if n.contains("name") || n.contains("desc") || n.contains("text") { return SemanticFieldRole::FreeText; }
	SemanticFieldRole::Categorical
}

pub async fn infer_semantic_model_async(namespace: &str) -> SemanticModel {
	// Read stats from S3 (authoritative)
	let ns_stats: Option<NamespaceStats> = match Config::read_namespace_stats_async(namespace).await {
		Some(v) => serde_json::from_value::<NamespaceStats>(v).ok(),
		None => None,
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
