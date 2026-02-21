use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

use react_core::agent::AgentCtx;
use react_core::llm::ChatMessage;
use react_core::llm::LlmCallOptions;
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

fn excerpt_for_error(s: &str, max_chars: usize) -> String {
    let t = s.trim();
    if max_chars == 0 || t.is_empty() {
        return String::new();
    }
    // `max_chars` is a byte budget; ensure we truncate on a UTF-8 boundary.
    if t.len() <= max_chars {
        return t.to_string();
    }
    let mut end = 0usize;
    for (i, ch) in t.char_indices() {
        if i >= max_chars {
            break;
        }
        end = i + ch.len_utf8();
    }
    if end == 0 {
        return String::new();
    }
    let mut out = t[..end].to_string();
    out.push_str("…[truncated]");
    out
}

pub fn default_patch_loop_max_output_tokens() -> u32 {
    std::env::var("REACT_PATCH_LOOP_MAX_OUTPUT_TOKENS")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|v| *v >= 512)
        .unwrap_or(3200)
}

fn has_required_analyst_notes(notes: &[String]) -> Result<(), String> {
    // Contract: require these sections (case-insensitive) somewhere in notes as distinct entries.
    // We enforce this only when the caller opts-in via a sys_prompt sentinel.
    let mut need: std::collections::BTreeSet<&'static str> = std::collections::BTreeSet::new();
    need.insert("business question");
    need.insert("entity definition");
    need.insert("grain");
    need.insert("time axis");
    need.insert("metric definitions");
    need.insert("assumptions");

    for n in notes.iter() {
        let t = n.trim().to_ascii_lowercase();
        if t.starts_with("business question") {
            need.remove("business question");
        } else if t.starts_with("entity definition") {
            need.remove("entity definition");
        } else if t.starts_with("grain") {
            need.remove("grain");
        } else if t.starts_with("time axis") {
            need.remove("time axis");
        } else if t.starts_with("metric definitions") || t.starts_with("metrics") {
            need.remove("metric definitions");
        } else if t.starts_with("assumptions") {
            need.remove("assumptions");
        }
    }
    if need.is_empty() {
        return Ok(());
    }
    Err(format!(
        "missing required analyst-mindset notes sections: {}\n\
\n\
Include each as a separate notes entry prefixed like:\n\
- Business question: ...\n\
- Entity definition: ...\n\
- Grain: ...\n\
- Time axis: ...\n\
- Metric definitions: ...\n\
- Assumptions & gaps: ...",
        need.into_iter().collect::<Vec<_>>().join(", ")
    ))
}

fn split_lines_preserve_trailing_newline_for_prompt(s: &str) -> (Vec<String>, bool) {
    // Keep prompt line counting deterministic.
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

fn looks_like_patch_object(v: &Value) -> bool {
    let Some(m) = v.as_object() else { return false };
    m.contains_key("patch_text")
}

fn parse_patch_json_from_llm(text: &str) -> Result<Value, String> {
    // Like parse_json_from_llm, but prefer the JSON object that actually contains patch primitives.
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        if v.is_object() && looks_like_patch_object(&v) {
            return Ok(v);
        }
    }
    let s = text.trim();
    let vals = extract_all_json_values(s, 16);
    let mut last_obj: Option<Value> = None;
    for vtxt in vals.iter().rev() {
        if let Ok(v) = serde_json::from_str::<Value>(vtxt) {
            if v.is_object() {
                if looks_like_patch_object(&v) {
                    return Ok(v);
                }
                last_obj = Some(v);
            }
        }
    }
    if let Some(v) = last_obj {
        return Ok(v);
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
struct LlmPatchResponse {
    #[serde(default)]
    path: Option<String>,
    patch_text: String,
    #[serde(default)]
    notes: Vec<String>,
}

fn parse_llm_patch_response(text: &str, expected_rel_path: &str) -> Result<LlmPatchResponse, String> {
    let v = parse_patch_json_from_llm(text)?;
    let parsed: LlmPatchResponse =
        serde_json::from_value(v).map_err(|e| format!("failed to parse patch response JSON: {}", e))?;
    if parsed.patch_text.trim().is_empty() {
        return Err("patch_text is empty".to_string());
    }
    if let Some(p) = parsed.path.as_deref() {
        let rel = project_fs::normalize_rel_path(p)?;
        if rel != expected_rel_path {
            return Err(format!(
                "path '{}' did not match expected_rel_path '{}'",
                rel, expected_rel_path
            ));
        }
    }
    Ok(parsed)
}

/// Ask the LLM for a patch for one expected file, apply it in-memory, and retry on patch errors.
///
/// - The LLM must return JSON containing `patch_text`.
/// - The patch must target exactly `expected_rel_path` (no other files).
pub async fn llm_patch_loop_single_file(
    ctx: &AgentCtx,
    datasets: Option<&Arc<dyn DatasetCatalogProvider>>,
    sys_prompt: String,
    user_payload_json: String,
    expected_rel_path: &str,
    max_iters: usize,
    llm_options: Option<LlmCallOptions>,
) -> Result<(project_fs::PatchOutcome, Vec<String>), String> {
    let max_iters = max_iters.max(1).min(10);
    let enforce_analyst_notes_contract = sys_prompt.contains("ANALYST_NOTES_CONTRACT_V1");

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
    // Prompt facts used to help the LLM produce stable hunks.
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
        "instruction": "Return a single JSON object with patch_text (unified diff). Prefer Cursor-style hunks-only patch_text starting with '@@' and omitting ---/+++ headers. The patch MUST modify ONLY expected_rel_path. If prior attempts produced no-op patches, rewrite the entire file using a single large hunk."
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

    let mut call_opts = llm_options.unwrap_or_else(|| {
        react_core::llm::LlmCallOptions::new(
            "data_engineer.patch_protocol.llm_patch_loop",
            react_core::llm::LlmExpectedFormat::JsonObject,
        )
    });
    call_opts.expected_format = react_core::llm::LlmExpectedFormat::JsonObject;
    if call_opts.max_output_tokens.is_none() {
        call_opts.max_output_tokens = Some(default_patch_loop_max_output_tokens());
    }

    let mut last_err: Option<String> = None;
    let mut bumped_output_budget = false;
    let mut no_op_failures = 0usize;
    let no_op_breaker_threshold = 2usize;
    for attempt in 1..=max_iters {
        let resp = ctx.llm.chat(&messages, &call_opts);
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
                if !bumped_output_budget && e.contains("max_output_tokens") {
                    if let Some(current) = call_opts.max_output_tokens {
                        let bumped = current.saturating_mul(2).min(8000);
                        if bumped > current {
                            bumped_output_budget = true;
                            call_opts.max_output_tokens = Some(bumped);
                            last_err = Some(format!(
                                "patch authoring truncated at max_output_tokens={}; retrying once with {}",
                                current, bumped
                            ));
                            continue;
                        }
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
        let parsed = match parse_llm_patch_response(&resp_text, expected_rel_path) {
            Ok(v) => v,
            Err(e) => {
                // FAIL FAST: structural/contract violation. Retrying wastes tokens and can lead to batch_locked.
                return Err(format!(
                    "invalid_json: {}\n\nExpected patch contract:\n{}\n\nResponse excerpt:\n{}",
                    e.trim(),
                    crate::prompts::patch_contract::llm_patch_response_contract(),
                    excerpt_for_error(&resp_text, 2000)
                ));
            }
        };

        if no_op_failures >= no_op_breaker_threshold {
            let err = format!(
                "no-op patch breaker: {} consecutive no-op patches for '{}'. Return patch_text that rewrites the entire file (single large hunk).",
                no_op_failures, expected_rel_path
            );
            last_err = Some(err.clone());
            let repair = serde_json::json!({
                "attempt": attempt,
                "error": err,
                "consecutive_noop_patches": no_op_failures,
                "expected_rel_path": expected_rel_path,
                "base_sha256": base_sha256,
                "base_exists": existed,
                "existing_line_count": existing_line_count,
                "existing_had_trailing_newline": existing_had_trailing_newline,
                "existing_content": existing,
                "existing_content_with_line_numbers": existing_content_with_line_numbers,
                "existing_content_with_line_numbers_truncated": existing_content_with_line_numbers_truncated,
                "previous_response": parsed,
                "instruction": "Return ONLY corrected JSON with patch_text (unified diff). Prefer Cursor-style hunks-only patch_text starting with '@@'. The patch MUST modify ONLY expected_rel_path. Rewrite the entire file using one large hunk if needed."
            })
            .to_string();
            messages.push(ChatMessage {
                role: "user".to_string(),
                content: repair,
            });
            continue;
        }

        // Optional content contract gates (opt-in via sys_prompt sentinel).
        let gate_err: Option<String> = if enforce_analyst_notes_contract {
            has_required_analyst_notes(&parsed.notes).err()
        } else {
            None
        };
        if let Some(e) = gate_err {
            // Skip patch application; let the existing repair loop prompt the model.
            let err = e;
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
                "instruction": "Return ONLY corrected JSON with notes + patch_text."
            })
            .to_string();
            messages.push(ChatMessage {
                role: "user".to_string(),
                content: repair,
            });
            continue;
        }

        // Apply patch_text (unified diff). If patch_text is hunks-only, synthesize a minimal git header.
        let mut patch_text = parsed.patch_text.clone();
        let has_headers = patch_text
            .lines()
            .any(|l| l.trim_start().starts_with("--- "));
        if !has_headers {
            if existed {
                patch_text = format!(
                    "diff --git a/{0} b/{0}\n--- a/{0}\n+++ b/{0}\n{1}",
                    expected_rel_path,
                    patch_text.trim_start()
                );
            } else {
                patch_text = format!(
                    "diff --git a/{0} b/{0}\n--- /dev/null\n+++ b/{0}\n{1}",
                    expected_rel_path,
                    patch_text.trim_start()
                );
            }
        }

        match project_fs::apply_patch(
            ctx,
            datasets,
            expected_rel_path,
            &patch_text,
            Some(base_sha256.as_str()),
            Some(existed),
            project_fs::PatchApplyKind::UnifiedDiff,
        )
        .await
        {
            Ok(outcome) => {
                if outcome.base_sha256 == outcome.new_sha256 {
                    no_op_failures = no_op_failures.saturating_add(1);
                    last_err = Some(format!(
                        "patch produced no file changes for '{}' (consecutive_noops={}).",
                        expected_rel_path, no_op_failures
                    ));
                } else {
                    return Ok((outcome, parsed.notes));
                }
            }
            Err(e) => last_err = Some(e),
        }

        // Repair prompt: feed back the failing patch + error + expected file + current content.
        let err = last_err
            .clone()
            .unwrap_or_else(|| "unknown patch error".to_string());
        let repair = serde_json::json!({
            "attempt": attempt,
            "error": err,
            "consecutive_noop_patches": no_op_failures,
            "expected_rel_path": expected_rel_path,
            "base_sha256": base_sha256,
            "base_exists": existed,
            "existing_line_count": existing_line_count,
            "existing_had_trailing_newline": existing_had_trailing_newline,
            "existing_content": existing,
            "existing_content_with_line_numbers": existing_content_with_line_numbers,
            "existing_content_with_line_numbers_truncated": existing_content_with_line_numbers_truncated,
            "previous_response": parsed,
            "instruction": "Return ONLY corrected JSON with patch_text (unified diff). Prefer Cursor-style hunks-only patch_text starting with '@@'. The patch MUST modify ONLY expected_rel_path. If repeated no-ops occur, rewrite the entire file using one large hunk."
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

#[cfg(test)]
mod tests {
    use super::*;
    use react_core::keyspace::{DefaultKeyspace, Keyspace};
    use react_core::llm::LargeLanguageModel;
    use react_core::scope::RequestScope;
    use react_core::storage::{InMemoryStorageAdapter, StorageAdapter};
    use std::sync::{Arc, Mutex};

    #[test]
    fn parse_llm_patch_response_accepts_patch_text() {
        let txt = r#"{
          "path": "models/schema.yml",
          "patch_text": "@@\n- a\n+ b\n",
          "notes": ["ok"]
        }"#;
        let parsed = parse_llm_patch_response(txt, "models/schema.yml").expect("parse ok");
        assert_eq!(parsed.path.as_deref(), Some("models/schema.yml"));
        assert!(parsed.patch_text.contains("@@"));
        assert_eq!(parsed.notes, vec!["ok".to_string()]);
    }

    #[test]
    fn parse_llm_patch_response_rejects_wrong_path() {
        let txt = r#"{
          "path": "models/other.yml",
          "patch_text": "@@\n- a\n+ b\n"
        }"#;
        let err = parse_llm_patch_response(txt, "models/schema.yml").unwrap_err();
        assert!(err.contains("expected_rel_path"));
    }

    fn minimal_cfg() -> Arc<crate::config::ReactResolvedConfig> {
        Arc::new(crate::config::ReactResolvedConfig {
            server: crate::config::ServerResolved { port: 1 },
            storage: crate::config::StorageResolved {
                bucket: "b".to_string(),
            },
            scope: RequestScope {
                tenant: "t".to_string(),
                workspace: "w".to_string(),
                project_id: "p".to_string(),
            },
            llm: crate::config::LlmResolved::default(),
            providers: crate::config::ProvidersResolved {
                warehouse: crate::config::WarehouseResolved {
                    kind: "athena".to_string(),
                    container: "AwsDataCatalog".to_string(),
                    namespace: "test_raw".to_string(),
                    extras: serde_json::json!({"region":"eu-west-1","workgroup":"wg","result_s3":"s3://x/"}),
                },
                catalog: crate::config::CatalogResolved {
                    enabled: false,
                    refresh_secs: 60,
                    max_concurrency: 8,
                },
                dbt: crate::config::DbtResolved {
                    enabled: true,
                    profiles_dir: None,
                    target: "athena".to_string(),
                    naming: crate::config::DbtNamingResolved {
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
                vector: crate::config::VectorResolved { enabled: false },
            },
        })
    }

    #[derive(Default)]
    struct ScriptedLlm {
        replies: Mutex<Vec<String>>,
    }
    impl LargeLanguageModel for ScriptedLlm {
        fn chat(
            &self,
            _messages: &[react_core::llm::ChatMessage],
            _options: &react_core::llm::LlmCallOptions,
        ) -> Result<String, String> {
            let mut g = self.replies.lock().map_err(|_| "mutex poisoned".to_string())?;
            if g.is_empty() {
                return Err("no more replies".to_string());
            }
            Ok(g.remove(0))
        }
        fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn patch_loop_retries_when_analyst_notes_contract_missing() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let llm: Arc<dyn LargeLanguageModel> = Arc::new(ScriptedLlm {
            replies: Mutex::new(vec![
                // First attempt: valid patch primitive, but missing required notes -> should trigger repair retry.
                serde_json::json!({
                    "path": "models/schema.yml",
                    "patch_text": "diff --git a/models/schema.yml b/models/schema.yml\n--- /dev/null\n+++ b/models/schema.yml\n@@ -0,0 +1,3 @@\n+version: 2\n+\n+models: []\n",
                    "notes": []
                })
                .to_string(),
                // Second attempt: same patch, now with required notes -> should succeed.
                serde_json::json!({
                    "path": "models/schema.yml",
                    "patch_text": "diff --git a/models/schema.yml b/models/schema.yml\n--- /dev/null\n+++ b/models/schema.yml\n@@ -0,0 +1,3 @@\n+version: 2\n+\n+models: []\n",
                    "notes": [
                        "Business question: x",
                        "Entity definition: x",
                        "Grain: x",
                        "Time axis: x",
                        "Metric definitions: x",
                        "Assumptions & gaps: x"
                    ]
                })
                .to_string(),
            ]),
        });
        let scope = RequestScope {
            tenant: "t".to_string(),
            workspace: "w".to_string(),
            project_id: "p".to_string(),
        };
        let ctx = AgentCtx {
            top_k: 1,
            per_step_timeout_secs: 1,
            max_steps: 2,
            thread_id: Some("tid".to_string()),
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: None,
            agent_name: Some("test".to_string()),
            policy: Arc::new(react_core::agent::DefaultPolicy),
            llm,
            storage: storage.clone(),
            scope: scope.clone(),
            keyspace,
            query: None,
            warehouse: Arc::new(react_core::providers::NullWarehouseProvider::default()),
            dbt: None,
            vector: None,
            thread_store: None,
            exec_ctx: None,
            runtime: Some(minimal_cfg() as Arc<dyn std::any::Any + Send + Sync>),
        };

        let (outcome, notes) = llm_patch_loop_single_file(
            &ctx,
            None,
            "ANALYST_NOTES_CONTRACT_V1".to_string(),
            serde_json::json!({"x": 1}).to_string(),
            "models/schema.yml",
            4,
            None,
        )
        .await
        .expect("ok");
        assert!(outcome.content.contains("version: 2"));
        assert!(notes.iter().any(|n| n.to_ascii_lowercase().starts_with("business question")));
    }
}
