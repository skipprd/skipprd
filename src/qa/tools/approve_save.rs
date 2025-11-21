use async_trait::async_trait;
use serde_json::Value;
use crate::qa::agent::AgentCtx;
use super::Tool;
use tracing::info;
use datafusion::prelude::SessionContext;

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
		let preview_diff = args.get("preview_diff").and_then(|x| x.as_bool()).unwrap_or(false);

			// Load candidates from thread's resolved_datasets
			let (mut pipeline, mut namespace) = {
			let mut candidates: Vec<(String, String)> = Vec::new();
			if let Some(tid) = _ctx.thread_id.as_ref() {
				let store = crate::qa::session::ThreadStore::new();
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
			if let Some(tid) = _ctx.thread_id.as_ref() {
				let store = crate::qa::session::ThreadStore::new();
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
					let base = dbt_base_prefix(&pipeline);
					let current = format!("{}/models/{}/{}.sql", base, namespace, name_final);
					let ver = format!("{}/models/{}/_versions/{}/{}.sql", base, namespace, name_final, chrono::Utc::now().format("%Y%m%d_%H%M%S"));
				(current, ver, "text/sql")
			}
			_ => {
					let base = dbt_base_prefix(&pipeline);
					let current = format!("{}/metrics/{}/{}.yaml", base, namespace, name_final);
					let ver = format!("{}/metrics/{}/_versions/{}/{}.yaml", base, namespace, name_final, chrono::Utc::now().format("%Y%m%d_%H%M%S"));
				(current, ver, "text/yaml")
			}
		};

		// Fetch existing (if any)
		let existing: Option<String> = match crate::helpers::s3::get_bytes(&current_key).await {
			Ok(bytes) => Some(String::from_utf8_lossy(&bytes).to_string()),
			Err(_) => None,
		};

		if preview_diff {
				let diff = compute_unified_diff(existing.as_deref().unwrap_or(""), &content_final);
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

		// Validate using thread-scoped DF context and only the referenced datasets
			let ctx_df: SessionContext = if let Some(tid) = _ctx.thread_id.as_ref() {
				crate::ws::agent_runner::get_or_create_thread_ctx(tid)
			} else {
				SessionContext::new()
			};
			// Build a minimal registration set: any resolved candidates referenced in content
			let mut to_register: Vec<(String,String)> = vec![(pipeline.clone(), namespace.clone())];
			if let Some(tid) = _ctx.thread_id.as_ref() {
				let store = crate::qa::session::ThreadStore::new();
				if let Some(log) = store.get(tid).await {
					let mut uniq = std::collections::HashSet::<String>::new();
					let lc = content.to_lowercase();
					for step in log.steps.iter().rev() {
						if step.action == "resolved_datasets" {
							if let Some(arr) = step.args.get("candidates").and_then(|x| x.as_array()) {
								for v in arr {
									if let (Some(p), Some(ns)) = (v.get("pipeline").and_then(|x| x.as_str()), v.get("namespace").and_then(|x| x.as_str())) {
										let fqn = format!("{}.{}", p, ns).to_lowercase();
										let src1 = format!("source('{}','{}')", p, ns);
										let src2 = format!("source(\"{}\",\"{}\")", p, ns);
										let src3 = format!("source('{}', '{}')", p, ns);
										let src4 = format!("source(\"{}\", \"{}\")", p, ns);
										if lc.contains(&fqn) || lc.contains(&src1) || lc.contains(&src2) || lc.contains(&src3) || lc.contains(&src4) {
											let key = format!("{}.{}", p, ns);
											if uniq.insert(key) {
												to_register.push((p.to_string(), ns.to_string()));
											}
										}
									}
								}
							}
							break;
						}
					}
				}
			}
			crate::ws::agent_runner::pre_register_selected_namespaces(&ctx_df, &to_register).await;
			if kind == "model" {
				if let Err(err0) = validate_model_sql_for_dataset(&content_final, &pipeline, &namespace, &ctx_df).await {
					// Eager fix: if dataset has a 'properties' struct, try prefixing known nested fields and re-validate
					let table_name = format!("{}.{}", pipeline, namespace);
					let mut properties_fields: std::collections::HashSet<String> = std::collections::HashSet::new();
					if let Ok(df0) = ctx_df.table(&table_name).await {
						let schema = df0.schema();
						for f in schema.fields() {
							if f.name() == "properties" {
								use datafusion::arrow::datatypes::DataType;
								if let DataType::Struct(fields) = f.data_type() {
									for ch in fields {
										properties_fields.insert(ch.name().to_string());
									}
								}
								break;
							}
						}
					}
					if !properties_fields.is_empty() {
						let fixed = apply_properties_prefix(&content_final, &properties_fields);
						if fixed != content_final {
							if validate_model_sql_for_dataset(&fixed, &pipeline, &namespace, &ctx_df).await.is_ok() {
								// Return a structured fix suggestion for user approval
								let diff = compute_unified_diff(&content_final, &fixed);
								if let Some(tid) = _ctx.thread_id.as_ref() {
									let store = crate::qa::session::ThreadStore::new();
									let _ = store.append_step(tid, crate::qa::session::ThreadStep {
										action: "eager_fix".to_string(),
										args: serde_json::json!({ "reason": "prefix nested fields under 'properties'", "diff": diff }),
										observation: serde_json::json!({ "ok": true }),
										ts: chrono::Utc::now().to_rfc3339(),
										agent: _ctx.agent_name.clone(),
									}).await;
								}
								return Ok(serde_json::json!({
									"ok": false,
									"error": err0,
									"fix_suggested": true,
									"fixed_content": fixed,
									"diff": compute_unified_diff(&content_final, &fixed),
									"kind": kind,
									"name": name_final,
									"pipeline": pipeline,
									"namespace": namespace
								}));
							}
						}
					}
					return Err(err0);
				}
			} else {
				validate_metric_yaml_for_dataset(&content_final, &pipeline, &namespace, &ctx_df).await?;
			}

		// Ensure minimal dbt project scaffolding exists before first save
			let _ = crate::qa::dbt::ensure_minimal_project(&pipeline).await;
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
			crate::helpers::s3::put_bytes(&current_key, content_final.as_bytes(), content_type).await.map_err(|e| format!("{:?}", e))?;
		// Save versioned copy
			let _ = crate::helpers::s3::put_bytes(&version_key, content_final.as_bytes(), content_type).await;

		info!("Artifact saved: kind={} key={}", kind, current_key);

		// Upsert/update embedding for this artifact (immediate availability)
		{
			let cfg = crate::llm::config_from_env();
			let model = crate::llm::create_llm(&cfg);
			if let Ok(vecs) = model.embed(&[content_final.clone()]) {
				if let Some(v) = vecs.get(0) {
					let atype = if kind == "model" { "dbt_model" } else { "dbt_metricflow" };
					let id = format!("artifact:{}:{}:{}:{}", if kind == "model" { "model" } else { "metric" }, pipeline, namespace, name_final);
					let chunk = crate::qa::vector::lance_store::Chunk {
						id,
						kind: "artifact".to_string(),
						namespace: namespace.clone(),
						field: None,
						text: content_final.clone(),
						vector: v.clone(),
						meta: serde_json::json!({"type": atype, "path": current_key, "s3_uri": format!("s3://{}/{}", crate::helpers::configuration::Config::get_skippr_s3_bucket(), current_key)}),
						epoch: chrono::Utc::now().timestamp() as u64,
					};
					let store = crate::qa::vector::lance_store::LanceDbStore::new(&pipeline);
					let _ = store.upsert(&[chunk]).await;
				}
			}
		}

		// Log step into thread if available
		if let Some(tid) = _ctx.thread_id.as_ref() {
			let store = crate::qa::session::ThreadStore::new();
			let _ = store.append_step(tid, crate::qa::session::ThreadStep {
				action: "artifact_saved".to_string(),
				args: serde_json::json!({ "kind": kind, "name": name, "pipeline": pipeline, "namespace": namespace }),
				observation: serde_json::json!({ "key": current_key, "status": status, "lines_added": lines_added, "lines_removed": lines_removed }),
				ts: chrono::Utc::now().to_rfc3339(),
				agent: _ctx.agent_name.clone(),
			}).await;
		}

		Ok(serde_json::json!({"ok": true, "key": current_key, "status": status, "lines_added": lines_added, "lines_removed": lines_removed}))
	}
}

fn dbt_base_prefix(pipeline: &str) -> String {
	let tenant = crate::helpers::configuration::Config::get_tenant();
	let workspace = crate::helpers::configuration::Config::get_workspace_name();
	format!("{}/{}/{}/dbt", tenant, workspace, pipeline)
}

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

async fn validate_model_sql_for_dataset(raw: &str, pipeline: &str, namespace: &str, ctx: &SessionContext) -> Result<(), String> {
	let cleaned = preprocess_model_sql(raw);
	if cleaned.is_empty() {
		return Err("empty SQL after preprocessing".to_string());
	}
	// Be robust to leading template debris: find first SELECT/WITH token and slice from there
	let trimmed = cleaned.trim_start();
	let lower_all = trimmed.to_lowercase();
	let mut core = trimmed;
	let sel_idx = lower_all.find("select ");
	let with_idx = lower_all.find("with ");
	if let (None, None) = (sel_idx, with_idx) {
		// Provide a hint: echo first 80 chars for debugging
		let snippet: String = trimmed.chars().take(80).collect();
		return Err(format!("model SQL must start with SELECT or WITH; saw: {}", snippet));
	} else {
		let start_idx = match (sel_idx, with_idx) {
			(Some(a), Some(b)) => std::cmp::min(a, b),
			(Some(a), None) => a,
			(None, Some(b)) => b,
			_ => 0,
		};
		core = &trimmed[start_idx..];
	}
	let mut forced = core.trim().to_string();
	if !forced.to_lowercase().contains(" limit ") {
		forced.push_str(" LIMIT 10");
	}
	// Ensure only the selected dataset is registered in this context
	crate::ws::agent_runner::pre_register_selected_namespaces(ctx, &[(pipeline.to_string(), namespace.to_string())]).await;
	// Try to fetch base schema to provide helpful nested-field hints if available
	let mut has_properties_struct = false;
	let table_name = format!("{}.{}", pipeline, namespace);
	if let Ok(df0) = ctx.table(&table_name).await {
		let schema = df0.schema();
		for f in schema.fields() {
			if f.name() == "properties" {
				#[allow(unused_imports)]
				use datafusion::arrow::datatypes::DataType;
				if matches!(f.data_type(), DataType::Struct(_)) {
					has_properties_struct = true;
				}
				break;
			}
		}
	}
	match ctx.sql(&forced).await {
		Ok(df) => {
			match df.collect().await {
				Ok(_) => Ok(()),
				Err(e) => {
					let mut msg = e.to_string();
					if has_properties_struct && msg.contains("No field named ") {
						// Add a concise hint for nested struct access commonly seen in event schemas
						msg.push_str(" Hint: nested fields are under 'properties', e.g., properties.remaining_boosts, properties.active.");
					}
					Err(msg)
				}
			}
		}
		Err(e) => {
			let mut msg = e.to_string();
			if has_properties_struct && msg.contains("No field named ") {
				msg.push_str(" Hint: nested fields are under 'properties', e.g., properties.remaining_boosts, properties.active.");
			}
			Err(msg)
		}
	}
}

async fn validate_metric_yaml_for_dataset(yaml_text: &str, pipeline: &str, namespace: &str, ctx: &SessionContext) -> Result<(), String> {
	// Structural validation
	let parsed_yaml = serde_yaml::from_str::<serde_yaml::Value>(yaml_text).map_err(|e| e.to_string())?;
	// Optional: validate embedded SELECTs against the selected dataset context only
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
	crate::ws::agent_runner::pre_register_selected_namespaces(ctx, &[(pipeline.to_string(), namespace.to_string())]).await;
	for s in sqls.iter() {
		let sl = s.trim().to_lowercase();
		if sl.starts_with("select ") {
			let limited = if sl.contains(" limit ") { s.clone() } else { format!("{} LIMIT 10", s) };
			if let Err(e) = ctx.sql(&limited).await {
				return Err(format!("MetricFlow SQL validation failed: {}", e));
			}
		}
	}
	// Best-effort field compatibility check: ensure expressions reference at least one known column
	let table_name = format!("{}.{}", pipeline, namespace);
	let df = ctx.table(&table_name).await.map_err(|e| format!("Dataset '{}' unavailable: {}", table_name, e))?;
	let mut cols: Vec<String> = Vec::new();
	for f in df.schema().fields() {
		cols.push(f.name().to_lowercase());
	}
	let colset: std::collections::HashSet<&str> = cols.iter().map(|s| s.as_str()).collect();
	let mut exprs: Vec<String> = Vec::new();
	fn collect_exprs(v: &serde_yaml::Value, out: &mut Vec<String>) {
		match v {
			serde_yaml::Value::Mapping(map) => {
				for (k, val) in map {
					if let serde_yaml::Value::String(key) = k {
						let key_l = key.to_lowercase();
						if key_l == "expr" || key_l.contains("expression") {
							if let serde_yaml::Value::String(s) = val { out.push(s.clone()); }
						}
					}
					collect_exprs(val, out);
				}
			}
			serde_yaml::Value::Sequence(arr) => { for el in arr { collect_exprs(el, out); } }
			_ => {}
		}
	}
	collect_exprs(&parsed_yaml, &mut exprs);
	let mut all_ok = true;
	for e in exprs.iter() {
		let el = e.to_lowercase();
		let mut matched = false;
		for c in colset.iter() {
			if el.contains(c) {
				matched = true;
				break;
			}
		}
		if !matched {
			all_ok = false;
			break;
		}
	}
	if !all_ok {
		return Err(format!("MetricFlow YAML expressions may not match fields in '{}'. Inspect schema and align expressions.", table_name));
	}
	Ok(())
}

fn apply_properties_prefix(sql: &str, property_fields: &std::collections::HashSet<String>) -> String {
	// Best-effort textual rewrite:
	// - Replace ".field" with ".properties.field" when not already ".properties."
	// - Replace bare " field" tokens with " properties.field"
	// Avoid double-prefixing.
	let mut out = sql.to_string();
	for f in property_fields.iter() {
		// Replace alias.field -> alias.properties.field
		// We do a manual scan to avoid double-prefix
		let needle = format!(".{}", f);
		let repl = format!(".properties.{}", f);
		let mut idx = 0usize;
		while let Some(pos) = out[idx..].find(&needle) {
			let pos_abs = idx + pos;
			let already = pos_abs >= 11 && &out[pos_abs - 11..pos_abs] == ".properties";
			if !already {
				out.replace_range(pos_abs..pos_abs + needle.len(), &repl);
				idx = pos_abs + repl.len();
			} else {
				idx = pos_abs + needle.len();
			}
		}
		// Replace bare tokens: use simple separators to reduce false positives
		for sep in [" ", "(", ",", "\n", "\t"] {
			let needle2 = format!("{}{}", sep, f);
			let repl2 = format!("{}properties.{}", sep, f);
			// Skip if already properties.field
			let needle2_already = format!("{}properties.{}", sep, f);
			if !out.contains(&needle2_already) && out.contains(&needle2) {
				out = out.replace(&needle2, &repl2);
			}
		}
	}
	out
}


