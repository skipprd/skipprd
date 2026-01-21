use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use react_core::agent::AgentCtx;
use react_core::providers::DatasetCatalogProvider;
use react_core::tools::Tool;

use crate::data_engineer::project_fs;

pub struct DbtFilesTool {
    pub datasets: Option<Arc<dyn DatasetCatalogProvider>>,
}

#[async_trait]
impl Tool for DbtFilesTool {
    fn name(&self) -> &'static str {
        "dbt_files"
    }

    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        let op = args.get("op").and_then(|x| x.as_str()).unwrap_or("get");
        match op {
            "list" => {
                let prefix = args.get("prefix").and_then(|x| x.as_str()).unwrap_or("").trim();
                let limit = args.get("limit").and_then(|x| x.as_u64()).unwrap_or(200).min(2000) as usize;
                project_fs::list_files(ctx, prefix, limit).await
            }
            "get" => {
                let path = args.get("path").and_then(|x| x.as_str()).ok_or_else(|| "path required".to_string())?;
                let max_chars = args.get("max_chars").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                project_fs::get_file(ctx, path, max_chars).await
            }
            "get_json" => {
                let path = args.get("path").and_then(|x| x.as_str()).ok_or_else(|| "path required".to_string())?;
                let pointer = args.get("pointer").and_then(|x| x.as_str());
                project_fs::get_json(ctx, path, pointer).await
            }
            "manifest_find" => {
                let path = args.get("path").and_then(|x| x.as_str()).unwrap_or("target/manifest.json");
                let unique_id = args.get("unique_id").and_then(|x| x.as_str());
                let name = args.get("name").and_then(|x| x.as_str());
                let resource_type = args.get("resource_type").and_then(|x| x.as_str());
                let limit = args.get("limit").and_then(|x| x.as_u64()).unwrap_or(20).min(200) as usize;
                project_fs::manifest_find(ctx, path, unique_id, name, resource_type, limit).await
            }
            "patch" => {
                let path = args.get("path").and_then(|x| x.as_str()).ok_or_else(|| "path required".to_string())?;
                let rel_path = project_fs::normalize_rel_path(path)?;
                let patch_text = if let Some(patch_text) = args.get("patch_text").and_then(|x| x.as_str()) {
                    patch_text.to_string()
                } else if let Some(content) = args.get("content").and_then(|x| x.as_str()) {
                    let key = project_fs::join_storage_key(ctx, &rel_path);
                    let existing = ctx
                        .storage
                        .get_bytes(&key)
                        .await
                        .ok()
                        .map(|b| String::from_utf8_lossy(&b).to_string())
                        .unwrap_or_default();
                    project_fs::create_patch_text(&existing, content)
                } else {
                    return Err("patch_text or content required".to_string());
                };
                let base_sha256 = args.get("base_sha256").and_then(|x| x.as_str());
                let create_if_missing = args.get("create_if_missing").and_then(|x| x.as_bool()).unwrap_or(false);
                let preview = args.get("preview_diff").and_then(|x| x.as_bool()).unwrap_or(false);
                let outcome = project_fs::apply_patch(ctx, self.datasets.as_ref(), &rel_path, &patch_text, base_sha256, create_if_missing).await?;
                if preview {
                    return Ok(serde_json::json!({
                        "ok": true,
                        "path": outcome.rel_path,
                        "key": outcome.key,
                        "exists": outcome.existed,
                        "base_sha256": outcome.base_sha256,
                        "new_sha256": outcome.new_sha256,
                        "diff": outcome.diff,
                        "lines_added": outcome.lines_added,
                        "lines_removed": outcome.lines_removed
                    }));
                }
                ctx.storage
                    .put_bytes(&outcome.key, outcome.content.as_bytes(), "text/plain")
                    .await?;
                Ok(serde_json::json!({
                    "ok": true,
                    "path": outcome.rel_path,
                    "key": outcome.key,
                    "exists": outcome.existed,
                    "base_sha256": outcome.base_sha256,
                    "new_sha256": outcome.new_sha256,
                    "diff": outcome.diff,
                    "lines_added": outcome.lines_added,
                    "lines_removed": outcome.lines_removed
                }))
            }
            _ => Err("unsupported op; use 'list', 'get', 'get_json', 'manifest_find', or 'patch'".to_string()),
        }
    }
}
