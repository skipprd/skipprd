use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use react_core::agent::AgentCtx;
use react_core::providers::DatasetCatalogProvider;
use react_core::tools::Tool;
use tracing::info;

use crate::data_engineer::project_files;

pub struct ApproveAndSaveArtifactBatchTool {
    pub datasets: Option<Arc<dyn DatasetCatalogProvider>>,
}

fn yaml_has_top_level_sources(content: &str) -> bool {
    // Fast path: look for an unindented top-level "sources:" key.
    if content.lines().any(|l| l.starts_with("sources:")) {
        return true;
    }
    // Best-effort parse (covers cases where "sources:" isn't on its own line first).
    if let Ok(v) = serde_yaml::from_str::<serde_yaml::Value>(content) {
        if let Some(map) = v.as_mapping() {
            return map
                .keys()
                .any(|k| k.as_str().map(|s| s == "sources").unwrap_or(false));
        }
    }
    false
}

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
        if args.get("preview_diff").is_some() {
            return Err("preview_diff is no longer supported; remove it from the request".to_string());
        }
        let mut out_keys: Vec<String> = Vec::new();
        let mut out_files: Vec<Value> = Vec::new();
        let warnings: Vec<String> = Vec::new();

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
            let content = it
                .get("content")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
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
            let explicit_path = it
                .get("path")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            if content.trim().is_empty() {
                return Err(
                    "each item requires {content} and either a {path} (kind='file') or {dataset_id,name} (kind='model'|'metric')".to_string(),
                );
            }
            let base = ctx
                .keyspace
                .dbt_prefix(&ctx.scope)
                .trim_end_matches('/')
                .to_string();

            // Derive path and type
            let (current_key, content_type, rel_path) = if let Some(path) = explicit_path {
                // Save arbitrary file under provided relative path
                let rel = path.trim_start_matches('/').to_string();
                // Guardrail: dbt sources must be defined in ONE place to avoid dbt compilation errors.
                // If an agent tries to create sources in multiple YAMLs (e.g. models/sources.yml and
                // models/staging/schema.yml), dbt will fail with duplicate source names.
                if (rel.ends_with(".yml") || rel.ends_with(".yaml"))
                    && rel != project_files::MODELS_SCHEMA_YML
                    && yaml_has_top_level_sources(&content)
                {
                    return Err(format!(
                        "DBT sources must be defined ONLY in {0}. This file appears to contain a top-level 'sources:' block: {1}. Move/merge those sources into {0} (use artifacts op=get to read existing), then retry.",
                        project_files::MODELS_SCHEMA_YML,
                        rel
                    ));
                }
                let current = format!("{}/{}", base, rel);
                let ct = if rel.ends_with(".sql") {
                    "text/sql"
                } else if rel.ends_with(".yaml") || rel.ends_with(".yml") {
                    "text/yaml"
                } else if rel.ends_with(".md") {
                    "text/markdown"
                } else {
                    "text/plain"
                };
                (current, ct, rel)
            } else if kind == "model" {
                // Special-case: dbt schema.yml
                if dataset_id == "models"
                    && name == "schema"
                    && content.trim_start().to_lowercase().starts_with("version:")
                {
                    let rel = project_files::MODELS_SCHEMA_YML.to_string();
                    let current = format!("{}/{}", base, rel);
                    (current, "text/yaml", rel)
                } else {
                    if dataset_id.is_empty() || name.is_empty() {
                        return Err("model items require {dataset_id,name} (or use kind='file' with a path)".to_string());
                    }
                    let dir = encode_key_component(&dataset_id);
                    let rel = format!("models/{}/{}.sql", dir, name);
                    let current = format!("{}/{}", base, rel);
                    (current, "text/sql", rel)
                }
            } else if kind == "metric" {
                if dataset_id.is_empty() || name.is_empty() {
                    return Err(
                        "metric items require {dataset_id,name} (or use kind='file' with a path)"
                            .to_string(),
                    );
                }
                let dir = encode_key_component(&dataset_id);
                let rel = format!("metrics/{}/{}.yaml", dir, name);
                let current = format!("{}/{}", base, rel);
                (current, "text/yaml", rel)
            } else {
                return Err("file kind requires a 'path'".to_string());
            };

            let existing: Option<String> = match ctx.storage.get_bytes(&current_key).await {
                Ok(bytes) => Some(String::from_utf8_lossy(&bytes).to_string()),
                Err(_) => None,
            };

            // Ensure minimal project file
            let dbt = ctx
                .dbt
                .as_ref()
                .ok_or_else(|| "dbt provider missing".to_string())?;
            if let Err(e) = dbt.ensure_minimal_project(&ctx.scope).await {
                return Ok(serde_json::json!({
                    "ok": false,
                    "error": format!("failed to ensure minimal dbt project: {e}"),
                    "keys": out_keys,
                    "files": out_files,
                    "warnings": warnings,
                }));
            }

            let old_text = existing.clone().unwrap_or_default();
            let patch_text =
                crate::data_engineer::project_fs::hunks_only_full_replace_patch(&old_text, &content);
            let outcome = crate::data_engineer::project_fs::apply_patch(
                ctx,
                self.datasets.as_ref(),
                &rel_path,
                &patch_text,
                None,
                None,
                crate::data_engineer::project_fs::PatchApplyKind::UnifiedDiff,
            )
            .await?;
            ctx.storage
                .put_bytes(&current_key, outcome.content.as_bytes(), content_type)
                .await?;
            info!("Artifact saved (batch): kind={} key={}", kind, current_key);
            out_keys.push(current_key.clone());

            let status = if outcome.existed { "modified" } else { "added" };
            out_files.push(serde_json::json!({
                "key": current_key,
                "status": status,
                "lines_added": outcome.lines_added,
                "lines_removed": outcome.lines_removed
            }));
        }
        Ok(
            serde_json::json!({"ok": true, "keys": out_keys, "files": out_files, "warnings": warnings}),
        )
    }
}
