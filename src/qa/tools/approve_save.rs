use async_trait::async_trait;
use serde_json::Value;
use crate::qa::agent::AgentCtx;
use super::Tool;
use tracing::info;

pub struct ApproveAndSaveArtifactTool;

#[async_trait]
impl Tool for ApproveAndSaveArtifactTool {
	fn name(&self) -> &'static str { "approve_and_save_artifact" }
	async fn call(&self, args: Value, _ctx: &AgentCtx) -> Result<Value, String> {
		let kind = args.get("kind").and_then(|x| x.as_str()).unwrap_or("");
		if kind != "model" && kind != "metric" {
			return Err("kind must be 'model' or 'metric'".to_string());
		}
		let name = args.get("name").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
		if name.is_empty() {
			return Err("name required".to_string());
		}
		let content = args.get("content").and_then(|x| x.as_str()).unwrap_or("").to_string();
		if content.trim().is_empty() {
			return Err("content required".to_string());
		}
		let pipeline_arg = args.get("pipeline").and_then(|x| x.as_str()).map(|s| s.to_string());
		let namespace_arg = args.get("namespace").and_then(|x| x.as_str()).map(|s| s.to_string());
		let preview_diff = args.get("preview_diff").and_then(|x| x.as_bool()).unwrap_or(false);

		// Infer (pipeline, namespace) from content FQNs if not provided
		let (pipeline, namespace) = match (pipeline_arg, namespace_arg) {
			(Some(p), Some(ns)) => (p, ns),
			(p, n) => {
				let derived = derive_fqn(&content);
				let p2 = p.or_else(|| derived.as_ref().map(|(a, _)| a.clone()))
					.ok_or_else(|| "pipeline could not be inferred; provide explicitly".to_string())?;
				let n2 = n.or_else(|| derived.as_ref().map(|(_, b)| b.clone()))
					.ok_or_else(|| "namespace could not be inferred; provide explicitly".to_string())?;
				(p2, n2)
			}
		};

		let (current_key, version_key, content_type) = match kind {
			"model" => {
				let base = dbt_base_prefix(&pipeline);
				let current = format!("{}/models/{}/{}.sql", base, namespace, name);
				let ver = format!("{}/models/{}/_versions/{}/{}.sql", base, namespace, name, chrono::Utc::now().format("%Y%m%d_%H%M%S"));
				(current, ver, "text/sql")
			}
			_ => {
				let base = dbt_base_prefix(&pipeline);
				let current = format!("{}/metrics/{}/{}.yaml", base, namespace, name);
				let ver = format!("{}/metrics/{}/_versions/{}/{}.yaml", base, namespace, name, chrono::Utc::now().format("%Y%m%d_%H%M%S"));
				(current, ver, "text/yaml")
			}
		};

		// Fetch existing (if any)
		let existing: Option<String> = match crate::helpers::s3::get_bytes(&current_key).await {
			Ok(bytes) => Some(String::from_utf8_lossy(&bytes).to_string()),
			Err(_) => None,
		};

		if preview_diff {
			let diff = compute_unified_diff(existing.as_deref().unwrap_or(""), &content);
			// Log focus step for auditing which artifact is being considered
			if let Some(tid) = _ctx.thread_id.as_ref() {
				let store = crate::qa::session::ThreadStore::new();
				let _ = store.append_step(tid, crate::qa::session::ThreadStep {
					action: "artifact_focus".to_string(),
					args: serde_json::json!({ "kind": kind, "name": name, "pipeline": pipeline, "namespace": namespace }),
					observation: serde_json::json!({ "exists": existing.is_some() }),
					ts: chrono::Utc::now().to_rfc3339(),
					agent: _ctx.agent_name.clone(),
				}).await;
			}
			return Ok(serde_json::json!({
				"ok": true,
				"exists": existing.is_some(),
				"key": current_key,
				"diff": diff,
				"pipeline": pipeline,
				"namespace": namespace,
				"kind": kind
			}));
		}

		// Validate
		if kind == "model" {
			validate_model_sql(&content).await?;
		} else {
			validate_metric_yaml(&content).await?;
		}

		// Save current (stable name)
		crate::helpers::s3::put_bytes(&current_key, content.as_bytes(), content_type).await.map_err(|e| format!("{:?}", e))?;
		// Save versioned copy
		let _ = crate::helpers::s3::put_bytes(&version_key, content.as_bytes(), content_type).await;

		info!("Artifact saved: kind={} key={}", kind, current_key);

		// Log step into thread if available
		if let Some(tid) = _ctx.thread_id.as_ref() {
			let store = crate::qa::session::ThreadStore::new();
			let _ = store.append_step(tid, crate::qa::session::ThreadStep {
				action: "artifact_saved".to_string(),
				args: serde_json::json!({ "kind": kind, "name": name, "pipeline": pipeline, "namespace": namespace }),
				observation: serde_json::json!({ "key": current_key }),
				ts: chrono::Utc::now().to_rfc3339(),
				agent: _ctx.agent_name.clone(),
			}).await;
		}

		Ok(serde_json::json!({"ok": true, "key": current_key}))
	}
}

fn dbt_base_prefix(pipeline: &str) -> String {
	let tenant = crate::helpers::configuration::Config::get_tenant();
	let workspace = crate::helpers::configuration::Config::get_workspace_name();
	format!("{}/{}/{}/dbt", tenant, workspace, pipeline)
}

fn derive_fqn(content: &str) -> Option<(String, String)> {
	let lower = content.to_lowercase();
	let pipes = futures::executor::block_on(crate::sql::registry::list_pipelines());
	for p in pipes {
		let nss = futures::executor::block_on(crate::sql::registry::list_namespaces(&p));
		for ns in nss {
			let fqn = format!("{}.{}", p, ns).to_lowercase();
			if lower.contains(&fqn) { return Some((p.clone(), ns.clone())); }
		}
	}
	None
}

fn compute_unified_diff(old: &str, new: &str) -> String {
	// Simple line-wise diff; not minimal but sufficient for preview
	let old_lines: Vec<&str> = old.split('\n').collect();
	let new_lines: Vec<&str> = new.split('\n').collect();
	let mut out: Vec<String> = Vec::new();
	out.push(format!("--- original"));
	out.push(format!("+++ modified"));
	let mut i = 0usize;
	let mut j = 0usize;
	while i < old_lines.len() || j < new_lines.len() {
		if i < old_lines.len() && j < new_lines.len() {
			if old_lines[i] == new_lines[j] {
				out.push(format!(" {}", old_lines[i]));
				i += 1; j += 1;
			} else {
				out.push(format!("- {}", old_lines[i]));
				out.push(format!("+ {}", new_lines[j]));
				i += 1; j += 1;
			}
		} else if i < old_lines.len() {
			out.push(format!("- {}", old_lines[i]));
			i += 1;
		} else {
			out.push(format!("+ {}", new_lines[j]));
			j += 1;
		}
	}
	out.join("\n")
}

async fn validate_model_sql(sql: &str) -> Result<(), String> {
	let s = sql.trim();
	if !s.to_uppercase().starts_with("SELECT ") {
		return Err("model SQL must start with SELECT".to_string());
	}
	let mut forced = s.to_string();
	if !forced.to_lowercase().contains(" limit ") {
		forced.push_str(" LIMIT 10");
	}
	let ctx = crate::sql::query::new_context_all_namespaces().await;
	match ctx.sql(&forced).await {
		Ok(df) => df.collect().await.map(|_| ()).map_err(|e| e.to_string()),
		Err(e) => Err(e.to_string()),
	}
}

async fn validate_metric_yaml(yaml_text: &str) -> Result<(), String> {
	let parsed_yaml = serde_yaml::from_str::<serde_yaml::Value>(yaml_text).map_err(|e| e.to_string())?;
	let mut sqls: Vec<String> = Vec::new();
	fn find_sql_fragments(v: &serde_yaml::Value, out: &mut Vec<String>) {
		match v {
			serde_yaml::Value::Mapping(map) => {
				for (k, val) in map {
					if let serde_yaml::Value::String(key) = k {
						let key_l = key.to_lowercase();
						if key_l == "sql" || key_l.contains("expression") {
							if let serde_yaml::Value::String(s) = val { out.push(s.clone()); }
						}
					}
					find_sql_fragments(val, out);
				}
			}
			serde_yaml::Value::Sequence(arr) => { for el in arr { find_sql_fragments(el, out); } }
			_ => {}
		}
	}
	find_sql_fragments(&parsed_yaml, &mut sqls);
	let ctx = crate::sql::query::new_context_all_namespaces().await;
	for s in sqls.iter() {
		let sl = s.trim().to_lowercase();
		if sl.starts_with("select ") {
			let limited = if sl.contains(" limit ") { s.clone() } else { format!("{} LIMIT 10", s) };
			if let Err(e) = ctx.sql(&limited).await {
				return Err(format!("MetricFlow SQL validation failed: {}", e));
			}
		}
	}
	Ok(())
}


