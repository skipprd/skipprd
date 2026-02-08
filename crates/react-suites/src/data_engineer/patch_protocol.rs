use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

use react_core::agent::AgentCtx;
use react_core::llm::ChatMessage;
use react_core::llm_observability::{self, PartInput};
use react_core::providers::DatasetCatalogProvider;
use react_core::session::{Observation, ThreadStep};

use crate::data_engineer::project_fs;

fn sha256_hex(s: &str) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(s.as_bytes());
    hex::encode(hasher.finalize())
}

fn split_lines_preserve_trailing_newline_for_prompt(s: &str) -> (Vec<String>, bool) {
    // Keep semantics aligned with project_fs::apply_replace_range.
    let had_trailing_newline = s.ends_with('\n');
    let mut lines: Vec<String> = s.split('\n').map(|x| x.to_string()).collect();
    // `split('\n')` produces a trailing empty segment when the string ends with '\n'.
    if had_trailing_newline {
        if let Some(last) = lines.last() {
            if last.is_empty() {
                lines.pop();
            }
        }
    }
    (lines, had_trailing_newline)
}

fn format_with_line_numbers(s: &str, max_chars: usize) -> (String, bool, usize, bool) {
    let (lines, had_trailing_newline) = split_lines_preserve_trailing_newline_for_prompt(s);
    let line_count = lines.len();
    if max_chars == 0 {
        return (String::new(), false, line_count, had_trailing_newline);
    }

    let mut out = String::new();
    let mut truncated = false;
    for (idx, line) in lines.iter().enumerate() {
        let n = idx + 1;
        // `N|<content>` keeps copy/paste friendly while still being parseable.
        let chunk = format!("{n}|{line}\n");
        if out.len() + chunk.len() > max_chars {
            truncated = true;
            break;
        }
        out.push_str(&chunk);
    }
    if truncated {
        out.push_str("...|[truncated]\n");
    }
    (out, truncated, line_count, had_trailing_newline)
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
    #[serde(default)]
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
///   - replace_range
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

    let base = ctx
        .keyspace
        .dbt_prefix(&ctx.scope)
        .trim_end_matches('/')
        .to_string();
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
    // Prompt facts: line_count is the authoritative upper bound for replace_range end_line.
    // We cap numbered content to avoid doubling prompt size for large files.
    let (existing_content_with_line_numbers, existing_content_with_line_numbers_truncated, existing_line_count, existing_had_trailing_newline) =
        format_with_line_numbers(&existing, 200_000);

    // Always include the raw file content in the prompt payload.
    let user_payload_value = parse_json_from_llm(&user_payload_json)?;
    let initial_user = serde_json::json!({
        "expected_rel_path": expected_rel_path,
        "base_sha256": base_sha256,
        "base_exists": existed,
        "existing_line_count": existing_line_count,
        "existing_had_trailing_newline": existing_had_trailing_newline,
        "existing_content": existing,
        "existing_content_with_line_numbers": existing_content_with_line_numbers,
        "existing_content_with_line_numbers_truncated": existing_content_with_line_numbers_truncated,
        "input": user_payload_value,
        "instruction": "Return ONLY JSON. Choose EXACTLY ONE patch primitive: replace_file | replace_range | replace_list. Prefer replace_file when possible. The patch MUST modify ONLY expected_rel_path. For replace_range/replace_list edits, end_line MUST be <= existing_line_count; if replacing to end-of-file, use end_line = existing_line_count. Do NOT include expected_sha256; the suite enforces drift safety from base_sha256/base_exists."
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
        let resp = ctx.llm.chat(&messages);
        let resp_text = match resp {
            Ok(t) => t,
            Err(e) => {
                // Persist LLM observability even on failure.
                if llm_observability::llm_calls_enabled() {
                    if let (Some(thread_id), Some(store)) =
                        (ctx.thread_id.as_deref(), ctx.thread_store.as_ref())
                    {
                        let call_id = llm_observability::next_call_id(thread_id);
                        let prompt_hash = llm_observability::prompt_hash_for_messages(&messages);
                        let parts = vec![
                            PartInput {
                                name: "system".to_string(),
                                text: messages
                                    .get(0)
                                    .map(|m| m.content.clone())
                                    .unwrap_or_default(),
                            },
                            PartInput {
                                name: "user".to_string(),
                                text: messages
                                    .get(1)
                                    .map(|m| m.content.clone())
                                    .unwrap_or_default(),
                            },
                        ];
                        let built = llm_observability::build_parts_for_thread(thread_id, &parts);
                        let response_hash = llm_observability::sha256_hex_str(&e);
                        let response_text = if llm_observability::llm_response_text_enabled() {
                            Some(llm_observability::redact_common_secrets(&e))
                        } else {
                            None
                        };
                        let phase =
                            format!("patch_protocol:{}:attempt_{}", expected_rel_path, attempt);
                        tracing::debug!(
                            "LLM_CALL thread_id={} call_id={} agent={} phase={} model={} prompt_hash={} response_hash={}",
                            thread_id,
                            call_id,
                            ctx.agent_name.clone().unwrap_or_else(|| "unknown".to_string()),
                            phase,
                            "unknown",
                            prompt_hash,
                            response_hash
                        );
                        for p in built.parts.iter() {
                            let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("-");
                            let hash = p.get("hash").and_then(|v| v.as_str()).unwrap_or("-");
                            let text = p.get("text").and_then(|v| v.as_str()).unwrap_or("");
                            tracing::debug!(
                                "LLM_PART thread_id={} call_id={} name={} hash={} text={}",
                                thread_id,
                                call_id,
                                name,
                                hash,
                                text
                            );
                        }
                        if let Some(txt) = response_text.as_deref() {
                            tracing::debug!(
                                "LLM_RESPONSE thread_id={} call_id={} text={}",
                                thread_id,
                                call_id,
                                txt
                            );
                        }
                        let _ = store
                            .append_step(
                                thread_id,
                                ThreadStep::LlmCall {
                                    call_id,
                                    model: "unknown".to_string(),
                                    phase,
                                    prompt_hash,
                                    parts: built.parts,
                                    part_hashes: built.part_hashes,
                                    response_hash,
                                    response_text,
                                    observation: Observation::fail(vec![
                                        "llm_call_failed".to_string()
                                    ]),
                                    ts: chrono::Utc::now().to_rfc3339(),
                                    agent: ctx
                                        .agent_name
                                        .clone()
                                        .unwrap_or_else(|| "unknown".to_string()),
                                },
                            )
                            .await;
                    }
                }
                return Err(format!("LLM patch authoring call failed: {}", e));
            }
        };

        // Persist observability for successful calls.
        if llm_observability::llm_calls_enabled() {
            if let (Some(thread_id), Some(store)) =
                (ctx.thread_id.as_deref(), ctx.thread_store.as_ref())
            {
                let call_id = llm_observability::next_call_id(thread_id);
                let prompt_hash = llm_observability::prompt_hash_for_messages(&messages);
                let parts = vec![
                    PartInput {
                        name: "system".to_string(),
                        text: messages
                            .get(0)
                            .map(|m| m.content.clone())
                            .unwrap_or_default(),
                    },
                    PartInput {
                        name: "user".to_string(),
                        text: messages
                            .get(1)
                            .map(|m| m.content.clone())
                            .unwrap_or_default(),
                    },
                ];
                let built = llm_observability::build_parts_for_thread(thread_id, &parts);
                let response_hash = llm_observability::sha256_hex_str(&resp_text);
                let response_text = if llm_observability::llm_response_text_enabled() {
                    Some(llm_observability::redact_common_secrets(&resp_text))
                } else {
                    None
                };
                let phase = format!("patch_protocol:{}:attempt_{}", expected_rel_path, attempt);
                tracing::debug!(
                    "LLM_CALL thread_id={} call_id={} agent={} phase={} model={} prompt_hash={} response_hash={}",
                    thread_id,
                    call_id,
                    ctx.agent_name.clone().unwrap_or_else(|| "unknown".to_string()),
                    phase,
                    "unknown",
                    prompt_hash,
                    response_hash
                );
                for p in built.parts.iter() {
                    let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("-");
                    let hash = p.get("hash").and_then(|v| v.as_str()).unwrap_or("-");
                    let text = p.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    tracing::debug!(
                        "LLM_PART thread_id={} call_id={} name={} hash={} text={}",
                        thread_id,
                        call_id,
                        name,
                        hash,
                        text
                    );
                }
                if let Some(txt) = response_text.as_deref() {
                    tracing::debug!(
                        "LLM_RESPONSE thread_id={} call_id={} text={}",
                        thread_id,
                        call_id,
                        txt
                    );
                }
                let _ = store
                    .append_step(
                        thread_id,
                        ThreadStep::LlmCall {
                            call_id,
                            model: "unknown".to_string(),
                            phase,
                            prompt_hash,
                            parts: built.parts,
                            part_hashes: built.part_hashes,
                            response_hash,
                            response_text,
                            observation: Observation::ok(),
                            ts: chrono::Utc::now().to_rfc3339(),
                            agent: ctx
                                .agent_name
                                .clone()
                                .unwrap_or_else(|| "unknown".to_string()),
                        },
                    )
                    .await;
            }
        }
        let parsed = match parse_llm_patch_response(&resp_text) {
            Ok(v) => v,
            Err(e) => {
                last_err = Some(format!("invalid_json: {}", e.trim()));
                // Minimal repair prompt: do not include historical thread transcript; just restate the contract.
                let repair = serde_json::json!({
                    "attempt": attempt,
                    "error": last_err.clone().unwrap_or_else(|| "invalid_json".to_string()),
                    "expected_rel_path": expected_rel_path,
                    "base_sha256": base_sha256,
                    "base_exists": existed,
                    "existing_line_count": existing_line_count,
                    "existing_had_trailing_newline": existing_had_trailing_newline,
                    "existing_content_with_line_numbers": existing_content_with_line_numbers,
                    "existing_content_with_line_numbers_truncated": existing_content_with_line_numbers_truncated,
                    "instruction": "Return ONLY JSON. Choose EXACTLY ONE patch primitive: replace_file | replace_range | replace_list. The patch MUST modify ONLY expected_rel_path. Prefer replace_file when possible. For replace_range/replace_list edits, end_line MUST be <= existing_line_count; if replacing to end-of-file, use end_line = existing_line_count. Do NOT include expected_sha256; the suite enforces drift safety from base_sha256/base_exists.",
                })
                .to_string();
                messages.push(ChatMessage {
                    role: "user".to_string(),
                    content: repair,
                });
                continue;
            }
        };

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
            last_err = Some(
                "LLM response must include exactly one of: replace_file | replace_range | replace_list"
                    .to_string(),
            );
        } else {
            // Build the intended final file contents deterministically from the selected primitive.
            // We avoid unified diff application entirely for structured primitives (too flaky).
            let new_text_res: Result<String, String> = if let Some(rf) =
                parsed.replace_file.as_ref()
            {
                if let Some(p) = rf.path.as_ref() {
                    let rel = project_fs::normalize_rel_path(p)?;
                    if rel != expected_rel_path {
                        return Err(format!(
                            "replace_file.path '{}' did not match expected_rel_path '{}'",
                            rel, expected_rel_path
                        ));
                    }
                }
                Ok(rf.new_text.clone())
            } else if let Some(rr) = parsed.replace_range.as_ref() {
                if let Some(p) = rr.path.as_ref() {
                    let rel = project_fs::normalize_rel_path(p)?;
                    if rel != expected_rel_path {
                        return Err(format!(
                            "replace_range.path '{}' did not match expected_rel_path '{}'",
                            rel, expected_rel_path
                        ));
                    }
                }
                project_fs::apply_replace_range(&existing, rr.start_line, rr.end_line, &rr.new_text)
            } else if let Some(rl) = parsed.replace_list.as_ref() {
                if let Some(p) = rl.path.as_ref() {
                    let rel = project_fs::normalize_rel_path(p)?;
                    if rel != expected_rel_path {
                        return Err(format!(
                            "replace_list.path '{}' did not match expected_rel_path '{}'",
                            rel, expected_rel_path
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
                        if existed { Some(base_sha256.as_str()) } else { None },
                        Some(existed),
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
        let err = last_err
            .clone()
            .unwrap_or_else(|| "unknown patch error".to_string());
        let repair = serde_json::json!({
            "attempt": attempt,
            "error": err,
            "expected_rel_path": expected_rel_path,
            "base_sha256": base_sha256,
            "base_exists": existed,
            "existing_line_count": existing_line_count,
            "existing_had_trailing_newline": existing_had_trailing_newline,
            "existing_content": existing,
            "existing_content_with_line_numbers": existing_content_with_line_numbers,
            "existing_content_with_line_numbers_truncated": existing_content_with_line_numbers_truncated,
            "previous_response": parsed,
            "instruction": "Return ONLY corrected JSON. Choose EXACTLY ONE patch primitive: replace_file | replace_range | replace_list. The patch MUST modify ONLY expected_rel_path. Prefer replace_file when possible. For replace_range/replace_list edits, end_line MUST be <= existing_line_count; if replacing to end-of-file, use end_line = existing_line_count. Do NOT include expected_sha256; the suite enforces drift safety from base_sha256/base_exists."
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
