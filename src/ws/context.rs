use crate::qa::vector::lance_store::LanceDbStore;
use crate::qa::vector::lance_store::ScoredChunk;
use serde_json::Value;

pub struct DatasetResolved {
	pub pipeline: String,
	pub namespace: String,
	pub score: f32,
	pub fields_hint: String,
}

pub struct ResolvedArtifact {
	pub pipeline: String,
	pub namespace: String,
	pub name: String,
	pub kind: String, // metric|model
	pub score: f32,
	pub text: String,
}

pub async fn resolve_datasets(question: &str, top_k: usize) -> Vec<DatasetResolved> {
	// Embed the question once
	let cfg = crate::llm::config_from_env();
	let model = crate::llm::create_llm(&cfg);
	let vec = match model.embed(&[question.to_string()]) {
		Ok(mut v) => v.pop().unwrap_or_default(),
		Err(_) => Vec::new(),
	};
	if vec.is_empty() {
		return Vec::new();
	}
	// Query each pipeline store for dataset hits
	let pipelines = crate::sql::registry::list_pipelines().await;
	let mut all: Vec<(String, String, f32)> = Vec::new();
	for p in pipelines {
		let store = LanceDbStore::new(&p);
		if let Ok(hits) = store.query(&vec, top_k, Some("dataset")).await {
			for ScoredChunk { item, score } in hits {
				let ns = item.namespace;
				all.push((p.clone(), ns, score));
			}
		}
	}
	// Dedupe by (pipeline,namespace), keep lowest score first
	use std::collections::HashMap;
	let mut best: HashMap<(String, String), f32> = HashMap::new();
	for (p, ns, s) in all.into_iter() {
		let k = (p, ns);
		let entry = best.entry((k.0.clone(), k.1.clone())).or_insert(s);
		if s < *entry { *entry = s; }
	}
	let mut pairs: Vec<((String, String), f32)> = best.into_iter().collect();
	pairs.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
	let mut out: Vec<DatasetResolved> = Vec::new();
	for ((p, ns), score) in pairs.into_iter().take(top_k) {
		let hint = load_catalog_hint(&p, &ns).await.unwrap_or_default();
		out.push(DatasetResolved { pipeline: p, namespace: ns, score, fields_hint: hint });
	}
	out
}

pub async fn resolve_artifacts(question: &str, top_k: usize, kind: &str) -> Vec<ResolvedArtifact> {
	let cfg = crate::llm::config_from_env();
	let model = crate::llm::create_llm(&cfg);
	let vec = match model.embed(&[question.to_string()]) {
		Ok(mut v) => v.pop().unwrap_or_default(),
		Err(_) => Vec::new(),
	};
	if vec.is_empty() { return Vec::new(); }
	let pipelines = crate::sql::registry::list_pipelines().await;
	let mut all: Vec<(String, String, String, f32, String)> = Vec::new(); // (pipeline, namespace, name, score, text)
	for p in pipelines {
		let store = LanceDbStore::new(&p);
		if let Ok(hits) = store.query(&vec, top_k * 3, None).await {
			for h in hits {
				let id = h.item.id.clone();
				if !(h.item.kind == "artifact" && id.starts_with(&format!("artifact:{}:", kind))) {
					continue;
				}
				// id pattern: artifact:<kind>:<pipeline>:<namespace>:<name>
				let parts: Vec<&str> = id.split(':').collect();
				if parts.len() >= 5 {
					let ap = parts[2].to_string();
					let ans = parts[3].to_string();
					let name = parts[4].to_string();
					all.push((ap, ans, name, h.score, h.item.text.clone()));
				}
			}
		}
	}
	// Dedup and sort by score asc
	use std::collections::HashSet;
	let mut out: Vec<ResolvedArtifact> = Vec::new();
	let mut seen: HashSet<(String, String, String)> = HashSet::new();
	all.sort_by(|a, b| a.3.partial_cmp(&b.3).unwrap_or(std::cmp::Ordering::Equal));
	for (p, ns, name, score, text) in all {
		if seen.insert((p.clone(), ns.clone(), name.clone())) {
			out.push(ResolvedArtifact { pipeline: p, namespace: ns, name, kind: kind.to_string(), score, text });
			if out.len() >= top_k { break; }
		}
	}
	out
}

async fn load_catalog_hint(pipeline: &str, namespace: &str) -> Result<String, String> {
	if let Some(entry) = crate::sql::registry::find_entry(pipeline, namespace).await {
		if !entry.catalog_key.is_empty() {
			if let Ok(v) = crate::helpers::s3::get_json(&entry.catalog_key).await {
				return Ok(build_fields_hint(&v));
			}
		}
	}
	Ok(String::new())
}

fn build_fields_hint(v: &Value) -> String {
	let mut out: Vec<String> = Vec::new();
	if let Some(fields) = v.get("fields").and_then(|x| x.as_array()) {
		for f in fields.iter().take(8) {
			let name = f.get("name").and_then(|x| x.as_str()).unwrap_or("");
			let role = f.get("role").and_then(|x| x.as_str()).unwrap_or("");
			out.push(format!("{}:{}", name, role));
		}
	}
	out.join(", ")
}


