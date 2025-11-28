#[derive(Clone, Default)]
pub struct PreflightConfig {
	pub resolve_artifacts: bool,
	pub gate_on_selection: bool,
	pub selection_types: Vec<&'static str>,
}

#[derive(Clone, Default)]
pub struct PreflightOutcome {
	pub intent: Option<serde_json::Value>,
	pub datasets: Vec<serde_json::Value>,
	pub artifacts: Vec<serde_json::Value>,
	pub decision: Option<crate::ws::context::PreflightDecision>,
}

pub async fn run_preflight(thread_id: &str, question: &str, agent: &str, cfg: &PreflightConfig) -> PreflightOutcome {
	// Prefer the first user message in the thread when deriving the query to embed,
	// so short confirmations like "Model" don't derail dataset/artifact resolution.
	let q_for_embed = {
		let storeq = crate::qa::session::ThreadStore::new();
		if let Some(log) = storeq.get(thread_id).await {
			if let Some(first_user) = log.steps.iter().find(|s| s.action == "user") {
				if let Some(t) = first_user.args.get("text").and_then(|x| x.as_str()) {
					if !t.trim().is_empty() { t.to_string() } else { question.to_string() }
				} else { question.to_string() }
			} else { question.to_string() }
		} else { question.to_string() }
	};
	// Resolve datasets
	let candidates = crate::ws::context::resolve_datasets(&q_for_embed, 50).await;
	if !candidates.is_empty() {
		let store = crate::qa::session::ThreadStore::new();
		let arr: Vec<serde_json::Value> = candidates.iter().map(|c| serde_json::json!({"pipeline": c.pipeline, "namespace": c.namespace, "score": c.score})).collect();
		let _ = store.append_step(thread_id, crate::qa::session::ThreadStep {
			action: "resolved_datasets".to_string(),
			args: serde_json::json!({"candidates": arr}),
			observation: serde_json::json!({"ok": true}),
			ts: chrono::Utc::now().to_rfc3339(),
			agent: Some(agent.to_string()),
		}).await;
	}
	// Resolve artifacts (optional)
	if cfg.resolve_artifacts {
		let arts = crate::ws::context::resolve_artifacts(&q_for_embed, 3, "metric").await;
		if !arts.is_empty() {
			let store = crate::qa::session::ThreadStore::new();
			let arr: Vec<serde_json::Value> = arts.iter().map(|a| serde_json::json!({
				"pipeline": a.pipeline, "namespace": a.namespace, "name": a.name, "kind": a.kind, "score": a.score
			})).collect();
			let _ = store.append_step(thread_id, crate::qa::session::ThreadStep {
				action: "resolved_artifacts".to_string(),
				args: serde_json::json!({"items": arr}),
				observation: serde_json::json!({"ok": true}),
				ts: chrono::Utc::now().to_rfc3339(),
				agent: Some(agent.to_string()),
			}).await;
		}
	}
	// Think-out-loud and decision
	let intent = crate::ws::context::preflight_intent_llm(&q_for_embed).await;
	{
		let store = crate::qa::session::ThreadStore::new();
		let _ = store.append_step(thread_id, crate::qa::session::ThreadStep {
			action: "preflight_intent".to_string(),
			args: serde_json::to_value(&intent).unwrap_or(serde_json::json!({})),
			observation: serde_json::json!({"ok": true}),
			ts: chrono::Utc::now().to_rfc3339(),
			agent: Some(agent.to_string()),
		}).await;
	}
	let arts_vec = {
		let store = crate::qa::session::ThreadStore::new();
		let mut out: Vec<crate::ws::context::ResolvedArtifact> = Vec::new();
		if let Some(log) = store.get(thread_id).await {
			for step in log.steps.iter().rev() {
				if step.action == "resolved_artifacts" {
					if let Some(arr) = step.args.get("items").and_then(|x| x.as_array()) {
						for v in arr {
							let p = v.get("pipeline").and_then(|x| x.as_str()).unwrap_or("").to_string();
							let ns = v.get("namespace").and_then(|x| x.as_str()).unwrap_or("").to_string();
							let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
							let score = v.get("score").and_then(|x| x.as_f64()).unwrap_or(0.0) as f32;
							out.push(crate::ws::context::ResolvedArtifact { pipeline: p, namespace: ns, name, kind: "metric".to_string(), score, text: String::new() });
						}
					}
					break;
				}
			}
		}
		out
	};
	let decision = crate::ws::context::preflight_decision_llm(&intent, &candidates, &arts_vec).await;
	{
		let store = crate::qa::session::ThreadStore::new();
		let _ = store.append_step(thread_id, crate::qa::session::ThreadStep {
			action: "preflight_decision".to_string(),
			args: serde_json::to_value(&decision).unwrap_or(serde_json::json!({})),
			observation: serde_json::json!({"ok": true}),
			ts: chrono::Utc::now().to_rfc3339(),
			agent: Some(agent.to_string()),
		}).await;
	}
	PreflightOutcome {
		intent: Some(serde_json::to_value(intent).unwrap_or(serde_json::json!({}))),
		datasets: candidates.iter().map(|c| serde_json::json!({"pipeline": c.pipeline, "namespace": c.namespace, "score": c.score})).collect(),
		artifacts: arts_vec.iter().map(|a| serde_json::json!({"pipeline": a.pipeline, "namespace": a.namespace, "name": a.name, "kind": a.kind, "score": a.score})).collect(),
		decision: Some(decision),
	}
}

pub async fn run_preflight_on_bundle(thread_id: &str, agent: &str) -> PreflightOutcome {
	// Read back the latest discovery bundle from the thread
	let store = crate::qa::session::ThreadStore::new();
	let mut datasets: Vec<crate::ws::context::DatasetResolved> = Vec::new();
	if let Some(log) = store.get(thread_id).await {
		for step in log.steps.iter().rev() {
			if step.action == "discovery_bundle" {
				if let Some(arr) = step.args.get("datasets").and_then(|x| x.as_array()) {
					for v in arr {
						let p = v.get("pipeline").and_then(|x| x.as_str()).unwrap_or("").to_string();
						let ns = v.get("namespace").and_then(|x| x.as_str()).unwrap_or("").to_string();
						let score = v.get("score").and_then(|x| x.as_f64()).unwrap_or(0.0) as f32;
						if !p.is_empty() && !ns.is_empty() {
							// Load catalog-derived field hints (names/roles) to enrich dataset context
							let hint = crate::ws::context::load_catalog_hint(&p, &ns).await.unwrap_or_default();
							datasets.push(crate::ws::context::DatasetResolved { pipeline: p, namespace: ns, score, fields_hint: hint });
						}
					}
				}
				break;
			}
		}
	}
	// Intent then decision using the original first user question
	let q_for_embed = {
		if let Some(log) = store.get(thread_id).await {
			if let Some(first_user) = log.steps.iter().find(|s| s.action == "user") {
				if let Some(t) = first_user.args.get("text").and_then(|x| x.as_str()) {
					if !t.trim().is_empty() { t.to_string() } else { String::new() }
				} else { String::new() }
			} else { String::new() }
		} else { String::new() }
	};
	let intent = crate::ws::context::preflight_intent_llm(&q_for_embed).await;
	{
		let _ = store.append_step(thread_id, crate::qa::session::ThreadStep {
			action: "preflight_intent".to_string(),
			args: serde_json::to_value(&intent).unwrap_or(serde_json::json!({})),
			observation: serde_json::json!({"ok": true}),
			ts: chrono::Utc::now().to_rfc3339(),
			agent: Some(agent.to_string()),
		}).await;
	}
	let arts_vec: Vec<crate::ws::context::ResolvedArtifact> = Vec::new();
	let decision = crate::ws::context::preflight_decision_llm(&intent, &datasets, &arts_vec).await;
	{
		let _ = store.append_step(thread_id, crate::qa::session::ThreadStep {
			action: "preflight_decision".to_string(),
			args: serde_json::to_value(&decision).unwrap_or(serde_json::json!({})),
			observation: serde_json::json!({"ok": true}),
			ts: chrono::Utc::now().to_rfc3339(),
			agent: Some(agent.to_string()),
		}).await;
	}
	PreflightOutcome {
		intent: Some(serde_json::to_value(intent).unwrap_or(serde_json::json!({}))),
		datasets: datasets.iter().map(|c| serde_json::json!({"pipeline": c.pipeline, "namespace": c.namespace, "score": c.score})).collect(),
		artifacts: Vec::new(),
		decision: Some(decision),
	}
}


