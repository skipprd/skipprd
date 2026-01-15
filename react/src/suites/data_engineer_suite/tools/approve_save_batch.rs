use async_trait::async_trait;
use serde_json::Value;

use crate::agent::AgentCtx;
use crate::tools::Tool;
use tracing::info;

pub struct ApproveAndSaveArtifactBatchTool;

fn encode_key_component(s: &str) -> String {
    // Match Keyspace / provider encoding: keep a conservative safe set; percent-encode the rest.
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let c = *b as char;
        let safe = c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.';
        if safe {
            out.push(c);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn compute_unified_diff(old: &str, new: &str) -> String {
    // Simple line-wise diff
    let old_lines: Vec<&str> = old.split('\n').collect();
    let new_lines: Vec<&str> = new.split('\n').collect();
    let mut out: Vec<String> = Vec::new();
    out.push("--- original".to_string());
    out.push("+++ modified".to_string());
    let mut i = 0usize;
    let mut j = 0usize;
    while i < old_lines.len() || j < new_lines.len() {
        if i < old_lines.len() && j < new_lines.len() {
            if old_lines[i] == new_lines[j] {
                out.push(format!(" {}", old_lines[i]));
                i += 1;
                j += 1;
            } else {
                out.push(format!("- {}", old_lines[i]));
                out.push(format!("+ {}", new_lines[j]));
                i += 1;
                j += 1;
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
    fn name(&self) -> &'static str {
        "approve_and_save_artifact_batch"
    }

    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        let items = args
            .get("items")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        if items.is_empty() {
            return Ok(serde_json::json!({"ok": true, "keys": []}));
        }
        let preview = args.get("preview_diff").and_then(|x| x.as_bool()).unwrap_or(false);
        let mut out_diffs: Vec<Value> = Vec::new();
        let mut out_keys: Vec<String> = Vec::new();
        let mut out_files: Vec<Value> = Vec::new();

        for it in items {
            let kind = it.get("kind").and_then(|x| x.as_str()).unwrap_or("");
            if kind != "model" && kind != "metric" && kind != "file" {
                return Err("kind must be 'model' or 'metric' or 'file'".to_string());
            }
            let name = it
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let content = it.get("content").and_then(|x| x.as_str()).unwrap_or("").to_string();
            // Refactor: replace legacy {pipeline,namespace} with {dataset_id} for grouping,
            // and allow saving to models/<dataset_id>/<name>.sql and metrics/<dataset_id>/<name>.yaml.
            // Back-compat: accept legacy `namespace` as an alias for `dataset_id`.
            let dataset_id = it
                .get("dataset_id")
                .or_else(|| it.get("namespace"))
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let explicit_path = it.get("path").and_then(|x| x.as_str()).map(|s| s.to_string());
            if content.trim().is_empty() {
                return Err(
                    "each item requires {content} and either a {path} (kind='file') or {dataset_id,name} (kind='model'|'metric')".to_string(),
                );
            }
            let base = ctx.keyspace.dbt_prefix(&ctx.scope).trim_end_matches('/').to_string();

            // Derive path and type
            let (current_key, version_key, content_type) = if let Some(path) = explicit_path {
                // Save arbitrary file under provided relative path
                let rel = path.trim_start_matches('/').to_string();
                let current = format!("{}/{}", base, rel);
                let ver = String::new();
                let ct = if rel.ends_with(".sql") {
                    "text/sql"
                } else if rel.ends_with(".yaml") || rel.ends_with(".yml") {
                    "text/yaml"
                } else if rel.ends_with(".md") {
                    "text/markdown"
                } else {
                    "text/plain"
                };
                (current, ver, ct)
            } else if kind == "model" {
                // Special-case: dbt schema.yml
                if dataset_id == "models" && name == "schema" && content.trim_start().to_lowercase().starts_with("version:")
                {
                    let current = format!("{}/models/schema.yml", base);
                    (current, String::new(), "text/yaml")
                } else {
                    if dataset_id.is_empty() || name.is_empty() {
                        return Err("model items require {dataset_id,name} (or use kind='file' with a path)".to_string());
                    }
                    let dir = encode_key_component(&dataset_id);
                    let current = format!("{}/models/{}/{}.sql", base, dir, name);
                    let ver = format!(
                        "{}/models/{}/_versions/{}/{}.sql",
                        base,
                        dir,
                        name,
                        chrono::Utc::now().format("%Y%m%d_%H%M%S")
                    );
                    (current, ver, "text/sql")
                }
            } else if kind == "metric" {
                if dataset_id.is_empty() || name.is_empty() {
                    return Err("metric items require {dataset_id,name} (or use kind='file' with a path)".to_string());
                }
                let dir = encode_key_component(&dataset_id);
                let current = format!("{}/metrics/{}/{}.yaml", base, dir, name);
                let ver = format!(
                    "{}/metrics/{}/_versions/{}/{}.yaml",
                    base,
                    dir,
                    name,
                    chrono::Utc::now().format("%Y%m%d_%H%M%S")
                );
                (current, ver, "text/yaml")
            } else {
                return Err("file kind requires a 'path'".to_string());
            };

            let existing: Option<String> = match ctx.storage.get_bytes(&current_key).await {
                Ok(bytes) => Some(String::from_utf8_lossy(&bytes).to_string()),
                Err(_) => None,
            };

            if preview {
                let diff = compute_unified_diff(existing.as_deref().unwrap_or(""), &content);
                let (lines_added, lines_removed) = diff_stats(existing.as_deref().unwrap_or(""), &content);
                out_diffs.push(serde_json::json!({
                    "name": name,
                    "dataset_id": dataset_id,
                    "kind": kind,
                    "key": current_key,
                    "diff": diff,
                    "exists": existing.is_some(),
                    "lines_added": lines_added,
                    "lines_removed": lines_removed
                }));
                continue;
            }

            // Ensure minimal project file
            let dbt = ctx.dbt.as_ref().ok_or_else(|| "dbt provider missing".to_string())?;
            let _ = dbt.ensure_minimal_project(&ctx.scope).await;

            ctx.storage.put_bytes(&current_key, content.as_bytes(), content_type).await?;
            if !version_key.is_empty() {
                let _ = ctx.storage.put_bytes(&version_key, content.as_bytes(), content_type).await;
            }
            info!("Artifact saved (batch): kind={} key={}", kind, current_key);
            out_keys.push(current_key.clone());

            let (lines_added, lines_removed) = diff_stats(existing.as_deref().unwrap_or(""), &content);
            let status = if existing.is_some() { "modified" } else { "added" };
            out_files.push(serde_json::json!({
                "key": current_key,
                "status": status,
                "lines_added": lines_added,
                "lines_removed": lines_removed
            }));
        }

        if preview {
            return Ok(serde_json::json!({"ok": true, "diffs": out_diffs}));
        }
        Ok(serde_json::json!({"ok": true, "keys": out_keys, "files": out_files}))
    }
}

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
                i += 1;
                j += 1;
            } else {
                removed += 1;
                added += 1;
                i += 1;
                j += 1;
            }
        } else if i < old_lines.len() {
            removed += 1;
            i += 1;
        } else {
            added += 1;
            j += 1;
        }
    }
    (added, removed)
}

