use async_trait::async_trait;
use serde_json::Value;
use crate::agent::AgentCtx;
use crate::tools::Tool;

pub struct SearchDbtExamplesTool;

#[async_trait]
impl Tool for SearchDbtExamplesTool {
	fn name(&self) -> &'static str { "search_dbt_examples" }
	async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
		let query = args.get("query").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
		if query.is_empty() { return Ok(serde_json::json!({"ok": true, "examples": []})); }
		let k = args.get("k").and_then(|x| x.as_u64()).unwrap_or(8) as usize;
		// Ensure examples synced at least once (non-blocking if already done)
		crate::dbt::examples::ensure_synced_once(
			ctx.storage.clone(),
			ctx.keyspace.clone(),
			ctx.scope.clone(),
			ctx.llm.clone(),
		).await;
		let embed_chars = query.len();
		let results = crate::dbt::examples::search_examples(
			ctx.keyspace.clone(),
			ctx.scope.clone(),
			ctx.llm.clone(),
			&query,
			k,
		).await?;
		// Map to compact response
		let mut examples: Vec<Value> = Vec::new();
		for sc in results.into_iter() {
			let project = sc.item.dataset_id.clone();
			let path = sc.item.field.clone().unwrap_or_default();
			// Filter to DBT-relevant paths only; skip CI/workflow or hidden files
			let p = path.replace('\\', "/");
			let is_allowed = p.starts_with("models/")
				|| p.starts_with("metrics/")
				|| p.starts_with("macros/")
				|| p.starts_with("snapshots/")
				|| p.starts_with("seeds/")
				|| p.starts_with("analyses/")
				|| p.starts_with("tests/")
				|| p.starts_with("exposures/")
				|| p.starts_with("docs/")
				|| p == "dbt_project.yml"
				|| p == "packages.yml";
			if !is_allowed { continue; }
			let s3_uri = sc.item.meta.get("s3_uri").and_then(|v| v.as_str()).unwrap_or("").to_string();
			let preview = if sc.item.text.len() > 280 { format!("{}...", &sc.item.text[..280]) } else { sc.item.text.clone() };
			examples.push(serde_json::json!({
				"project": project,
				"path": p,
				"s3_uri": s3_uri,
				"preview": preview,
				"score": sc.score,
			}));
		}
		let est_tokens = ((embed_chars as f32)/4.0).round() as i64;
		Ok(serde_json::json!({"ok": true, "examples": examples, "llm_expense": {"embed_chars": embed_chars, "est_tokens": est_tokens}}))
	}
}


