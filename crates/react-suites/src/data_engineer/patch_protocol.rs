use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

use react_core::agent::AgentCtx;
use react_core::llm::ChatMessage;
use react_core::providers::DatasetCatalogProvider;

use crate::data_engineer::project_fs;

fn sha256_hex(s: &str) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(s.as_bytes());
    hex::encode(hasher.finalize())
}

fn parse_json_from_llm(text: &str) -> Result<Value, String> {
    // The prompt instructs JSON-only, but be resilient to accidental wrappers.
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        return Ok(v);
    }
    let s = text.trim();
    let start = s
        .find('{')
        .ok_or_else(|| "LLM response did not contain JSON object".to_string())?;
    let end = s
        .rfind('}')
        .ok_or_else(|| "LLM response did not contain JSON object".to_string())?;
    if end <= start {
        return Err("LLM response JSON object bounds invalid".to_string());
    }
    serde_json::from_str::<Value>(&s[start..=end]).map_err(|e| e.to_string())
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct ReplaceFile {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    new_text: String,
    #[serde(default)]
    expected_sha256: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct ReplaceRange {
    #[serde(default)]
    path: Option<String>,
    start_line: usize,
    end_line: usize,
    #[serde(default)]
    new_text: String,
    #[serde(default)]
    expected_sha256: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct ReplaceListEdit {
    start_line: usize,
    end_line: usize,
    #[serde(default)]
    new_text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct ReplaceList {
    #[serde(default)]
    path: Option<String>,
    edits: Vec<ReplaceListEdit>,
    #[serde(default)]
    expected_sha256: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct LlmPatchResponse {
    /// Preferred: git-style unified diff (single file or bundle).
    #[serde(default)]
    unified_git_style_patch: String,
    /// Preferred structured primitives: the suite/tool will canonicalize into a git-style patch deterministically.
    #[serde(default)]
    replace_file: Option<ReplaceFile>,
    #[serde(default)]
    replace_range: Option<ReplaceRange>,
    #[serde(default)]
    replace_list: Option<ReplaceList>,
    #[serde(default)]
    notes: Vec<String>,
}

fn parse_llm_patch_response(text: &str) -> Result<LlmPatchResponse, String> {
    let v = parse_json_from_llm(text)?;
    serde_json::from_value(v).map_err(|e| format!("failed to parse patch response JSON: {}", e))
}

/// Ask the LLM for a patch for one expected file, apply it in-memory, and retry on patch errors.
///
/// - The LLM must return JSON and choose EXACTLY ONE patch primitive:
///   - unified_git_style_patch (git-style unified diff), OR
///   - replace_file, OR
///   - replace_range, OR
///   - replace_list
/// - The patch must target exactly `expected_rel_path` (no other files).
pub async fn llm_patch_loop_single_file(
    ctx: &AgentCtx,
    datasets: Option<&Arc<dyn DatasetCatalogProvider>>,
    sys_prompt: String,
    user_payload_json: String,
    expected_rel_path: &str,
    max_iters: usize,
) -> Result<(project_fs::PatchOutcome, Vec<String>), String> {
    let max_iters = max_iters.max(1).min(6);

    let base = ctx.keyspace.dbt_prefix(&ctx.scope).trim_end_matches('/').to_string();
    let key = format!("{}/{}", base, expected_rel_path);
    let existing_opt = ctx
        .storage
        .get_bytes(&key)
        .await
        .ok()
        .map(|b| String::from_utf8_lossy(&b).to_string());
    let existed = existing_opt.is_some();
    let existing = existing_opt.unwrap_or_default();
    let base_sha256 = sha256_hex(&existing);

    // Always include the raw file content in the prompt payload.
    let user_payload_value = parse_json_from_llm(&user_payload_json)?;
    let initial_user = serde_json::json!({
        "expected_rel_path": expected_rel_path,
        "base_sha256": base_sha256,
        "existing_content": existing,
        "input": user_payload_value,
        "instruction": "Return ONLY JSON. Choose EXACTLY ONE patch primitive: unified_git_style_patch | replace_file | replace_range | replace_list. Prefer replace_* primitives when possible. The patch MUST modify ONLY expected_rel_path."
    })
    .to_string();

    let mut messages: Vec<ChatMessage> = vec![
        ChatMessage {
            role: "system".to_string(),
            content: sys_prompt,
        },
        ChatMessage {
            role: "user".to_string(),
            content: initial_user.clone(),
        },
    ];

    let mut last_err: Option<String> = None;
    for attempt in 1..=max_iters {
        let resp_text = ctx
            .llm
            .chat(&messages)
            .map_err(|e| format!("LLM patch authoring call failed: {}", e))?;
        let parsed = parse_llm_patch_response(&resp_text)?;
        let unified = parsed.unified_git_style_patch.trim().to_string();

        let mut provided = 0usize;
        if !unified.is_empty() {
            provided += 1;
        }
        if parsed.replace_file.is_some() {
            provided += 1;
        }
        if parsed.replace_range.is_some() {
            provided += 1;
        }
        if parsed.replace_list.is_some() {
            provided += 1;
        }
        if provided != 1 {
            last_err = Some("LLM response must include exactly one of: unified_git_style_patch | replace_file | replace_range | replace_list".to_string());
        } else {
            // Build a canonical git-style patch text deterministically from the selected primitive.
            let patch_text: Result<String, String> = if !unified.is_empty() {
                Ok(unified.clone())
            } else if let Some(rf) = parsed.replace_file.as_ref() {
                if let Some(p) = rf.path.as_ref() {
                    let rel = project_fs::normalize_rel_path(p)?;
                    if rel != expected_rel_path {
                        return Err(format!("replace_file.path '{}' did not match expected_rel_path '{}'", rel, expected_rel_path));
                    }
                }
                if let Some(expected) = rf.expected_sha256.as_deref() {
                    if expected != base_sha256 {
                        return Err(format!(
                            "expected_sha256 mismatch for {}: expected {}, got {}",
                            expected_rel_path, expected, base_sha256
                        ));
                    }
                }
                project_fs::create_git_patch_text(&existing, &rf.new_text, expected_rel_path, existed)
            } else if let Some(rr) = parsed.replace_range.as_ref() {
                if let Some(p) = rr.path.as_ref() {
                    let rel = project_fs::normalize_rel_path(p)?;
                    if rel != expected_rel_path {
                        return Err(format!("replace_range.path '{}' did not match expected_rel_path '{}'", rel, expected_rel_path));
                    }
                }
                if let Some(expected) = rr.expected_sha256.as_deref() {
                    if expected != base_sha256 {
                        return Err(format!(
                            "expected_sha256 mismatch for {}: expected {}, got {}",
                            expected_rel_path, expected, base_sha256
                        ));
                    }
                }
                let new_text = project_fs::apply_replace_range(&existing, rr.start_line, rr.end_line, &rr.new_text)?;
                project_fs::create_git_patch_text(&existing, &new_text, expected_rel_path, true)
            } else if let Some(rl) = parsed.replace_list.as_ref() {
                if let Some(p) = rl.path.as_ref() {
                    let rel = project_fs::normalize_rel_path(p)?;
                    if rel != expected_rel_path {
                        return Err(format!("replace_list.path '{}' did not match expected_rel_path '{}'", rel, expected_rel_path));
                    }
                }
                if let Some(expected) = rl.expected_sha256.as_deref() {
                    if expected != base_sha256 {
                        return Err(format!(
                            "expected_sha256 mismatch for {}: expected {}, got {}",
                            expected_rel_path, expected, base_sha256
                        ));
                    }
                }
                let edits: Vec<project_fs::ReplaceListEdit> = rl
                    .edits
                    .iter()
                    .map(|e| project_fs::ReplaceListEdit {
                        start_line: e.start_line,
                        end_line: e.end_line,
                        new_text: e.new_text.clone(),
                    })
                    .collect();
                let new_text = project_fs::apply_replace_list(&existing, &edits)?;
                project_fs::create_git_patch_text(&existing, &new_text, expected_rel_path, true)
            } else {
                Err("invalid patch response".to_string())
            };

            match patch_text {
                Ok(patch_text) => match project_fs::apply_patch_bundle(ctx, datasets, &patch_text).await {
                    Ok(outcomes) => {
                        if outcomes.len() != 1 {
                            last_err = Some(format!(
                                "patch must target exactly one file (expected '{}'), but patch touched {} files",
                                expected_rel_path,
                                outcomes.len()
                            ));
                        } else if outcomes[0].rel_path != expected_rel_path {
                            last_err = Some(format!(
                                "patch targeted '{}' but expected '{}'",
                                outcomes[0].rel_path, expected_rel_path
                            ));
                        } else {
                            return Ok((outcomes.into_iter().next().unwrap(), parsed.notes));
                        }
                    }
                    Err(e) => last_err = Some(e),
                },
                Err(e) => last_err = Some(e),
            }
        }

        // Repair prompt: feed back the failing patch + error + expected file + current content.
        let err = last_err.clone().unwrap_or_else(|| "unknown patch error".to_string());
        let repair = serde_json::json!({
            "attempt": attempt,
            "error": err,
            "expected_rel_path": expected_rel_path,
            "base_sha256": base_sha256,
            "existing_content": existing,
            "previous_response": parsed,
            "instruction": "Return ONLY corrected JSON. Choose EXACTLY ONE patch primitive: unified_git_style_patch | replace_file | replace_range | replace_list. The patch MUST modify ONLY expected_rel_path. Prefer replace_* primitives when possible. If you provide unified_git_style_patch, it MUST be a git-style unified diff; if creating a new file it MUST use --- /dev/null and +++ b/<path>."
        })
        .to_string();
        messages.push(ChatMessage {
            role: "user".to_string(),
            content: repair,
        });
    }

    Err(format!(
        "LLM patch repair failed after {} attempt(s): {}",
        max_iters,
        last_err.unwrap_or_else(|| "unknown error".to_string())
    ))
}

