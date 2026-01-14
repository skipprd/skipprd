use async_trait::async_trait;
use serde_json::Value;
use crate::agent::AgentCtx;
use crate::tools::Tool;
use tracing::info;

pub struct ApproveAndSaveArtifactBatchTool;

fn compute_unified_diff(old: &str, new: &str) -> String {
	// Simple line-wise diff identical to approve_save
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

#[async_trait]
impl Tool for ApproveAndSaveArtifactBatchTool {
	fn name(&self) -> &'static str { "approve_and_save_artifact_batch" }
    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
		let items = args.get("items").and_then(|x| x.as_array()).cloned().unwrap_or_default();
		if items.is_empty() { return Ok(serde_json::json!({"ok": true, "keys": []})); }
		let preview = args.get("preview_diff").and_then(|x| x.as_bool()).unwrap_or(false);
		let mut out_diffs: Vec<Value> = Vec::new();
		let mut out_keys: Vec<String> = Vec::new();
		let mut out_files: Vec<Value> = Vec::new();
		for it in items {
			let kind = it.get("kind").and_then(|x| x.as_str()).unwrap_or("");
			if kind != "model" && kind != "metric" && kind != "file" {
				return Err("kind must be 'model' or 'metric' or 'file'".to_string());
			}
			let name = it.get("name").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
			let content = it.get("content").and_then(|x| x.as_str()).unwrap_or("").to_string();
			let pipeline = it.get("pipeline").and_then(|x| x.as_str()).unwrap_or("").to_string();
			let namespace = it.get("namespace").and_then(|x| x.as_str()).unwrap_or("").to_string();
			let explicit_path = it.get("path").and_then(|x| x.as_str()).map(|s| s.to_string());
			if content.trim().is_empty() || pipeline.is_empty() {
				return Err("each item requires {content,pipeline} and either {kind,name,namespace} or a {path}".to_string());
			}
			let base = ctx.keyspace.dbt_prefix(&ctx.scope).trim_end_matches('/').to_string();
			// Derive path and type
			let (current_key, version_key, content_type) = if let Some(path) = explicit_path {
				// Save arbitrary file under provided relative path
				let rel = path.trim_start_matches('/').to_string();
				let current = format!("{}/{}", base, rel);
				let ver = String::new();
				let ct = if rel.ends_with(".sql") { "text/sql" }
					else if rel.ends_with(".yaml") || rel.ends_with(".yml") { "text/yaml" }
					else if rel.ends_with(".md") { "text/markdown" }
					else { "text/plain" };
				(current, ver, ct)
			} else if kind == "model" {
				// Special-case: dbt schema.yml
				if namespace == "models" && name == "schema" && content.trim_start().to_lowercase().starts_with("version:") {
					let current = format!("{}/models/schema.yml", base);
					(current, String::new(), "text/yaml")
				} else {
					let current = format!("{}/models/{}/{}.sql", base, namespace, name);
					let ver = format!("{}/models/{}/_versions/{}/{}.sql", base, namespace, name, chrono::Utc::now().format("%Y%m%d_%H%M%S"));
					(current, ver, "text/sql")
				}
			} else if kind == "metric" {
				let current = format!("{}/metrics/{}/{}.yaml", base, namespace, name);
				let ver = format!("{}/metrics/{}/_versions/{}/{}.yaml", base, namespace, name, chrono::Utc::now().format("%Y%m%d_%H%M%S"));
				(current, ver, "text/yaml")
			} else {
				// kind == "file" but no explicit path — invalid
				return Err("file kind requires a 'path'".to_string());
			};
			let existing: Option<String> = match ctx.storage.get_bytes(&current_key).await {
				Ok(bytes) => Some(String::from_utf8_lossy(&bytes).to_string()),
				Err(_) => None,
			};
			if preview {
				let diff = compute_unified_diff(existing.as_deref().unwrap_or(""), &content);
				let (lines_added, lines_removed) = {
					let old = existing.as_deref().unwrap_or("");
					let old_lines: Vec<&str> = old.split('\n').collect();
					let new_lines: Vec<&str> = content.split('\n').collect();
					let mut add = 0usize; let mut rem = 0usize;
					let mut i = 0usize;
					let mut j = 0usize;
					while i < old_lines.len() || j < new_lines.len() {
						if i < old_lines.len() && j < new_lines.len() {
							if old_lines[i] == new_lines[j] {
								i += 1; j += 1;
							} else {
								rem += 1; add += 1;
								i += 1; j += 1;
							}
						} else if i < old_lines.len() {
							rem += 1; i += 1;
						} else {
							add += 1; j += 1;
						}
					}
					(add, rem)
				};
				out_diffs.push(serde_json::json!({"name": name, "pipeline": pipeline, "namespace": namespace, "kind": kind, "key": current_key, "diff": diff, "exists": existing.is_some(), "lines_added": lines_added, "lines_removed": lines_removed}));
				continue;
			}
			// Ensure minimal project file
			let dbt = ctx
				.dbt
				.as_ref()
				.ok_or_else(|| "dbt provider missing".to_string())?;
			let _ = dbt.ensure_minimal_project(&ctx.scope).await;
			ctx.storage.put_bytes(&current_key, content.as_bytes(), content_type).await?;
			if !version_key.is_empty() {
				let _ = ctx.storage.put_bytes(&version_key, content.as_bytes(), content_type).await;
			}
			info!("Artifact saved (batch): kind={} key={}", kind, current_key);
			out_keys.push(current_key.clone());
			// stats
			let (lines_added, lines_removed) = {
				let old = existing.as_deref().unwrap_or("");
				let old_lines: Vec<&str> = old.split('\n').collect();
				let new_lines: Vec<&str> = content.split('\n').collect();
				let mut add = 0usize; let mut rem = 0usize;
				let mut i = 0usize;
				let mut j = 0usize;
				while i < old_lines.len() || j < new_lines.len() {
					if i < old_lines.len() && j < new_lines.len() {
						if old_lines[i] == new_lines[j] {
							i += 1; j += 1;
						} else {
							rem += 1; add += 1;
							i += 1; j += 1;
						}
					} else if i < old_lines.len() {
						rem += 1; i += 1;
					} else {
						add += 1; j += 1;
					}
				}
				(add, rem)
			};
			let status = if existing.is_some() { "modified" } else { "added" };
			out_files.push(serde_json::json!({"key": current_key, "status": status, "lines_added": lines_added, "lines_removed": lines_removed}));
		}
		if preview {
			return Ok(serde_json::json!({"ok": true, "diffs": out_diffs}));
		}
		Ok(serde_json::json!({"ok": true, "keys": out_keys, "files": out_files}))
	}
}


