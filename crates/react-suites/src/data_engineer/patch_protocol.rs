use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

use react_core::agent::AgentCtx;
use react_core::llm::ChatMessage;
use react_core::providers::DatasetCatalogProvider;

use crate::data_engineer::project_fs;

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

#[derive(Clone, Debug, Deserialize, Default)]
struct LlmPatchResponse {
    #[serde(default)]
    patch_text: String,
    #[serde(default)]
    notes: Vec<String>,
}

fn parse_llm_patch_response(text: &str) -> Result<LlmPatchResponse, String> {
    let v = parse_json_from_llm(text)?;
    serde_json::from_value(v).map_err(|e| format!("failed to parse patch response JSON: {}", e))
}

/// Ask the LLM for a patch for one expected file, apply it in-memory, and retry on patch errors.
///
/// - The LLM must return JSON: {"patch_text":"...","notes":[...]}
/// - `patch_text` must be a git-style unified diff (may include a single-file bundle).
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
    let existing = ctx
        .storage
        .get_bytes(&key)
        .await
        .ok()
        .map(|b| String::from_utf8_lossy(&b).to_string())
        .unwrap_or_default();

    let mut messages: Vec<ChatMessage> = vec![
        ChatMessage {
            role: "system".to_string(),
            content: sys_prompt,
        },
        ChatMessage {
            role: "user".to_string(),
            content: user_payload_json.clone(),
        },
    ];

    let mut last_err: Option<String> = None;
    for attempt in 1..=max_iters {
        let resp_text = ctx
            .llm
            .chat(&messages)
            .map_err(|e| format!("LLM patch authoring call failed: {}", e))?;
        let parsed = parse_llm_patch_response(&resp_text)?;
        let patch_text = parsed.patch_text.trim().to_string();
        if patch_text.is_empty() {
            last_err = Some("LLM returned empty patch_text".to_string());
        } else {
            match project_fs::apply_patch_bundle(ctx, datasets, &patch_text).await {
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
                Err(e) => {
                    last_err = Some(e);
                }
            }
        }

        // Repair prompt: feed back the failing patch + error + expected file + current content.
        let err = last_err.clone().unwrap_or_else(|| "unknown patch error".to_string());
        let repair = serde_json::json!({
            "attempt": attempt,
            "error": err,
            "expected_rel_path": expected_rel_path,
            "existing_content": existing,
            "previous_patch_text": parsed.patch_text,
            "instruction": "Return ONLY corrected JSON: {\"patch_text\":\"...\",\"notes\":[...]}. The patch MUST modify ONLY expected_rel_path and be git-style unified diff. If creating a new file, it MUST use --- /dev/null and +++ b/<path>."
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

