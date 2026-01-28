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
    let vals = extract_all_json_values(s, 8);
    for vtxt in vals.iter().rev() {
        if let Ok(v) = serde_json::from_str::<Value>(vtxt) {
            if v.is_object() {
                return Ok(v);
            }
        }
    }
    Err("LLM response did not contain valid JSON object".to_string())
}

fn extract_all_json_values(s: &str, max: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if max == 0 {
        return out;
    }
    let mut i = 0usize;
    while i < s.len() && out.len() < max {
        let mut start: Option<usize> = None;
        for (off, ch) in s[i..].char_indices() {
            if ch == '{' || ch == '[' {
                start = Some(i + off);
                break;
            }
        }
        let Some(st) = start else { break };

        let mut stack: Vec<char> = Vec::new();
        let mut in_str = false;
        let mut esc = false;
        let mut end: Option<usize> = None;
        for (pos, ch) in s[st..].char_indices() {
            let abs = st + pos;

            if stack.is_empty() {
                stack.push(ch);
                continue;
            }
            if in_str {
                if esc {
                    esc = false;
                    continue;
                }
                if ch == '\\' {
                    esc = true;
                    continue;
                }
                if ch == '"' {
                    in_str = false;
                }
                continue;
            }

            match ch {
                '"' => in_str = true,
                '{' | '[' => stack.push(ch),
                '}' => {
                    if matches!(stack.pop(), Some('{')) && stack.is_empty() {
                        end = Some(abs);
                        break;
                    }
                }
                ']' => {
                    if matches!(stack.pop(), Some('[')) && stack.is_empty() {
                        end = Some(abs);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(en) = end else { break };
        out.push(s[st..=en].to_string());
        i = en + 1;
    }
    out
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ReplaceFile {
    #[serde(default)]
    path: Option<String>,
    new_text: String,
    #[serde(default)]
    expected_sha256: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ReplaceRange {
    #[serde(default)]
    path: Option<String>,
    start_line: usize,
    end_line: usize,
    new_text: String,
    #[serde(default)]
    expected_sha256: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ReplaceListEdit {
    start_line: usize,
    end_line: usize,
    new_text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ReplaceList {
    #[serde(default)]
    path: Option<String>,
    edits: Vec<ReplaceListEdit>,
    #[serde(default)]
    expected_sha256: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct LlmPatchResponse {
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
    let _existed = existing_opt.is_some();
    let existing = existing_opt.unwrap_or_default();
    let base_sha256 = sha256_hex(&existing);

    // Always include the raw file content in the prompt payload.
    let user_payload_value = parse_json_from_llm(&user_payload_json)?;
    let initial_user = serde_json::json!({
        "expected_rel_path": expected_rel_path,
        "base_sha256": base_sha256,
        "existing_content": existing,
        "input": user_payload_value,
        "instruction": "Return ONLY JSON. Choose EXACTLY ONE patch primitive: replace_file | replace_range | replace_list. Prefer replace_file when possible. The patch MUST modify ONLY expected_rel_path."
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

        let mut provided = 0usize;
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
            last_err = Some("LLM response must include exactly one of: replace_file | replace_range | replace_list".to_string());
        } else {
            // Build the intended final file contents deterministically from the selected primitive.
            // We avoid unified diff application entirely for structured primitives (too flaky).
            let new_text_res: Result<String, String> = if let Some(rf) = parsed.replace_file.as_ref() {
                if let Some(p) = rf.path.as_ref() {
                    let rel = project_fs::normalize_rel_path(p)?;
                    if rel != expected_rel_path {
                        return Err(format!("replace_file.path '{}' did not match expected_rel_path '{}'", rel, expected_rel_path));
                    }
                }
                // Require expected_sha256 on repair attempts to prevent drift.
                if attempt > 1 && rf.expected_sha256.as_deref().unwrap_or("") != base_sha256 {
                    return Err(format!(
                        "expected_sha256 mismatch for {}: expected {}, got {}",
                        expected_rel_path,
                        rf.expected_sha256.as_deref().unwrap_or("(missing)"),
                        base_sha256
                    ));
                }
                Ok(rf.new_text.clone())
            } else if let Some(rr) = parsed.replace_range.as_ref() {
                if let Some(p) = rr.path.as_ref() {
                    let rel = project_fs::normalize_rel_path(p)?;
                    if rel != expected_rel_path {
                        return Err(format!("replace_range.path '{}' did not match expected_rel_path '{}'", rel, expected_rel_path));
                    }
                }
                if attempt > 1 && rr.expected_sha256.as_deref().unwrap_or("") != base_sha256 {
                    return Err(format!(
                        "expected_sha256 mismatch for {}: expected {}, got {}",
                        expected_rel_path,
                        rr.expected_sha256.as_deref().unwrap_or("(missing)"),
                        base_sha256
                    ));
                }
                project_fs::apply_replace_range(&existing, rr.start_line, rr.end_line, &rr.new_text)
            } else if let Some(rl) = parsed.replace_list.as_ref() {
                if let Some(p) = rl.path.as_ref() {
                    let rel = project_fs::normalize_rel_path(p)?;
                    if rel != expected_rel_path {
                        return Err(format!("replace_list.path '{}' did not match expected_rel_path '{}'", rel, expected_rel_path));
                    }
                }
                if attempt > 1 && rl.expected_sha256.as_deref().unwrap_or("") != base_sha256 {
                    return Err(format!(
                        "expected_sha256 mismatch for {}: expected {}, got {}",
                        expected_rel_path,
                        rl.expected_sha256.as_deref().unwrap_or("(missing)"),
                        base_sha256
                    ));
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
                project_fs::apply_replace_list(&existing, &edits)
            } else {
                Err("invalid patch response".to_string())
            };

            match new_text_res {
                Ok(new_text) => {
                    match project_fs::apply_patch(
                        ctx,
                        datasets,
                        expected_rel_path,
                        &new_text,
                        Some(base_sha256.as_str()),
                        project_fs::PatchApplyKind::FullOverwrite,
                    )
                    .await
                    {
                        Ok(outcome) => return Ok((outcome, parsed.notes)),
                        Err(e) => last_err = Some(e),
                    }
                }
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
            "instruction": "Return ONLY corrected JSON. Choose EXACTLY ONE patch primitive: replace_file | replace_range | replace_list. The patch MUST modify ONLY expected_rel_path. Prefer replace_file when possible. Include expected_sha256 matching base_sha256 to avoid drift."
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

