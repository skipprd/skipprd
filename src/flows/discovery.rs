use datafusion::prelude::SessionContext;
use serde_json::json;
use tracing::info;

#[derive(Clone, Debug)]
pub struct DiscoveryBundle {
	pub datasets: Vec<(String, String, f32)>, // (pipeline, namespace, score)
	pub schemas: Vec<(String, String, Vec<(String, String)>)>, // (pipeline, namespace, [(col, dtype)])
	pub samples: Vec<(String, String, Vec<Vec<String>>)>, // (pipeline, namespace, rows)
	pub hints: Vec<String>,
}

pub struct DiscoveryLimits {
	pub vect_top_k: usize,
	pub max_datasets: usize,
	pub schema_sample_concurrency: usize,
	pub sample_rows: usize,
}

impl Default for DiscoveryLimits {
	fn default() -> Self {
		Self {
			vect_top_k: 200,
			max_datasets: 128,
			schema_sample_concurrency: 16,
			sample_rows: 5,
		}
	}
}

pub async fn run_discovery(thread_id: &str, question: &str, ctx: &SessionContext, limits: &DiscoveryLimits) -> DiscoveryBundle {
	let mut pairs: Vec<(String, String, f32)> = Vec::new();
	let mut hints: Vec<String> = Vec::new();

	// Primary: embedding-based dataset resolution (liberal)
	let candidates = crate::ws::context::resolve_datasets(question, limits.vect_top_k).await;
	for c in candidates.iter() {
		pairs.push((c.pipeline.clone(), c.namespace.clone(), c.score));
	}
	// Hints from vector fields if available (reuse vect_query(scope:\"dataset\") logic already returns field text)
	// For simplicity, add the namespace names as hints
	for c in candidates.iter() { hints.push(format!("{}.{}", c.pipeline, c.namespace)); }

	// Fallback enumeration if embeddings sparse: include all namespaces
	if pairs.is_empty() {
		let pipes = crate::sql::registry::list_pipelines().await;
		for p in pipes {
			let nss = crate::sql::registry::list_namespaces(&p).await;
			for ns in nss {
				pairs.push((p.clone(), ns, 10.0)); // neutral score
			}
		}
	}

	// Dedup and cap
	{
		pairs.sort_by(|a,b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
		pairs.dedup_by(|a,b| a.0==b.0 && a.1==b.1);
	}
	if pairs.len() > limits.max_datasets {
		pairs.truncate(limits.max_datasets);
	}

	// Register all discovered in the thread-scoped context
	let reg_pairs: Vec<(String, String)> = pairs.iter().map(|(p,ns,_)| (p.clone(), ns.clone())).collect();
	crate::ws::agent_runner::pre_register_selected_namespaces(ctx, &reg_pairs).await;

	// Parallel schema/sample
	let mut schemas: Vec<(String, String, Vec<(String, String)>)> = Vec::new();
	let mut samples: Vec<(String, String, Vec<Vec<String>>)> = Vec::new();

	let tasks: Vec<(String, String)> = reg_pairs.clone();
	let results = crate::flows::util::buffer_unordered_map(tasks, limits.schema_sample_concurrency, |(p,ns)| {
		let ctx2 = ctx.clone();
		async move {
			let tbl = format!("{}.{}", p, ns);
			let mut cols: Vec<(String, String)> = Vec::new();
			if let Ok(df) = ctx2.table(&tbl).await {
				for f in df.schema().fields() {
					cols.push((f.name().to_string(), format!("{:?}", f.data_type())));
				}
			}
			let mut rows: Vec<Vec<String>> = Vec::new();
			let sel = format!("SELECT * FROM {} LIMIT {}", tbl, limits.sample_rows);
			if let Ok(df2) = ctx2.sql(&sel).await {
				if let Ok(batches) = df2.collect().await {
					for b in batches {
						for r in 0..b.num_rows() {
							let mut row: Vec<String> = Vec::new();
							for c in 0..b.num_columns() {
								row.push(crate::sql::tui::value_to_string(b.column(c).as_ref(), r));
							}
							rows.push(row);
						}
					}
				}
			}
			(p, ns, cols, rows)
		}
	}).await;
	for (p, ns, cols, rows) in results {
		if !cols.is_empty() { schemas.push((p.clone(), ns.clone(), cols)); }
		if !rows.is_empty() { samples.push((p.clone(), ns.clone(), rows)); }
	}

	// Record bundle in thread log
	let store = crate::qa::session::ThreadStore::new();
	let ds_arr: Vec<serde_json::Value> = pairs.iter().map(|(p,ns,sc)| json!({"pipeline":p,"namespace":ns,"score":sc})).collect();
	let sch_map: serde_json::Value = serde_json::Value::Object(
		schemas.iter().fold(serde_json::Map::new(), |mut m, (p,ns,cols)| {
			let key = format!("{}.{}", p, ns);
			let val = serde_json::Value::Array(cols.iter().map(|(n,t)| json!({"name":n,"type":t})).collect());
			m.insert(key, val); m
		})
	);
	let samp_map: serde_json::Value = serde_json::Value::Object(
		samples.iter().fold(serde_json::Map::new(), |mut m, (p,ns,rows)| {
			let key = format!("{}.{}", p, ns);
			let val = serde_json::Value::Array(rows.iter().map(|r| serde_json::Value::Array(r.iter().map(|c| serde_json::Value::String(c.clone())).collect())).collect());
			m.insert(key, val); m
		})
	);
	let _ = store.append_step(thread_id, crate::qa::session::ThreadStep {
		action: "discovery_bundle".to_string(),
		args: json!({"datasets": ds_arr, "schemas": sch_map, "samples": samp_map, "hints": hints}),
		observation: json!({"ok": true}),
		ts: chrono::Utc::now().to_rfc3339(),
		agent: None,
	}).await;
	info!("Discovery bundle created: datasets={}, schemas={}, samples={}", ds_arr.len(), schemas.len(), samples.len());

	DiscoveryBundle { datasets: pairs, schemas, samples, hints }
}


