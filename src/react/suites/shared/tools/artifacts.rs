use async_trait::async_trait;
use serde_json::Value;
use crate::react::agent::AgentCtx;
use crate::react::tools::Tool;

pub struct ArtifactsTool;

#[async_trait]
impl Tool for ArtifactsTool {
	fn name(&self) -> &'static str { "artifacts" }
	async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
		let op = args.get("op").and_then(|x| x.as_str()).unwrap_or("list");
		match op {
			"list" => list_artifacts(args, ctx).await,
			"get" => get_artifact(args, ctx).await,
			_ => Err("unsupported op; use 'list' or 'get'".to_string()),
		}
	}
}

async fn list_artifacts(args: Value, ctx: &AgentCtx) -> Result<Value, String> {
	let ty = args.get("type").and_then(|x| x.as_str()); // "model"|"metric"|None
	let ns_filter = args.get("namespace").and_then(|x| x.as_str()).map(|s| s.to_string());
	let limit = args.get("limit").and_then(|x| x.as_u64()).unwrap_or(20) as usize;
	let mut items: Vec<Value> = Vec::new();
	// ReAct is engine-agnostic: enumerate artifacts within the current project_id only.
	let pipelines = vec![ctx.scope.project_id.clone()];
	for p in pipelines {
		let base = ctx.keyspace.dbt_prefix(&ctx.scope, &p).trim_end_matches('/').to_string();
		let kinds: &[(&str, &str)] = &[
			("metric", "metrics"),
			("model", "models"),
		];
		for (kind_name, dir_name) in kinds {
			if let Some(t) = ty { if t != *kind_name { continue; } }
			let prefix = format!("{}/{}/", base, dir_name);
			let keys = ctx.storage.list_prefix(&prefix).await.unwrap_or_default();
			for k in keys {
				if k.contains("/_versions/") { continue; }
				let rest = k.strip_prefix(&prefix).unwrap_or(&k);
				let parts: Vec<&str> = rest.split('/').collect();
				if parts.len() != 2 { continue; }
				let ns = parts[0].to_string();
				if let Some(filt) = ns_filter.as_ref() {
					if &ns != filt { continue; }
				}
				let name_ext = parts[1];
				let name = name_ext.trim_end_matches(".sql").trim_end_matches(".yaml").to_string();
				items.push(serde_json::json!({
					"pipeline": p,
					"namespace": ns,
					"kind": *kind_name,
					"name": name,
					"key": k
				}));
			}
		}
	}
	items.sort_by(|a, b| a.get("key").and_then(|x| x.as_str()).cmp(&b.get("key").and_then(|x| x.as_str())));
	let listed: Vec<Value> = items.into_iter().take(limit).collect();
	Ok(serde_json::json!({"ok": true, "items": listed}))
}

async fn get_artifact(args: Value, ctx: &AgentCtx) -> Result<Value, String> {
	let kind = args.get("type").and_then(|x| x.as_str()).unwrap_or("");
	if kind != "model" && kind != "metric" {
		return Err("type must be 'model' or 'metric'".to_string());
	}
	let pipeline = args.get("pipeline").and_then(|x| x.as_str()).map(|s| s.to_string());
	let namespace = args.get("namespace").and_then(|x| x.as_str()).map(|s| s.to_string());
	let name = args.get("name").and_then(|x| x.as_str()).map(|s| s.to_string()).ok_or_else(|| "name required".to_string())?;
	let (p, ns) = match (pipeline, namespace) {
		(Some(p), Some(ns)) => (p, ns),
		_ => return Err("pipeline and namespace required".to_string()),
	};
	let base = ctx.keyspace.dbt_prefix(&ctx.scope, &p).trim_end_matches('/').to_string();
	let key = match kind {
		"model" => format!("{}/models/{}/{}.sql", base, ns, name),
		_ => format!("{}/metrics/{}/{}.yaml", base, ns, name),
	};
	match ctx.storage.get_bytes(&key).await {
		Ok(bytes) => {
			let text = String::from_utf8_lossy(&bytes).to_string();
			Ok(serde_json::json!({"ok": true, "key": key, "content": text}))
		}
		Err(e) => Err(format!("not found or failed to fetch: {}", e)),
	}
}


