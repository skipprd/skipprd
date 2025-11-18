use async_trait::async_trait;
use serde_json::Value;
use crate::qa::agent::AgentCtx;
use super::Tool;

pub struct ArtifactsTool;

#[async_trait]
impl Tool for ArtifactsTool {
	fn name(&self) -> &'static str { "artifacts" }
	async fn call(&self, args: Value, _ctx: &AgentCtx) -> Result<Value, String> {
		let op = args.get("op").and_then(|x| x.as_str()).unwrap_or("list");
		match op {
			"list" => list_artifacts(args).await,
			"get" => get_artifact(args).await,
			_ => Err("unsupported op; use 'list' or 'get'".to_string()),
		}
	}
}

async fn list_artifacts(args: Value) -> Result<Value, String> {
	let ty = args.get("type").and_then(|x| x.as_str()); // "model"|"metric"|None
	let ns_filter = args.get("namespace").and_then(|x| x.as_str()).map(|s| s.to_string());
	let limit = args.get("limit").and_then(|x| x.as_u64()).unwrap_or(20) as usize;
	let bucket = crate::helpers::configuration::Config::get_skippr_s3_bucket();
	let client = crate::helpers::s3::get_s3_client().await;
	let mut items: Vec<(i64, Value)> = Vec::new();
	let pipelines = crate::sql::registry::list_pipelines().await;
	for p in pipelines {
		let base = dbt_base_prefix(&p);
		let kinds: &[(&str, &str)] = &[
			("metric", "metrics"),
			("model", "models"),
		];
		for (kind_name, dir_name) in kinds {
			if let Some(t) = ty { if t != *kind_name { continue; } }
			let prefix = format!("{}/{}/", base, dir_name);
			let mut token: Option<String> = None;
			loop {
				let mut req = client.list_objects_v2().bucket(&bucket).prefix(&prefix).max_keys(1000);
				if let Some(t) = token.as_ref() { req = req.continuation_token(t); }
				match req.send().await {
					Ok(resp) => {
						for obj in resp.contents() {
							if let Some(k) = obj.key() {
								// filter out versions directory
								if k.contains("/_versions/") { continue; }
								// namespace filter: expect .../{namespace}/{name}.ext
								let rest = &k[prefix.len()..];
								let parts: Vec<&str> = rest.split('/').collect();
								if parts.len() != 2 { continue; }
								let ns = parts[0].to_string();
								if let Some(filt) = ns_filter.as_ref() {
									if &ns != filt { continue; }
								}
								let name_ext = parts[1];
								let name = name_ext.trim_end_matches(".sql").trim_end_matches(".yaml").to_string();
								let ts = obj.last_modified().map(|t| t.secs()).unwrap_or_default();
								items.push((ts, serde_json::json!({
									"pipeline": p,
									"namespace": ns,
									"kind": *kind_name,
									"name": name,
									"key": k
								})));
							}
						}
						if resp.next_continuation_token().is_none() { break; }
						token = resp.next_continuation_token().map(|s| s.to_string());
					}
					Err(_) => { break; }
				}
			}
		}
	}
	// newest first
	items.sort_by_key(|(ts, _)| *ts);
	items.reverse();
	let listed: Vec<Value> = items.into_iter().take(limit).map(|(_, v)| v).collect();
	Ok(serde_json::json!({"ok": true, "items": listed}))
}

async fn get_artifact(args: Value) -> Result<Value, String> {
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
	let base = dbt_base_prefix(&p);
	let key = match kind {
		"model" => format!("{}/models/{}/{}.sql", base, ns, name),
		_ => format!("{}/metrics/{}/{}.yaml", base, ns, name),
	};
	match crate::helpers::s3::get_bytes(&key).await {
		Ok(bytes) => {
			let text = String::from_utf8_lossy(&bytes).to_string();
			Ok(serde_json::json!({"ok": true, "key": key, "content": text}))
		}
		Err(e) => Err(format!("not found or failed to fetch: {:?}", e)),
	}
}

fn dbt_base_prefix(pipeline: &str) -> String {
	let tenant = crate::helpers::configuration::Config::get_tenant();
	let workspace = crate::helpers::configuration::Config::get_workspace_name();
	format!("{}/{}/{}/dbt", tenant, workspace, pipeline)
}


