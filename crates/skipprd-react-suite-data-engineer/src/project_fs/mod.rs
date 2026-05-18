use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use react_core::agent::AgentCtx;
use react_core::storage::{
    is_storage_not_found_error, retry_delete_object, retry_get_bytes, retry_list_prefix,
    retry_put_bytes,
};

pub mod diff;
pub mod patch;
pub mod yaml;

pub use patch::*;
pub use yaml::*;

#[allow(dead_code)]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplaceListEdit {
    pub start_line: usize,
    pub end_line: usize,
    pub new_text: String,
}

pub fn join_storage_key(ctx: &AgentCtx, rel: &str) -> String {
    let base = ctx
        .keyspace()
        .scoped_prefix(ctx.scope(), &["dbt"])
        .trim_end_matches('/')
        .to_string();
    format!("{}/{}", base, rel)
}

pub fn local_dbt_project_root() -> Option<PathBuf> {
    std::env::var("SKIPPR_LOCAL_DBT_PROJECT_ROOT")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn local_file_path(rel: &str) -> Result<Option<PathBuf>, String> {
    let Some(root) = local_dbt_project_root() else {
        return Ok(None);
    };
    let rel = normalize_rel_path(rel)?;
    Ok(Some(root.join(rel)))
}

pub fn read_file_text_sync(rel: &str) -> Result<Option<String>, String> {
    let Some(path) = local_file_path(rel)? else {
        return Ok(None);
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!(
            "failed to read local dbt file {}: {e}",
            path.display()
        )),
    }
}

pub async fn read_project_file_text(ctx: &AgentCtx, rel: &str) -> Result<Option<String>, String> {
    let rel = normalize_rel_path(rel)?;
    if local_dbt_project_root().is_some() {
        return read_file_text_sync(&rel);
    }
    let key = join_storage_key(ctx, &rel);
    match retry_get_bytes(ctx.storage().as_ref(), &key).await {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).to_string())),
        Err(e) if is_storage_not_found_error(&e) => Ok(None),
        Err(_) => Ok(None),
    }
}

pub async fn project_file_exists(ctx: &AgentCtx, rel: &str) -> Result<bool, String> {
    Ok(read_project_file_text(ctx, rel).await?.is_some())
}

pub async fn list_project_files(ctx: &AgentCtx, prefix: &str) -> Result<Vec<String>, String> {
    let rel_prefix = if prefix.trim().is_empty() {
        String::new()
    } else {
        normalize_rel_path(prefix)?
    };
    if let Some(root) = local_dbt_project_root() {
        let dir = root.join(&rel_prefix);
        let mut out: Vec<String> = Vec::new();
        if dir.exists() {
            for entry in walkdir::WalkDir::new(&dir)
                .min_depth(1)
                .max_depth(32)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_file())
            {
                out.push(local_display_path(&root, entry.path()));
            }
        }
        out.sort();
        return Ok(out);
    }

    let base = ctx
        .keyspace()
        .scoped_prefix(ctx.scope(), &["dbt"])
        .trim_end_matches('/')
        .to_string();
    let key_prefix = if rel_prefix.is_empty() {
        format!("{}/", base)
    } else {
        format!("{}/{}", base, rel_prefix)
    };
    let mut keys = retry_list_prefix(ctx.storage().as_ref(), &key_prefix)
        .await
        .unwrap_or_default();
    keys.sort();
    Ok(keys
        .into_iter()
        .filter_map(|key| {
            key.strip_prefix(&(base.clone() + "/"))
                .map(|rel| rel.to_string())
        })
        .collect())
}

fn local_display_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

pub async fn list_files(ctx: &AgentCtx, prefix: &str, limit: usize) -> Result<Value, String> {
    let rel_prefix = if prefix.trim().is_empty() {
        "models/".to_string()
    } else {
        normalize_rel_path(prefix)?
    };
    if let Some(root) = local_dbt_project_root() {
        let dir = root.join(&rel_prefix);
        let mut out: Vec<Value> = Vec::new();
        if dir.exists() {
            for entry in walkdir::WalkDir::new(&dir)
                .min_depth(1)
                .max_depth(32)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_file())
                .take(limit)
            {
                let rel = local_display_path(&root, entry.path());
                out.push(serde_json::json!({
                    "path": rel,
                    "key": join_storage_key(ctx, &rel),
                    "source": "local_dbt"
                }));
            }
        }
        return Ok(serde_json::json!({"ok": true, "items": out, "source": "local_dbt"}));
    }
    let key_prefix = join_storage_key(ctx, &rel_prefix.trim_start_matches('/'));
    let mut keys = retry_list_prefix(ctx.storage().as_ref(), &key_prefix)
        .await
        .unwrap_or_default();
    keys.sort();
    let mut out: Vec<Value> = Vec::new();
    for k in keys.into_iter().take(limit) {
        let rel = k
            .strip_prefix(
                &(ctx
                    .keyspace()
                    .scoped_prefix(ctx.scope(), &["dbt"])
                    .trim_end_matches('/')
                    .to_string()
                    + "/"),
            )
            .unwrap_or(&k)
            .to_string();
        out.push(serde_json::json!({"path": rel, "key": k}));
    }
    Ok(serde_json::json!({"ok": true, "items": out}))
}

pub async fn get_file(ctx: &AgentCtx, path: &str, max_chars: usize) -> Result<Value, String> {
    let rel = normalize_rel_path(path)?;
    let key = join_storage_key(ctx, &rel);
    if let Some(root) = local_dbt_project_root() {
        let local_path = root.join(&rel);
        match std::fs::read_to_string(&local_path) {
            Ok(text) => {
                let base_sha256 = sha256_hex(&text);
                let existing_line_count = if text.is_empty() {
                    0
                } else {
                    text.lines().count()
                };
                let existing_had_trailing_newline = text.ends_with('\n');
                let content = if max_chars > 0 && text.len() > max_chars {
                    let mut s = text.chars().take(max_chars).collect::<String>();
                    s.push_str("\n... (truncated; use file op=get with higher max_chars for additional content)\n");
                    s
                } else {
                    text
                };
                return Ok(serde_json::json!({
                    "ok": true,
                    "path": rel,
                    "key": key,
                    "source": "local_dbt",
                    "base_sha256": base_sha256,
                    "existing_line_count": existing_line_count,
                    "existing_had_trailing_newline": existing_had_trailing_newline,
                    "content": content
                }));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let bootstrap_missing = rel == PACKAGES_YML || rel == MODELS_SCHEMA_YML;
                if bootstrap_missing {
                    return Ok(serde_json::json!({
                        "ok": true,
                        "path": rel,
                        "key": key,
                        "source": "local_dbt",
                        "exists": false,
                        "missing": true,
                        "base_sha256": "",
                        "existing_line_count": 0,
                        "existing_had_trailing_newline": false,
                        "content": "",
                    }));
                }
                return Ok(serde_json::json!({
                    "ok": false,
                    "path": rel,
                    "key": key,
                    "source": "local_dbt",
                    "error": format!("not found: {}", local_path.display()),
                }));
            }
            Err(e) => {
                return Ok(serde_json::json!({
                    "ok": false,
                    "path": rel,
                    "key": key,
                    "source": "local_dbt",
                    "error": format!("failed to read local dbt file {}: {e}", local_path.display()),
                }));
            }
        }
    }
    match retry_get_bytes(ctx.storage().as_ref(), &key).await {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes).to_string();
            let base_sha256 = {
                use sha2::Digest;
                let mut hasher = sha2::Sha256::new();
                hasher.update(text.as_bytes());
                hex::encode(hasher.finalize())
            };
            let existing_line_count = if text.is_empty() {
                0
            } else {
                text.lines().count()
            };
            let existing_had_trailing_newline = text.ends_with('\n');
            let content = if max_chars > 0 && text.len() > max_chars {
                let mut s = text.chars().take(max_chars).collect::<String>();
                s.push_str("\n... (truncated; use file op=get with higher max_chars for additional content)\n");
                s
            } else {
                text
            };
            Ok(serde_json::json!({
                "ok": true,
                "path": rel,
                "key": key,
                "base_sha256": base_sha256,
                "existing_line_count": existing_line_count,
                "existing_had_trailing_newline": existing_had_trailing_newline,
                "content": content
            }))
        }
        Err(e) => {
            let bootstrap_missing =
                (rel == PACKAGES_YML || rel == MODELS_SCHEMA_YML) && is_storage_not_found_error(&e);
            if bootstrap_missing {
                return Ok(serde_json::json!({
                    "ok": true,
                    "path": rel,
                    "key": key,
                    "exists": false,
                    "missing": true,
                    "base_sha256": "",
                    "existing_line_count": 0,
                    "existing_had_trailing_newline": false,
                    "content": "",
                }));
            }
            Ok(serde_json::json!({
                "ok": false,
                "path": rel,
                "key": key,
                "error": format!("not found or failed to fetch: {}", e),
            }))
        }
    }
}

pub async fn remove_file(
    ctx: &AgentCtx,
    path: &str,
    expected_sha256: Option<&str>,
) -> Result<Value, String> {
    let rel = normalize_rel_path(path)?;
    let key = join_storage_key(ctx, &rel);
    if let Some(root) = local_dbt_project_root() {
        let path = root.join(&rel);
        let existing = std::fs::read_to_string(&path).ok();
        let existed = existing.is_some();
        let base_sha256 = existing.as_deref().map(sha256_hex).unwrap_or_default();
        if let Some(expected) = expected_sha256 {
            if existed && expected != base_sha256 {
                return Err(format!(
                    "expected_sha256 mismatch for {}: expected {}, current {}",
                    rel, expected, base_sha256
                ));
            }
        }
        if existed {
            std::fs::remove_file(&path)
                .map_err(|e| format!("failed to remove local dbt file {}: {e}", path.display()))?;
        }
        return Ok(serde_json::json!({
            "ok": true,
            "mutated": existed,
            "source": "local_dbt",
            "results": [{
                "op": "rm",
                "path": rel,
                "key": key,
                "existed": existed,
                "mutated": existed,
                "base_sha256": base_sha256
            }]
        }));
    }

    let existing = retry_get_bytes(ctx.storage().as_ref(), &key).await.ok();
    let existed = existing.is_some();
    let base_sha256 = existing
        .as_ref()
        .map(|b| sha256_hex(String::from_utf8_lossy(b).as_ref()))
        .unwrap_or_default();

    if let Some(expected) = expected_sha256 {
        if existed && expected != base_sha256 {
            return Err(format!(
                "expected_sha256 mismatch for {}: expected {}, current {}",
                rel, expected, base_sha256
            ));
        }
    }

    if existed {
        retry_delete_object(ctx.storage().as_ref(), &key)
            .await
            .map_err(|e| e.to_string())?;
    }

    Ok(serde_json::json!({
        "ok": true,
        "mutated": existed,
        "results": [{
            "op": "rm",
            "path": rel,
            "key": key,
            "existed": existed,
            "mutated": existed,
            "base_sha256": base_sha256
        }]
    }))
}

pub async fn move_file(
    ctx: &AgentCtx,
    from_path: &str,
    to_path: &str,
    expected_sha256: Option<&str>,
) -> Result<Value, String> {
    let from_rel = normalize_rel_path(from_path)?;
    let to_rel = normalize_rel_path(to_path)?;
    if from_rel == to_rel {
        return Err("mv requires from != to".to_string());
    }
    let from_key = join_storage_key(ctx, &from_rel);
    let to_key = join_storage_key(ctx, &to_rel);
    if let Some(root) = local_dbt_project_root() {
        let from = root.join(&from_rel);
        let to = root.join(&to_rel);
        let bytes = std::fs::read(&from).map_err(|_| format!("not found: {}", from_rel))?;
        let base_sha256 = sha256_hex(String::from_utf8_lossy(&bytes).as_ref());
        if let Some(expected) = expected_sha256 {
            if expected != base_sha256 {
                return Err(format!(
                    "expected_sha256 mismatch for {}: expected {}, current {}",
                    from_rel, expected, base_sha256
                ));
            }
        }
        if to.exists() {
            return Err(format!("destination already exists: {}", to_rel));
        }
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
        }
        std::fs::rename(&from, &to).map_err(|e| {
            format!(
                "failed to move local dbt file {} -> {}: {e}",
                from.display(),
                to.display()
            )
        })?;
        return Ok(serde_json::json!({
            "ok": true,
            "mutated": true,
            "source": "local_dbt",
            "results": [{
                "op": "mv",
                "from": from_rel,
                "to": to_rel,
                "path": to_rel,
                "from_key": from_key,
                "key": to_key,
                "mutated": true,
                "base_sha256": base_sha256,
                "new_sha256": base_sha256
            }]
        }));
    }

    let bytes = retry_get_bytes(ctx.storage().as_ref(), &from_key)
        .await
        .map_err(|_| format!("not found: {}", from_rel))?;
    let base_sha256 = sha256_hex(String::from_utf8_lossy(&bytes).as_ref());
    if let Some(expected) = expected_sha256 {
        if expected != base_sha256 {
            return Err(format!(
                "expected_sha256 mismatch for {}: expected {}, current {}",
                from_rel, expected, base_sha256
            ));
        }
    }

    if retry_get_bytes(ctx.storage().as_ref(), &to_key)
        .await
        .is_ok()
    {
        return Err(format!("destination already exists: {}", to_rel));
    }

    retry_put_bytes(ctx.storage().as_ref(), &to_key, &bytes, "text/plain")
        .await
        .map_err(|e| e.to_string())?;
    retry_delete_object(ctx.storage().as_ref(), &from_key)
        .await
        .map_err(|e| e.to_string())?;

    Ok(serde_json::json!({
        "ok": true,
        "mutated": true,
        "results": [{
            "op": "mv",
            "from": from_rel,
            "to": to_rel,
            "path": to_rel,
            "from_key": from_key,
            "key": to_key,
            "mutated": true,
            "base_sha256": base_sha256,
            "new_sha256": base_sha256
        }]
    }))
}

pub async fn write_file(
    ctx: &AgentCtx,
    datasets: Option<&std::sync::Arc<dyn crate::providers::DatasetCatalogProvider>>,
    path: &str,
    content: &str,
) -> Result<Value, String> {
    let rel = normalize_rel_path(path)?;
    let key = join_storage_key(ctx, &rel);

    let final_content = yaml::postprocess_content(ctx, datasets, &rel, content).await?;

    let existing = if local_dbt_project_root().is_some() {
        read_file_text_sync(&rel)?.map(|text| text.into_bytes())
    } else {
        retry_get_bytes(ctx.storage().as_ref(), &key).await.ok()
    };
    let existed = existing.is_some();
    let old_content = existing
        .as_ref()
        .map(|b| String::from_utf8_lossy(b).to_string())
        .unwrap_or_default();
    let base_sha256 = sha256_hex(&old_content);
    let new_sha256 = sha256_hex(&final_content);
    let mutated = base_sha256 != new_sha256;

    if mutated {
        if let Some(root) = local_dbt_project_root() {
            let path = root.join(&rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
            }
            std::fs::write(&path, final_content.as_bytes())
                .map_err(|e| format!("failed to write local dbt file {}: {e}", path.display()))?;
        } else {
            retry_put_bytes(
                ctx.storage().as_ref(),
                &key,
                final_content.as_bytes(),
                "text/plain",
            )
            .await
            .map_err(|e| e.to_string())?;
        }
    }

    Ok(serde_json::json!({
        "ok": true,
        "mutated": mutated,
        "source": if local_dbt_project_root().is_some() { "local_dbt" } else { "storage" },
        "results": [{
            "op": "write",
            "path": rel,
            "key": key,
            "existed": existed,
            "mutated": mutated,
            "base_sha256": base_sha256,
            "new_sha256": new_sha256
        }]
    }))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PersistedPatch {
    pub source: String,
    pub path: String,
    pub key: String,
    pub existed: bool,
    pub mutated: bool,
    pub base_sha256: String,
    pub new_sha256: String,
    pub lines_added: usize,
    pub lines_removed: usize,
}

pub async fn persist_patch_outcome(
    ctx: &AgentCtx,
    outcome: &PatchOutcome,
    content_type: &str,
) -> Result<PersistedPatch, String> {
    let mutated = outcome.base_sha256 != outcome.new_sha256;
    let source = if let Some(root) = local_dbt_project_root() {
        if mutated {
            let path = root.join(&outcome.rel_path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
            }
            std::fs::write(&path, outcome.content.as_bytes())
                .map_err(|e| format!("failed to write local dbt file {}: {e}", path.display()))?;
        }
        "local_dbt"
    } else {
        if mutated {
            retry_put_bytes(
                ctx.storage().as_ref(),
                &outcome.key,
                outcome.content.as_bytes(),
                content_type,
            )
            .await
            .map_err(|e| e.to_string())?;
        }
        "storage"
    };

    Ok(PersistedPatch {
        source: source.to_string(),
        path: outcome.rel_path.clone(),
        key: outcome.key.clone(),
        existed: outcome.existed,
        mutated,
        base_sha256: outcome.base_sha256.clone(),
        new_sha256: outcome.new_sha256.clone(),
        lines_added: outcome.lines_added,
        lines_removed: outcome.lines_removed,
    })
}

pub async fn write_project_file_via_patch(
    ctx: &AgentCtx,
    datasets: Option<&std::sync::Arc<dyn crate::providers::DatasetCatalogProvider>>,
    rel_path: &str,
    content: &str,
    content_type: &str,
) -> Result<PersistedPatch, String> {
    let rel = normalize_rel_path(rel_path)?;
    let existing = read_project_file_text(ctx, &rel).await?;
    let existed = existing.is_some();
    let old = existing.unwrap_or_default();
    if old == content {
        let key = join_storage_key(ctx, &rel);
        let hash = sha256_hex(&old);
        return Ok(PersistedPatch {
            source: if local_dbt_project_root().is_some() {
                "local_dbt".to_string()
            } else {
                "storage".to_string()
            },
            path: rel,
            key,
            existed,
            mutated: false,
            base_sha256: hash.clone(),
            new_sha256: hash,
            lines_added: 0,
            lines_removed: 0,
        });
    }

    let patch = create_git_patch_text(&old, content, &rel, existed)?;
    let base_sha256 = if existed {
        Some(sha256_hex(&old))
    } else {
        None
    };
    let outcome = apply_patch(
        ctx,
        datasets,
        &rel,
        &patch,
        base_sha256.as_deref(),
        Some(existed),
        PatchApplyKind::UnifiedDiff,
    )
    .await?;
    persist_patch_outcome(ctx, &outcome, content_type).await
}

pub(crate) fn sha256_hex(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    let out = hasher.finalize();
    hex::encode(out)
}

fn is_allowed_rel_path(rel: &str) -> bool {
    let rel = rel.trim();
    if rel.is_empty() {
        return false;
    }
    if rel.starts_with('/') || rel.starts_with('\\') {
        return false;
    }
    if rel.contains("..") {
        return false;
    }
    true
}

#[cfg(test)]
pub(crate) mod test_helpers {
    use super::*;
    use crate::providers::{DatasetId, QueryProvider, QueryResult};
    use async_trait::async_trait;
    use react_core::keyspace::{DefaultKeyspace, Keyspace};
    use react_core::llm::{ChatMessage, LargeLanguageModel};
    use react_core::scope::RequestScope;
    use react_core::storage::StorageAdapter;
    use std::collections::HashMap;
    pub use std::sync::Arc;

    #[derive(Default)]
    pub struct DummyLlm;

    impl LargeLanguageModel for DummyLlm {
        fn chat(
            &self,
            _messages: &[ChatMessage],
            _options: &react_core::llm::LlmCallOptions,
        ) -> Result<String, String> {
            Err("not used".to_string())
        }
        fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            Ok(vec![])
        }
    }

    #[derive(Clone)]
    pub struct MockDatasets {
        pub items: Vec<DatasetId>,
    }

    #[async_trait]
    impl crate::providers::DatasetCatalogProvider for MockDatasets {
        async fn list_datasets(&self) -> Result<Vec<DatasetId>, String> {
            Ok(self.items.clone())
        }
        async fn get_dataset_schema(
            &self,
            _dataset: &DatasetId,
        ) -> Result<Vec<(String, String)>, String> {
            Ok(vec![])
        }
        async fn get_dataset_stats(
            &self,
            _dataset: &DatasetId,
            _max_fields: usize,
        ) -> Result<
            (
                crate::providers::DatasetFieldStats,
                crate::providers::DatasetStats,
            ),
            String,
        > {
            Err("not implemented".to_string())
        }

        fn evidence_capabilities(&self) -> crate::providers::ProviderEvidenceCapabilities {
            crate::providers::ProviderEvidenceCapabilities::schema_only("mock dataset provider")
        }
    }

    pub fn minimal_cfg() -> Arc<react_core::resolved_config::ReactResolvedConfig> {
        Arc::new(react_core::resolved_config::ReactResolvedConfig {
            server: react_core::resolved_config::ServerResolved { port: 1 },
            storage: react_core::resolved_config::StorageResolved {
                mode: react_core::resolved_config::StorageMode::Local,
                bucket: None,
                path: None,
                s3_credentials: None,
            },
            scope: RequestScope::parse("t", "w", "p").expect("valid test scope"),
            llm: react_core::resolved_config::LlmResolved::default(),
            suite_config: serde_json::json!({
                "warehouse": { "kind": "athena", "container": "AwsDataCatalog", "namespace": "test_raw", "extras": {"region":"eu-west-1","workgroup":"wg","result_s3":"s3://x/"} },
                "catalog": { "enabled": false, "refresh_secs": 60, "max_concurrency": 8 },
                "dbt": { "enabled": true, "target": "athena", "naming": { "target_schema": "test", "silver_suffix": "silver", "gold_suffix": "gold" }, "runner": "host" },
                "vector": { "enabled": false }
            }),
        })
    }

    #[derive(Clone, Default)]
    pub struct MockQuery {
        pub schemas: Arc<std::sync::Mutex<HashMap<String, Vec<(String, String)>>>>,
    }

    #[async_trait]
    impl QueryProvider for MockQuery {
        async fn query(&self, _sql: &str) -> Result<QueryResult, String> {
            Err("not implemented".to_string())
        }
        async fn schema(&self, dataset_fqn: &str) -> Result<Vec<(String, String)>, String> {
            let m = self.schemas.lock().unwrap();
            m.get(dataset_fqn)
                .cloned()
                .ok_or_else(|| format!("not found: {}", dataset_fqn))
        }
        async fn sample(
            &self,
            _dataset_fqn: &str,
            _limit: usize,
        ) -> Result<Vec<Vec<String>>, String> {
            Err("not implemented".to_string())
        }
    }

    #[derive(Clone, Default)]
    pub struct MockWarehouse {
        pub schemas: Arc<std::sync::Mutex<HashMap<String, Vec<(String, String)>>>>,
    }

    #[async_trait]
    impl QueryProvider for MockWarehouse {
        async fn query(&self, _sql: &str) -> Result<QueryResult, String> {
            Err("not implemented".to_string())
        }
        async fn schema(&self, dataset_fqn: &str) -> Result<Vec<(String, String)>, String> {
            let m = self.schemas.lock().unwrap();
            m.get(dataset_fqn)
                .cloned()
                .ok_or_else(|| format!("not found: {}", dataset_fqn))
        }
        async fn sample(
            &self,
            _dataset_fqn: &str,
            _limit: usize,
        ) -> Result<Vec<Vec<String>>, String> {
            Err("not implemented".to_string())
        }
        fn max_concurrency(&self) -> usize {
            1
        }
    }

    #[async_trait]
    impl crate::providers::DatasetCatalogProvider for MockWarehouse {
        async fn list_datasets(&self) -> Result<Vec<DatasetId>, String> {
            Ok(vec![])
        }
        async fn get_dataset_schema(
            &self,
            dataset: &DatasetId,
        ) -> Result<Vec<(String, String)>, String> {
            self.schema(&dataset.fqn()).await
        }
        async fn get_dataset_stats(
            &self,
            _dataset: &DatasetId,
            _max_fields: usize,
        ) -> Result<
            (
                crate::providers::DatasetFieldStats,
                crate::providers::DatasetStats,
            ),
            String,
        > {
            Err("not implemented".to_string())
        }

        fn evidence_capabilities(&self) -> crate::providers::ProviderEvidenceCapabilities {
            crate::providers::ProviderEvidenceCapabilities::schema_only("mock warehouse provider")
        }
    }

    impl crate::providers::WarehouseNaming for MockWarehouse {
        fn kind(&self) -> crate::de_config::WarehouseKind {
            crate::de_config::WarehouseKind::default()
        }
        fn parse_dataset_fqn(&self, dataset_fqn: &str) -> Result<DatasetId, String> {
            let raw = dataset_fqn.trim().trim_matches('"').trim_matches('`');
            let parts: Vec<&str> = raw.split('.').collect();
            if parts.len() != 3 {
                return Err("mock dataset id must be <catalog>.<schema>.<table>".to_string());
            }
            Ok(DatasetId {
                catalog: parts[0].to_string(),
                database: parts[1].to_string(),
                table: parts[2].to_string(),
            })
        }
        fn quote_ident(&self, ident: &str) -> String {
            format!("\"{}\"", ident.replace('"', "\"\""))
        }
    }

    pub fn make_ctx(
        storage: Arc<dyn StorageAdapter>,
        query: Option<Arc<dyn QueryProvider>>,
    ) -> AgentCtx {
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let warehouse: Arc<dyn crate::providers::WarehouseProvider> =
            Arc::new(crate::providers::warehouse::NullWarehouseProvider::default());
        let mut actx = react_core::agent::AgentCtxBuilder::new(
            Arc::new(DummyLlm::default()),
            storage,
            scope.clone(),
            keyspace,
            Arc::new(react_core::agent::DefaultPolicy),
        )
        .top_k(1)
        .per_step_timeout_secs(1)
        .max_steps(1)
        .agent_name("test".to_string())
        .resolved_config(Some(minimal_cfg()))
        .build();
        actx.set_capability(Arc::new(crate::ctx_ext::WarehouseCap(warehouse)));
        if let Some(q) = query {
            actx.set_capability(Arc::new(crate::ctx_ext::QueryCap(q)));
        }
        actx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use react_core::error::CoreError;
    use react_core::storage::{cached, ConditionalWriteStatus, StorageAdapter};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_local_dbt_root(root: &Path, test: impl FnOnce()) {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved_local = std::env::var("SKIPPR_LOCAL_DBT_PROJECT_ROOT").ok();
        std::env::set_var("SKIPPR_LOCAL_DBT_PROJECT_ROOT", root);

        test();

        match saved_local {
            Some(value) => std::env::set_var("SKIPPR_LOCAL_DBT_PROJECT_ROOT", value),
            None => std::env::remove_var("SKIPPR_LOCAL_DBT_PROJECT_ROOT"),
        }
    }

    fn with_no_local_dbt_root(test: impl FnOnce()) {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved_local = std::env::var("SKIPPR_LOCAL_DBT_PROJECT_ROOT").ok();
        std::env::remove_var("SKIPPR_LOCAL_DBT_PROJECT_ROOT");

        test();

        if let Some(value) = saved_local {
            std::env::set_var("SKIPPR_LOCAL_DBT_PROJECT_ROOT", value);
        }
    }

    #[derive(Default)]
    struct MissingCountingStorage {
        get_bytes_calls: AtomicUsize,
    }

    impl MissingCountingStorage {
        fn get_bytes_calls(&self) -> usize {
            self.get_bytes_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl StorageAdapter for MissingCountingStorage {
        async fn get_json(&self, key: &str) -> Result<Value, CoreError> {
            Err(CoreError::Storage(format!("get_json('{key}'): not found")))
        }

        async fn put_json(&self, _key: &str, _value: &Value) -> Result<(), CoreError> {
            Ok(())
        }

        async fn put_json_if_etag_matches(
            &self,
            _key: &str,
            _value: &Value,
            _expected_etag: Option<&str>,
        ) -> Result<ConditionalWriteStatus, CoreError> {
            Ok(ConditionalWriteStatus::Written)
        }

        async fn get_bytes(&self, key: &str) -> Result<Vec<u8>, CoreError> {
            self.get_bytes_calls.fetch_add(1, Ordering::SeqCst);
            Err(CoreError::Storage(format!("get_bytes('{key}'): not found")))
        }

        async fn put_bytes(
            &self,
            _key: &str,
            _bytes: &[u8],
            _content_type: &str,
        ) -> Result<(), CoreError> {
            Ok(())
        }

        async fn delete_object(&self, _key: &str) -> Result<(), CoreError> {
            Ok(())
        }

        async fn head_etag(&self, _key: &str) -> Result<Option<String>, CoreError> {
            Ok(None)
        }

        async fn list_prefix(&self, _prefix: &str) -> Result<Vec<String>, CoreError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn optional_packages_yml_miss_uses_storage_cache() {
        let inner = test_helpers::Arc::new(MissingCountingStorage::default());
        let storage = cached(inner.clone());
        let ctx = test_helpers::make_ctx(storage, None);

        for _ in 0..2 {
            let value = get_file(&ctx, PACKAGES_YML, 4000).await.expect("get file");
            assert_eq!(value.get("ok").and_then(Value::as_bool), Some(true));
            assert_eq!(value.get("missing").and_then(Value::as_bool), Some(true));
        }

        assert_eq!(inner.get_bytes_calls(), 1);
    }

    #[test]
    fn local_dbt_root_backs_file_reads_and_writes() {
        let temp = tempfile::tempdir().expect("tempdir");
        with_local_dbt_root(temp.path(), || {
            let storage: test_helpers::Arc<dyn StorageAdapter> = test_helpers::Arc::new(
                react_module_storage_memory::InMemoryStorageAdapter::default(),
            );
            let ctx = test_helpers::make_ctx(storage, None);
            let rt = tokio::runtime::Runtime::new().expect("runtime");

            rt.block_on(async {
                let out = write_file(&ctx, None, "models/core/example.sql", "select 1\n")
                    .await
                    .expect("write local");
                assert_eq!(out.get("source").and_then(Value::as_str), Some("local_dbt"));

                let read = get_file(&ctx, "models/core/example.sql", 0)
                    .await
                    .expect("read local");
                assert_eq!(
                    read.get("source").and_then(Value::as_str),
                    Some("local_dbt")
                );
                let content = read.get("content").and_then(Value::as_str).unwrap_or("");
                assert!(content.contains("select 1"));

                let listed = list_files(&ctx, "models", 10).await.expect("list local");
                let items = listed
                    .get("items")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                assert!(items.iter().any(|item| {
                    item.get("path").and_then(Value::as_str) == Some("models/core/example.sql")
                }));
            });
        });
    }

    #[test]
    fn persist_patch_outcome_writes_local_without_storage_object() {
        let temp = tempfile::tempdir().expect("tempdir");
        with_local_dbt_root(temp.path(), || {
            let storage: test_helpers::Arc<dyn StorageAdapter> = test_helpers::Arc::new(
                react_module_storage_memory::InMemoryStorageAdapter::default(),
            );
            let ctx = test_helpers::make_ctx(storage.clone(), None);
            let rt = tokio::runtime::Runtime::new().expect("runtime");

            rt.block_on(async {
                let rel = "models/core/example.sql";
                let patch = create_git_patch_text("", "select 1\n", rel, false).expect("patch");
                let outcome = apply_patch(
                    &ctx,
                    None,
                    rel,
                    &patch,
                    None,
                    Some(false),
                    PatchApplyKind::UnifiedDiff,
                )
                .await
                .expect("apply patch");
                let persisted = persist_patch_outcome(&ctx, &outcome, "text/sql")
                    .await
                    .expect("persist local");

                assert_eq!(persisted.source, "local_dbt");
                assert!(temp.path().join(rel).is_file());
                assert!(storage.get_bytes(&outcome.key).await.is_err());
            });
        });
    }

    #[test]
    fn persist_patch_outcome_falls_back_to_storage_without_local_root() {
        with_no_local_dbt_root(|| {
            let storage: test_helpers::Arc<dyn StorageAdapter> = test_helpers::Arc::new(
                react_module_storage_memory::InMemoryStorageAdapter::default(),
            );
            let ctx = test_helpers::make_ctx(storage.clone(), None);
            let rt = tokio::runtime::Runtime::new().expect("runtime");

            rt.block_on(async {
                let rel = "models/core/example.sql";
                let patch = create_git_patch_text("", "select 1\n", rel, false).expect("patch");
                let outcome = apply_patch(
                    &ctx,
                    None,
                    rel,
                    &patch,
                    None,
                    Some(false),
                    PatchApplyKind::UnifiedDiff,
                )
                .await
                .expect("apply patch");
                let persisted = persist_patch_outcome(&ctx, &outcome, "text/sql")
                    .await
                    .expect("persist storage");

                assert_eq!(persisted.source, "storage");
                let bytes = storage
                    .get_bytes(&outcome.key)
                    .await
                    .expect("storage object");
                assert!(String::from_utf8_lossy(&bytes).contains("select 1"));
            });
        });
    }

    #[test]
    fn compile_and_write_model_persists_to_local_root() {
        let temp = tempfile::tempdir().expect("tempdir");
        with_local_dbt_root(temp.path(), || {
            let storage: test_helpers::Arc<dyn StorageAdapter> = test_helpers::Arc::new(
                react_module_storage_memory::InMemoryStorageAdapter::default(),
            );
            let ctx = test_helpers::make_ctx(storage.clone(), None);
            let rt = tokio::runtime::Runtime::new().expect("runtime");

            rt.block_on(async {
                let rel = "models/marts/fct_orders.sql";
                let result = crate::tools::model_authoring_engine::compile_and_write_model(
                    &ctx,
                    &crate::sql_first::SqlFirstDraft {
                        sql: "select\n  order_id\nfrom __SOURCE__\n".to_string(),
                        notes: vec!["test note".to_string()],
                    },
                    &[],
                    &std::collections::HashMap::new(),
                    |_| Ok(()),
                    "",
                    rel,
                    None,
                )
                .await
                .expect("write model");

                assert_eq!(result.key, join_storage_key(&ctx, rel));
                let local = std::fs::read_to_string(temp.path().join(rel)).expect("local sql");
                assert!(local.contains("order_id"));
                assert!(storage.get_bytes(&result.key).await.is_err());
            });
        });
    }
}
