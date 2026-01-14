use async_trait::async_trait;
use serde_json::Value;
use crate::agent::AgentCtx;
use crate::tools::Tool;
use tracing::info;
// NOTE: Engine-agnostic ReAct: no DataFusion/SessionContext usage here.
use serde_json::json;

pub struct ApproveAndSaveArtifactTool;

#[async_trait]
impl Tool for ApproveAndSaveArtifactTool {
	fn name(&self) -> &'static str { "approve_and_save_artifact" }
    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
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
		let preview_diff = args.get("preview_diff").and_then(|x| x.as_bool()).unwrap_or(false);

			// Load candidates from thread's resolved_datasets
			let (mut pipeline, mut namespace) = {
			let mut candidates: Vec<(String, String)> = Vec::new();
			if let Some(tid) = ctx.thread_id.as_ref() {
				let store = ctx
					.thread_store
					.as_ref()
					.ok_or_else(|| "thread_store not configured".to_string())?;
				if let Some(log) = store.get(tid).await {
					for step in log.steps.iter().rev() {
						if step.action == "resolved_datasets" {
							if let Some(arr) = step.args.get("candidates").and_then(|x| x.as_array()) {
								for v in arr {
									let p = v.get("pipeline").and_then(|x| x.as_str()).unwrap_or("").to_string();
									let ns = v.get("namespace").and_then(|x| x.as_str()).unwrap_or("").to_string();
									if !p.is_empty() && !ns.is_empty() {
										candidates.push((p, ns));
									}
								}
							}
							break;
						}
					}
				}
			}
			if candidates.is_empty() {
				return Err("No resolved datasets found. First, resolve dataset candidates via vect_query(scope:\"dataset\"), record them, then retry save.".to_string());
			}
				// Selection strategy:
				// - For DBT models, require explicit FQN presence in SQL to avoid invented tables.
				// - For MetricFlow YAML, anchor to the top preflight candidate (no YAML changes needed for spec);
				//   we will add a top-of-file comment to document the assumed dataset.
				if kind == "model" {
					let lc = content.to_lowercase();
					// find at least one referenced candidate (FQN or dbt source)
					let mut referenced: Vec<(String,String)> = Vec::new();
					for (p, ns) in candidates.iter() {
						let fqn = format!("{}.{}", p, ns).to_lowercase();
						let src1 = format!("source('{}','{}')", p, ns);
						let src2 = format!("source(\"{}\",\"{}\")", p, ns);
						let src3 = format!("source('{}', '{}')", p, ns);
						let src4 = format!("source(\"{}\", \"{}\")", p, ns);
						if lc.contains(&fqn) || lc.contains(&src1) || lc.contains(&src2) || lc.contains(&src3) || lc.contains(&src4) {
							referenced.push((p.clone(), ns.clone()));
						}
					}
					if referenced.is_empty() {
						let list = candidates.iter().map(|(p, ns)| format!("{}.{}", p, ns)).collect::<Vec<_>>().join(", ");
						return Err(format!("Model SQL must reference at least one resolved dataset (FQN or dbt source). Use one of: {}", list));
					}
					// choose the first as primary
					referenced[0].clone()
				} else {
					// metric
					candidates.first().cloned().unwrap()
				}
			}; // end selection

			// For metrics, persist a top comment documenting the assumed dataset
			let mut content_final = if kind == "metric" { ensure_dataset_comment(&content, &pipeline, &namespace) } else { content.clone() };

			// Update-in-place: if a focused artifact exists in the thread, enforce writing to that exact key
			let mut name_final = name.clone();
			if let Some(tid) = ctx.thread_id.as_ref() {
				let store = ctx
					.thread_store
					.as_ref()
					.ok_or_else(|| "thread_store not configured".to_string())?;
				if let Some(log) = store.get(tid).await {
					for step in log.steps.iter().rev() {
						if step.action == "artifact_focus" {
							let exists_true = step.observation.get("exists").and_then(|v| v.as_bool()).unwrap_or(false);
							if exists_true {
								let fk = step.args.get("kind").and_then(|v| v.as_str()).unwrap_or("");
								let fname = step.args.get("name").and_then(|v| v.as_str()).unwrap_or("");
								let fp = step.args.get("pipeline").and_then(|v| v.as_str()).unwrap_or("");
								let fns = step.args.get("namespace").and_then(|v| v.as_str()).unwrap_or("");
								if fk != kind {
									return Err(format!("Focused artifact kind is '{}'; cannot save kind '{}'. Update the focused artifact in place.", fk, kind));
								}
								if fname.is_empty() || fp.is_empty() || fns.is_empty() {
									break;
								}
								// Override target to focused artifact
								name_final = fname.to_string();
								pipeline = fp.to_string();
								namespace = fns.to_string();
								// Ensure metric YAML comment reflects focused dataset if metric
								if kind == "metric" {
									content_final = ensure_dataset_comment(&content_final, &pipeline, &namespace);
								}
							}
							break;
						}
					}
				}
			}

		let (current_key, version_key, content_type) = match kind {
			"model" => {
					let base = ctx.keyspace.dbt_prefix(&ctx.scope).trim_end_matches('/').to_string();
					let current = format!("{}/models/{}/{}.sql", base, namespace, name_final);
					let ver = format!("{}/models/{}/_versions/{}/{}.sql", base, namespace, name_final, chrono::Utc::now().format("%Y%m%d_%H%M%S"));
				(current, ver, "text/sql")
			}
			_ => {
					let base = ctx.keyspace.dbt_prefix(&ctx.scope).trim_end_matches('/').to_string();
					let current = format!("{}/metrics/{}/{}.yaml", base, namespace, name_final);
					let ver = format!("{}/metrics/{}/_versions/{}/{}.yaml", base, namespace, name_final, chrono::Utc::now().format("%Y%m%d_%H%M%S"));
				(current, ver, "text/yaml")
			}
		};

		// Fetch existing (if any)
		let existing: Option<String> = match ctx.storage.get_bytes(&current_key).await {
			Ok(bytes) => Some(String::from_utf8_lossy(&bytes).to_string()),
			Err(_) => None,
		};

		if preview_diff {
				let diff = compute_unified_diff(existing.as_deref().unwrap_or(""), &content_final);
			// Log focus step for auditing which artifact is being considered
			if let Some(tid) = ctx.thread_id.as_ref() {
				let store = ctx
					.thread_store
					.as_ref()
					.ok_or_else(|| "thread_store not configured".to_string())?;
				let _ = store.append_step(tid, crate::session::ThreadStep {
					action: "artifact_focus".to_string(),
					args: serde_json::json!({ "kind": kind, "name": name, "pipeline": pipeline, "namespace": namespace }),
					observation: serde_json::json!({ "exists": existing.is_some() }),
					ts: chrono::Utc::now().to_rfc3339(),
					agent: ctx.agent_name.clone(),
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

		// Engine-agnostic: skip DataFusion/sqlrt validation here. Use dbt compilation/tests (via dbt_validate)
		// and/or the configured query provider at runtime.

		// Ensure minimal dbt project scaffolding exists before first save
			let dbt = ctx
				.dbt
				.as_ref()
				.ok_or_else(|| "dbt provider missing".to_string())?;
			let _ = dbt.ensure_minimal_project(&ctx.scope).await;
		// Compute diff stats against existing for summary
			fn diff_stats(old: &str, new: &str) -> (usize, usize) {
				let old_lines: Vec<&str> = old.split('\n').collect();
				let new_lines: Vec<&str> = new.split('\n').collect();
				let mut added = 0usize;
				let mut removed = 0usize;
				let mut i = 0usize;
				let mut j = 0usize;
				while i < old_lines.len() || j < new_lines.len() {
					if i < old_lines.len() && j < new_lines.len() {
						if old_lines[i] == new_lines[j] {
							i += 1; j += 1;
						} else {
							removed += 1;
							added += 1;
							i += 1; j += 1;
						}
					} else if i < old_lines.len() {
						removed += 1; i += 1;
					} else {
						added += 1; j += 1;
					}
				}
				(added, removed)
			}
			let (lines_added, lines_removed) = diff_stats(existing.as_deref().unwrap_or(""), &content_final);
			let status = if existing.is_some() { "modified" } else { "added" };
		// Save current (stable name)
			ctx.storage.put_bytes(&current_key, content_final.as_bytes(), content_type).await?;
		// Save versioned copy
			let _ = ctx.storage.put_bytes(&version_key, content_final.as_bytes(), content_type).await;

		info!("Artifact saved: kind={} key={}", kind, current_key);

		// Upsert/update embedding for this artifact (immediate availability)
		{
			let cfg = crate::llm::config_from_env();
			let model = crate::llm::create_llm(&cfg);
			let vector = ctx.vector.as_ref().ok_or_else(|| "vector provider missing".to_string())?;
			if let Ok(vecs) = model.embed(&[content_final.clone()]) {
				if let Some(v) = vecs.get(0) {
					let atype = if kind == "model" { "dbt_model" } else { "dbt_metricflow" };
					let dataset_id = format!("{}.{}", pipeline, namespace);
					let id = format!("artifact:{}:{}:{}", if kind == "model" { "model" } else { "metric" }, dataset_id, name_final);
					let chunk = crate::vector::lance_store::Chunk {
						id,
						kind: "artifact".to_string(),
						dataset_id,
						field: None,
						text: content_final.clone(),
						vector: v.clone(),
						meta: serde_json::json!({"type": atype, "path": current_key}),
						epoch: chrono::Utc::now().timestamp() as u64,
					};
					let _ = vector.upsert(&ctx.scope, &[chunk]).await;
				}
			}
		}

		// Log step into thread if available
		if let Some(tid) = ctx.thread_id.as_ref() {
			let store = ctx
				.thread_store
				.as_ref()
				.ok_or_else(|| "thread_store not configured".to_string())?;
			let _ = store.append_step(tid, crate::session::ThreadStep {
				action: "artifact_saved".to_string(),
				args: serde_json::json!({ "kind": kind, "name": name, "pipeline": pipeline, "namespace": namespace }),
				observation: serde_json::json!({ "key": current_key, "status": status, "lines_added": lines_added, "lines_removed": lines_removed }),
				ts: chrono::Utc::now().to_rfc3339(),
				agent: ctx.agent_name.clone(),
			}).await;

			// Immediately validate the DBT project and refresh compiled views to guarantee consistency
			// Build s3_prefix: <tenant>/<workspace>/<pipeline>/dbt/
			let s3_prefix = ctx.keyspace.dbt_prefix(&ctx.scope);
			let validate_tool = crate::suites::skippr_model_suite::tools::dbt_validate::DbtValidateTool;
			let args = json!({
				"project_name": format!("{}_project", pipeline.replace('/', "_")),
				"s3_prefix": s3_prefix,
				"target": "datafusion",
				"build": true
			});
			match validate_tool.call(args, ctx).await {
				Ok(obs) => {
					info!("approve_and_save_artifact: dbt_validate observation: {:?}", obs);
					let _ = store.append_step(tid, crate::session::ThreadStep {
						action: "dbt_validate".to_string(),
						args: serde_json::json!({"s3_prefix": s3_prefix, "build": true}),
						observation: obs,
						ts: chrono::Utc::now().to_rfc3339(),
						agent: ctx.agent_name.clone(),
					}).await;
				}
				Err(e) => {
					info!("approve_and_save_artifact: dbt_validate failed: {}", e);
					let _ = store.append_step(tid, crate::session::ThreadStep {
						action: "dbt_validate".to_string(),
						args: serde_json::json!({"s3_prefix": s3_prefix, "build": true}),
						observation: serde_json::json!({"ok": false, "error": e}),
						ts: chrono::Utc::now().to_rfc3339(),
						agent: ctx.agent_name.clone(),
					}).await;
				}
			}
			// No sqlrt/DataFusion registration in engine-agnostic ReAct.
		}

		Ok(serde_json::json!({"ok": true, "key": current_key, "status": status, "lines_added": lines_added, "lines_removed": lines_removed}))
	}
}

// NOTE: DBT paths must be derived from `Keyspace` (scoped).

// Legacy derive_fqn removed: enforcement relies on resolved_datasets from thread

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

fn ensure_dataset_comment(yaml_text: &str, pipeline: &str, namespace: &str) -> String {
	let wanted = format!("# Dataset: {}.{}", pipeline, namespace);
	// If already present (anywhere in the first 5 lines), keep original
	let mut lines: Vec<&str> = yaml_text.split('\n').collect();
	let scan_end = std::cmp::min(lines.len(), 5);
	for i in 0..scan_end {
		if lines[i].trim_start().starts_with("# Dataset:") {
			return yaml_text.to_string();
		}
	}
	// Insert comment at the very top followed by a blank line if first line is not empty/comment
	let mut out = String::new();
	out.push_str(&wanted);
	out.push('\n');
	out.push('\n');
	out.push_str(yaml_text);
	out
}

fn preprocess_model_sql(raw: &str) -> String {
	// Strip Jinja config and template blocks commonly used by dbt
	// Remove lines like "{{ config(...) }}" and unwrap {{ ref('x.y') }} -> x.y
	let mut out = String::new();
	let mut in_block_comment = false;
	for line in raw.lines() {
		let mut l = line.to_string();
		// Remove block comments /* ... */
		if in_block_comment {
			if let Some(end) = l.find("*/") {
				l = l[end + 2..].to_string();
				in_block_comment = false;
			} else {
				continue;
			}
		}
		if let Some(start) = l.find("/*") {
			if let Some(end) = l.find("*/") {
				l.replace_range(start..end + 2, "");
			} else {
				in_block_comment = true;
				l.replace_range(start.., "");
			}
		}
		let lt = l.trim();
		// Drop config-only lines
		if lt.starts_with("{{") && lt.contains("config(") {
			continue;
		}
		// Unwrap ref('x') or ref(\"x\") and source('db','table') → db.table
		let mut ll = l.clone();
		while let Some(start) = ll.find("{{") {
			if let Some(end) = ll[start..].find("}}") {
				let expr = &ll[start + 2..start + end].trim();
				let replacement = if expr.starts_with("ref(") {
					// extract quoted content
					let inner = expr.trim_start_matches("ref(").trim_end_matches(')').trim();
					let inner = inner.trim_matches('"').trim_matches('\'').to_string();
					inner
				} else if expr.starts_with("source(") {
					let inner = expr.trim_start_matches("source(").trim_end_matches(')').trim();
					let parts: Vec<&str> = inner.split(',').map(|s| s.trim().trim_matches('"').trim_matches('\'')).collect();
					if parts.len() == 2 { format!("{}.{}", parts[0], parts[1]) } else { String::new() }
				} else {
					String::new()
				};
				ll.replace_range(start..start + end + 2, &replacement);
			} else {
				break;
			}
		}
		// Remove line comments
		let ll2 = if let Some(pos) = ll.find("--") { ll[..pos].to_string() } else { ll };
		out.push_str(&ll2);
		out.push('\n');
	}
	let cleaned = out.trim().to_string();
	cleaned
}

// NOTE: Model/YAML validation previously used DataFusion/sqlrt; it is intentionally removed.
