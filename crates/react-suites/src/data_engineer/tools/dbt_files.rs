use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;

use sha2::{Digest, Sha256};

use react_core::agent::AgentCtx;
use react_core::providers::DatasetCatalogProvider;
use react_core::tools::Tool;

use crate::data_engineer::project_fs;

pub struct DbtFilesTool {
    pub datasets: Option<Arc<dyn DatasetCatalogProvider>>,
}

fn sha256_hex(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    let out = hasher.finalize();
    hex::encode(out)
}

fn parse_one_or_many<T: DeserializeOwned>(args: &Value, key: &str) -> Result<Vec<T>, String> {
    let Some(v) = args.get(key) else {
        return Ok(Vec::new());
    };
    if v.is_null() {
        return Ok(Vec::new());
    }
    if v.is_array() {
        serde_json::from_value::<Vec<T>>(v.clone())
            .map_err(|e| format!("{} parse error: {}", key, e))
    } else {
        let one = serde_json::from_value::<T>(v.clone())
            .map_err(|e| format!("{} parse error: {}", key, e))?;
        Ok(vec![one])
    }
}

fn patch_contract_error(msg: &str) -> String {
    format!(
        "dbt_files op=patch contract violation: {}\n\n\
Allowed shape:\n\
- args.op = \"patch\"\n\
- preview_diff?: bool (TOP LEVEL ONLY)\n\
- Provide EXACTLY ONE of:\n\
  - replace_file: {{path,new_text,expected_sha256?}} or array\n\
  - replace_range: {{path,start_line,end_line,new_text,expected_sha256?}} or array\n\
  - replace_list: {{path,edits:[{{start_line,end_line,new_text}}],expected_sha256?}} or array\n\
\n\
Common errors:\n\
- preview_diff must NOT be nested under replace_* objects\n\
- replace_file must be an object/array (not a string)\n\
- path and new_text are required\n",
        msg
    )
}

fn validate_patch_args_shape(args: &Value) -> Result<(), String> {
    // Enforce top-level preview_diff only (never nested).
    if let Some(v) = args.get("replace_file") {
        if let Some(obj) = v.as_object() {
            if obj.contains_key("preview_diff") {
                return Err(patch_contract_error("replace_file.preview_diff is not allowed (preview_diff must be top-level args.preview_diff)"));
            }
        }
        if let Some(arr) = v.as_array() {
            for (i, it) in arr.iter().enumerate() {
                if let Some(obj) = it.as_object() {
                    if obj.contains_key("preview_diff") {
                        return Err(patch_contract_error(&format!("replace_file[{}].preview_diff is not allowed (preview_diff must be top-level args.preview_diff)", i)));
                    }
                }
            }
        }
        if v.is_string() {
            return Err(patch_contract_error(
                "replace_file must be an object or array (got string)",
            ));
        }
    }
    if let Some(v) = args.get("replace_range") {
        if let Some(obj) = v.as_object() {
            if obj.contains_key("preview_diff") {
                return Err(patch_contract_error("replace_range.preview_diff is not allowed (preview_diff must be top-level args.preview_diff)"));
            }
        }
        if let Some(arr) = v.as_array() {
            for (i, it) in arr.iter().enumerate() {
                if let Some(obj) = it.as_object() {
                    if obj.contains_key("preview_diff") {
                        return Err(patch_contract_error(&format!("replace_range[{}].preview_diff is not allowed (preview_diff must be top-level args.preview_diff)", i)));
                    }
                }
            }
        }
        if v.is_string() {
            return Err(patch_contract_error(
                "replace_range must be an object or array (got string)",
            ));
        }
    }
    if let Some(v) = args.get("replace_list") {
        if let Some(obj) = v.as_object() {
            if obj.contains_key("preview_diff") {
                return Err(patch_contract_error("replace_list.preview_diff is not allowed (preview_diff must be top-level args.preview_diff)"));
            }
        }
        if let Some(arr) = v.as_array() {
            for (i, it) in arr.iter().enumerate() {
                if let Some(obj) = it.as_object() {
                    if obj.contains_key("preview_diff") {
                        return Err(patch_contract_error(&format!("replace_list[{}].preview_diff is not allowed (preview_diff must be top-level args.preview_diff)", i)));
                    }
                }
            }
        }
        if v.is_string() {
            return Err(patch_contract_error(
                "replace_list must be an object or array (got string)",
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaceFileArgs {
    path: String,
    new_text: String,
    expected_sha256: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaceRangeArgs {
    path: String,
    start_line: usize,
    end_line: usize,
    new_text: String,
    expected_sha256: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaceListEditArgs {
    start_line: usize,
    end_line: usize,
    new_text: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaceListArgs {
    path: String,
    edits: Vec<ReplaceListEditArgs>,
    expected_sha256: Option<String>,
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
                let prefix = args
                    .get("prefix")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .trim();
                let limit = args
                    .get("limit")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(200)
                    .min(2000) as usize;
                project_fs::list_files(ctx, prefix, limit).await
            }
            "get" => {
                let path = args
                    .get("path")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| "path required".to_string())?;
                let max_chars =
                    args.get("max_chars").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                project_fs::get_file(ctx, path, max_chars).await
            }
            "get_json" => {
                let path = args
                    .get("path")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| "path required".to_string())?;
                let pointer = args.get("pointer").and_then(|x| x.as_str());
                project_fs::get_json(ctx, path, pointer).await
            }
            "manifest_find" => {
                let path = args
                    .get("path")
                    .and_then(|x| x.as_str())
                    .unwrap_or("target/manifest.json");
                let unique_id = args.get("unique_id").and_then(|x| x.as_str());
                let name = args.get("name").and_then(|x| x.as_str());
                let resource_type = args.get("resource_type").and_then(|x| x.as_str());
                let limit = args
                    .get("limit")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(20)
                    .min(200) as usize;
                project_fs::manifest_find(ctx, path, unique_id, name, resource_type, limit).await
            }
            "patch" => {
                let preview = args
                    .get("preview_diff")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false);
                validate_patch_args_shape(&args)?;

                // Optional single-file guard: if provided, ensure the patch bundle targets exactly this rel path.
                let want_rel_path = args
                    .get("path")
                    .and_then(|x| x.as_str())
                    .map(project_fs::normalize_rel_path)
                    .transpose()?;

                // Hard-removed: unified diffs are not accepted as input (too flaky for LLMs).
                if args.get("unified_git_style_patch").is_some() {
                    return Err(
                        "dbt_files op=patch no longer accepts unified_git_style_patch. Use exactly one of: replace_file | replace_range | replace_list"
                            .to_string(),
                    );
                }
                let replace_file_ops: Vec<ReplaceFileArgs> =
                    parse_one_or_many(&args, "replace_file")?;
                let replace_range_ops: Vec<ReplaceRangeArgs> =
                    parse_one_or_many(&args, "replace_range")?;
                let replace_list_ops: Vec<ReplaceListArgs> =
                    parse_one_or_many(&args, "replace_list")?;

                let mut provided = 0usize;
                if !replace_file_ops.is_empty() {
                    provided += 1;
                }
                if !replace_range_ops.is_empty() {
                    provided += 1;
                }
                if !replace_list_ops.is_empty() {
                    provided += 1;
                }
                if provided != 1 {
                    return Err(
                        "dbt_files op=patch requires exactly one of: replace_file | replace_range | replace_list"
                            .to_string(),
                    );
                }

                // Deterministic application: compute intended file contents and use FullOverwrite fast-path.
                let mut outcomes: Vec<project_fs::PatchOutcome> = Vec::new();
                let mut seen: HashSet<String> = HashSet::new();
                if !replace_file_ops.is_empty() {
                    for rf in replace_file_ops.into_iter() {
                        let rel = project_fs::normalize_rel_path(&rf.path)?;
                        if !seen.insert(rel.clone()) {
                            return Err(format!("replace_file contains duplicate path: {}", rel));
                        }
                        let key = project_fs::join_storage_key(ctx, &rel);
                        let existing_opt = ctx
                            .storage
                            .get_bytes(&key)
                            .await
                            .ok()
                            .map(|b| String::from_utf8_lossy(&b).to_string());
                        let old = existing_opt.unwrap_or_default();
                        let base_sha256 = sha256_hex(&old);
                        if let Some(expected) = rf.expected_sha256.as_deref() {
                            if expected != base_sha256 {
                                return Err(format!(
                                    "expected_sha256 mismatch for {}: expected {}, got {}",
                                    rel, expected, base_sha256
                                ));
                            }
                        }
                        let out = project_fs::apply_patch(
                            ctx,
                            self.datasets.as_ref(),
                            &rel,
                            &rf.new_text,
                            Some(base_sha256.as_str()),
                            project_fs::PatchApplyKind::FullOverwrite,
                        )
                        .await?;
                        outcomes.push(out);
                    }
                } else if !replace_range_ops.is_empty() {
                    for rr in replace_range_ops.into_iter() {
                        let rel = project_fs::normalize_rel_path(&rr.path)?;
                        if !seen.insert(rel.clone()) {
                            return Err(format!("replace_range contains duplicate path: {}", rel));
                        }
                        let key = project_fs::join_storage_key(ctx, &rel);
                        let existing = ctx
                            .storage
                            .get_bytes(&key)
                            .await
                            .map_err(|_| format!("not found: {}", rel))
                            .map(|b| String::from_utf8_lossy(&b).to_string())?;
                        let base_sha256 = sha256_hex(&existing);
                        if let Some(expected) = rr.expected_sha256.as_deref() {
                            if expected != base_sha256 {
                                return Err(format!(
                                    "expected_sha256 mismatch for {}: expected {}, got {}",
                                    rel, expected, base_sha256
                                ));
                            }
                        }
                        let new_text = project_fs::apply_replace_range(
                            &existing,
                            rr.start_line,
                            rr.end_line,
                            &rr.new_text,
                        )?;
                        let out = project_fs::apply_patch(
                            ctx,
                            self.datasets.as_ref(),
                            &rel,
                            &new_text,
                            Some(base_sha256.as_str()),
                            project_fs::PatchApplyKind::FullOverwrite,
                        )
                        .await?;
                        outcomes.push(out);
                    }
                } else if !replace_list_ops.is_empty() {
                    for rl in replace_list_ops.into_iter() {
                        let rel = project_fs::normalize_rel_path(&rl.path)?;
                        if !seen.insert(rel.clone()) {
                            return Err(format!("replace_list contains duplicate path: {}", rel));
                        }
                        let key = project_fs::join_storage_key(ctx, &rel);
                        let existing = ctx
                            .storage
                            .get_bytes(&key)
                            .await
                            .map_err(|_| format!("not found: {}", rel))
                            .map(|b| String::from_utf8_lossy(&b).to_string())?;
                        let base_sha256 = sha256_hex(&existing);
                        if let Some(expected) = rl.expected_sha256.as_deref() {
                            if expected != base_sha256 {
                                return Err(format!(
                                    "expected_sha256 mismatch for {}: expected {}, got {}",
                                    rel, expected, base_sha256
                                ));
                            }
                        }
                        let edits: Vec<project_fs::ReplaceListEdit> = rl
                            .edits
                            .into_iter()
                            .map(|e| project_fs::ReplaceListEdit {
                                start_line: e.start_line,
                                end_line: e.end_line,
                                new_text: e.new_text,
                            })
                            .collect();
                        let new_text = project_fs::apply_replace_list(&existing, &edits)?;
                        let out = project_fs::apply_patch(
                            ctx,
                            self.datasets.as_ref(),
                            &rel,
                            &new_text,
                            Some(base_sha256.as_str()),
                            project_fs::PatchApplyKind::FullOverwrite,
                        )
                        .await?;
                        outcomes.push(out);
                    }
                } else {
                    return Err("invalid patch request".to_string());
                }
                if outcomes.is_empty() {
                    return Err("patch produced no file changes".to_string());
                }

                if let Some(want) = want_rel_path.as_ref() {
                    let matches: Vec<&project_fs::PatchOutcome> =
                        outcomes.iter().filter(|o| &o.rel_path == want).collect();
                    if matches.len() != 1 || outcomes.len() != 1 {
                        return Err(format!(
                            "patch must target exactly one file '{}' when args.path is provided (got {} file diffs)",
                            want,
                            outcomes.len()
                        ));
                    }
                }

                // Canonical applied patch: based on *postprocessed* file content actually produced by apply_patch.
                outcomes.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
                let applied_patch_text = outcomes
                    .iter()
                    .map(|o| o.git_patch.trim_end().to_string())
                    .collect::<Vec<String>>()
                    .join("\n\n");

                let mut results: Vec<Value> = Vec::new();
                let mut written_keys: Vec<String> = Vec::new();
                let mut mutated_any = false;
                for outcome in outcomes.into_iter() {
                    let mutated = outcome.base_sha256 != outcome.new_sha256;
                    mutated_any = mutated_any || mutated;
                    if !preview {
                        ctx.storage
                            .put_bytes(&outcome.key, outcome.content.as_bytes(), "text/plain")
                            .await?;
                        written_keys.push(outcome.key.clone());
                    }
                    results.push(serde_json::json!({
                        "path": outcome.rel_path,
                        "key": outcome.key,
                        "exists": outcome.existed,
                        "mutated": mutated,
                        "base_sha256": outcome.base_sha256,
                        "new_sha256": outcome.new_sha256,
                        "git_patch": outcome.git_patch,
                        "diff": outcome.diff,
                        "lines_added": outcome.lines_added,
                        "lines_removed": outcome.lines_removed
                    }));
                }

                Ok(serde_json::json!({
                    "ok": true,
                    "preview": preview,
                    "mutated": mutated_any,
                    "applied_patch_text": applied_patch_text,
                    "written_keys": written_keys,
                    "results": results
                }))
            }
            _ => Err(
                "unsupported op; use 'list', 'get', 'get_json', 'manifest_find', or 'patch'"
                    .to_string(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use react_core::agent::DefaultPolicy;
    use react_core::keyspace::{DefaultKeyspace, Keyspace};
    use react_core::llm::NullModel;
    use react_core::scope::RequestScope;
    use react_core::storage::{InMemoryStorageAdapter, StorageAdapter};

    fn minimal_cfg() -> Arc<config::ReactResolvedConfig> {
        Arc::new(config::ReactResolvedConfig {
            server: config::ServerResolved { port: 1 },
            storage: config::StorageResolved {
                bucket: "b".to_string(),
            },
            scope: RequestScope {
                tenant: "t".to_string(),
                workspace: "w".to_string(),
                project_id: "p".to_string(),
            },
            llm: config::LlmResolved::default(),
            providers: config::ProvidersResolved {
                warehouse: config::WarehouseResolved {
                    kind: "athena".to_string(),
                    container: "AwsDataCatalog".to_string(),
                    namespace: "test_raw".to_string(),
                    extras: serde_json::json!({"region":"eu-west-1","workgroup":"wg","result_s3":"s3://x/"}),
                },
                catalog: config::CatalogResolved {
                    enabled: false,
                    refresh_secs: 60,
                    max_concurrency: 8,
                },
                dbt: config::DbtResolved {
                    enabled: true,
                    profiles_dir: None,
                    target: "athena".to_string(),
                    naming: config::DbtNamingResolved {
                        target_schema: "test".to_string(),
                        silver_suffix: "silver".to_string(),
                        gold_suffix: "warehouse".to_string(),
                    },
                    runner: "host".to_string(),
                    docker_image: None,
                    docker_platform: None,
                    docker_network: None,
                    docker_mount_aws_dir: false,
                },
                vector: config::VectorResolved { enabled: false },
            },
        })
    }

    fn make_ctx(storage: Arc<dyn StorageAdapter>) -> AgentCtx {
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope {
            tenant: "t".to_string(),
            workspace: "w".to_string(),
            project_id: "p".to_string(),
        };
        AgentCtx {
            top_k: 1,
            per_step_timeout_secs: 1,
            max_steps: 1,
            thread_id: None,
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: None,
            agent_name: Some("test".to_string()),
            policy: Arc::new(DefaultPolicy),
            llm: Arc::new(NullModel::new()),
            storage,
            scope,
            keyspace,
            query: None,
            warehouse: Arc::new(react_core::providers::NullWarehouseProvider::default()),
            dbt: None,
            vector: None,
            thread_store: None,
            runtime: Some(minimal_cfg() as Arc<dyn std::any::Any + Send + Sync>),
        }
    }

    #[tokio::test]
    async fn dbt_files_patch_rejects_patch_text_key() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage);
        let tool = DbtFilesTool { datasets: None };

        let err = tool
            .call(
                serde_json::json!({
                    "op": "patch",
                    "patch_text": "diff --git a/models/x.sql b/models/x.sql\n--- /dev/null\n+++ b/models/x.sql\n@@\n+select 1\n"
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.contains("replace_file"));
    }

    #[tokio::test]
    async fn dbt_files_patch_rejects_unified_git_style_patch_key() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage);
        let tool = DbtFilesTool { datasets: None };

        let err = tool
            .call(
                serde_json::json!({
                    "op": "patch",
                    "unified_git_style_patch": "diff --git a/models/x.sql b/models/x.sql\n--- /dev/null\n+++ b/models/x.sql\n@@\n+select 1\n"
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_lowercase().contains("no longer accepts"));
        assert!(err.contains("replace_file"));
    }

    #[tokio::test]
    async fn dbt_files_patch_replace_file_writes_and_returns_canonical_patch() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage.clone());
        let tool = DbtFilesTool { datasets: None };

        let obs = tool
            .call(
                serde_json::json!({
                    "op": "patch",
                    "replace_file": {
                        "path": "models/x.sql",
                        "new_text": "select 1\n"
                    }
                }),
                &ctx,
            )
            .await
            .expect("patch ok");

        assert_eq!(obs.get("ok").and_then(|v| v.as_bool()), Some(true));
        let applied = obs
            .get("applied_patch_text")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(applied.contains("diff --git a/models/x.sql b/models/x.sql"));

        let written = obs
            .get("written_keys")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        assert_eq!(written.len(), 1);
        let key = written[0].as_str().unwrap_or("");
        let bytes = storage.get_bytes(key).await.expect("written");
        let content = String::from_utf8_lossy(&bytes).to_string();
        // Hard-cutover portability: do not inject `schema=` into model configs (dbt_project.yml governs schema).
        assert!(!content.contains("config(schema="));
        assert!(content.contains("alias=\"x\""));
        assert!(content.to_ascii_lowercase().contains("select 1"));
    }

    #[tokio::test]
    async fn dbt_files_patch_rejects_nested_preview_diff_under_replace_file() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage);
        let tool = DbtFilesTool { datasets: None };

        let err = tool
            .call(
                serde_json::json!({
                    "op": "patch",
                    "replace_file": {
                        "path": "models/x.sql",
                        "new_text": "select 1\n",
                        "preview_diff": true
                    }
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_lowercase().contains("contract violation"));
        assert!(err.contains("replace_file.preview_diff"));
        assert!(err.to_lowercase().contains("top-level"));
    }

    #[tokio::test]
    async fn dbt_files_patch_rejects_replace_file_as_string() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let ctx = make_ctx(storage);
        let tool = DbtFilesTool { datasets: None };

        let err = tool
            .call(
                serde_json::json!({
                    "op": "patch",
                    "replace_file": "models/x.sql"
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_lowercase().contains("contract violation"));
        assert!(err.to_lowercase().contains("replace_file"));
    }
}
