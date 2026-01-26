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
        serde_json::from_value::<Vec<T>>(v.clone()).map_err(|e| format!("{} parse error: {}", key, e))
    } else {
        let one = serde_json::from_value::<T>(v.clone()).map_err(|e| format!("{} parse error: {}", key, e))?;
        Ok(vec![one])
    }
}

#[derive(Clone, Debug, Deserialize)]
struct ReplaceFileArgs {
    path: String,
    #[serde(default)]
    new_text: String,
    #[serde(default)]
    expected_sha256: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ReplaceRangeArgs {
    path: String,
    start_line: usize,
    end_line: usize,
    #[serde(default)]
    new_text: String,
    #[serde(default)]
    expected_sha256: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ReplaceListEditArgs {
    start_line: usize,
    end_line: usize,
    #[serde(default)]
    new_text: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ReplaceListArgs {
    path: String,
    edits: Vec<ReplaceListEditArgs>,
    #[serde(default)]
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
                let preview = args.get("preview_diff").and_then(|x| x.as_bool()).unwrap_or(false);

                // Optional single-file guard: if provided, ensure the patch bundle targets exactly this rel path.
                let want_rel_path = args
                    .get("path")
                    .and_then(|x| x.as_str())
                    .map(project_fs::normalize_rel_path)
                    .transpose()?;

                let patch_text_opt = args.get("patch_text").and_then(|x| x.as_str()).map(|s| s.to_string());
                let replace_file_ops: Vec<ReplaceFileArgs> = parse_one_or_many(&args, "replace_file")?;
                let replace_range_ops: Vec<ReplaceRangeArgs> = parse_one_or_many(&args, "replace_range")?;
                let replace_list_ops: Vec<ReplaceListArgs> = parse_one_or_many(&args, "replace_list")?;

                let mut provided = 0usize;
                if patch_text_opt.as_ref().map(|s| !s.trim().is_empty()).unwrap_or(false) {
                    provided += 1;
                }
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
                        "dbt_files op=patch requires exactly one of: patch_text | replace_file | replace_range | replace_list"
                            .to_string(),
                    );
                }

                // Build a patch bundle to apply, regardless of primitive.
                let patch_text: String = if let Some(patch_text) = patch_text_opt {
                    // Guardrail: if someone accidentally passes full file contents as patch_text, fail loudly with guidance.
                    let looks_like_patch = patch_text.lines().any(|l| l.starts_with("diff --git "))
                        || (patch_text.lines().any(|l| l.starts_with("--- "))
                            && patch_text.lines().any(|l| l.starts_with("+++ ")));
                    if !looks_like_patch {
                        let hint = if let Some(p) = want_rel_path.as_ref() {
                            format!(
                                "patch_text must be a git-style unified diff (not raw file content). Example for {p}:\n\
\n\
diff --git a/{p} b/{p}\n\
new file mode 100644\n\
--- /dev/null\n\
+++ b/{p}\n\
@@\n\
+<file contents here>\n"
                            )
                        } else {
                            "patch_text must be a git-style unified diff (not raw file content). Include headers like `diff --git a/<path> b/<path>` and `+++ b/<path>`.".to_string()
                        };
                        return Err(hint);
                    }
                    patch_text
                } else if !replace_file_ops.is_empty() {
                    let mut seen: HashSet<String> = HashSet::new();
                    let mut per_file: Vec<(String, String)> = Vec::new();
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
                        let existed = existing_opt.is_some();
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
                        let p = project_fs::create_git_patch_text(&old, &rf.new_text, &rel, existed)?;
                        per_file.push((rel, p));
                    }
                    per_file.sort_by(|a, b| a.0.cmp(&b.0));
                    per_file.into_iter().map(|(_, p)| p.trim_end().to_string()).collect::<Vec<String>>().join("\n\n")
                } else if !replace_range_ops.is_empty() {
                    let mut seen: HashSet<String> = HashSet::new();
                    let mut per_file: Vec<(String, String)> = Vec::new();
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
                        let new_text = project_fs::apply_replace_range(&existing, rr.start_line, rr.end_line, &rr.new_text)?;
                        let p = project_fs::create_git_patch_text(&existing, &new_text, &rel, true)?;
                        per_file.push((rel, p));
                    }
                    per_file.sort_by(|a, b| a.0.cmp(&b.0));
                    per_file.into_iter().map(|(_, p)| p.trim_end().to_string()).collect::<Vec<String>>().join("\n\n")
                } else if !replace_list_ops.is_empty() {
                    let mut seen: HashSet<String> = HashSet::new();
                    let mut per_file: Vec<(String, String)> = Vec::new();
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
                        let p = project_fs::create_git_patch_text(&existing, &new_text, &rel, true)?;
                        per_file.push((rel, p));
                    }
                    per_file.sort_by(|a, b| a.0.cmp(&b.0));
                    per_file.into_iter().map(|(_, p)| p.trim_end().to_string()).collect::<Vec<String>>().join("\n\n")
                } else {
                    return Err("invalid patch request".to_string());
                };

                let mut outcomes = project_fs::apply_patch_bundle(ctx, self.datasets.as_ref(), &patch_text).await?;
                if outcomes.is_empty() {
                    return Err("patch_text produced no file changes".to_string());
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
            _ => Err("unsupported op; use 'list', 'get', 'get_json', 'manifest_find', or 'patch'".to_string()),
        }
    }
}
