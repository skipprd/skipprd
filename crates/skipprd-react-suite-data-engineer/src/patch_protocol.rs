use serde_json::{Map, Value};
use std::sync::Arc;

use crate::providers::DatasetCatalogProvider;
use react_core::agent::AgentCtx;
use react_core::llm::LlmCallOptions;
use react_core::llm::{ChatMessage, ChatRole};

use crate::patch_contract::{normalize_hunks_only_patch_text, LlmSingleFilePatchResponse};
use crate::project_fs;

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
    crate::env_util::env_u32(crate::env_util::env_keys::REACT_PATCH_LOOP_MAX_OUTPUT_TOKENS)
        .filter(|v| *v >= 512)
        .unwrap_or(3200)
}

#[derive(Clone, Debug)]
pub struct PatchBaseState {
    pub base_exists: bool,
    pub base_sha256: String,
}

pub async fn read_patch_base_state(ctx: &AgentCtx, rel_path: &str) -> PatchBaseState {
    let key = project_fs::join_storage_key(ctx, rel_path);
    let existing_opt = match project_fs::read_file_text_sync(rel_path) {
        Ok(Some(text)) => Some(text),
        _ => ctx
            .storage()
            .get_bytes(&key)
            .await
            .ok()
            .map(|b| String::from_utf8_lossy(&b).to_string()),
    };
    let base_exists = existing_opt.is_some();
    let existing = existing_opt.unwrap_or_default();
    PatchBaseState {
        base_exists,
        base_sha256: sha256_hex(&existing),
    }
}

fn normalize_patch_apply_error(err: String) -> String {
    if err.contains("patch_hunk_context_miss") {
        return format!(
            "{}\n\nPatch apply guidance: ensure patch_text contains real hunk edits with '-' and/or '+' lines under '@@ ... @@' for the exact target path.",
            err
        );
    }
    err
}

pub async fn apply_single_file_patch_with_base(
    ctx: &AgentCtx,
    datasets: Option<&Arc<dyn DatasetCatalogProvider>>,
    rel_path: &str,
    patch_text: &str,
    base: &PatchBaseState,
) -> Result<project_fs::PatchOutcome, String> {
    project_fs::apply_patch(
        ctx,
        datasets,
        rel_path,
        patch_text,
        Some(base.base_sha256.as_str()),
        Some(base.base_exists),
        project_fs::PatchApplyKind::UnifiedDiff,
    )
    .await
    .map_err(normalize_patch_apply_error)
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

fn parse_json_object_from_llm(text: &str) -> Result<Map<String, Value>, String> {
    let v: Value = react_core::json_repair::resilient_parse(text)?;
    v.as_object()
        .cloned()
        .ok_or_else(|| "LLM response was not a JSON object".to_string())
}

fn looks_like_patch_object(v: &Value) -> bool {
    let Some(m) = v.as_object() else { return false };
    m.contains_key("patch_text")
}

fn parse_patch_json_from_llm(text: &str) -> Result<Map<String, Value>, String> {
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        if v.is_object() && looks_like_patch_object(&v) {
            if let Some(obj) = v.as_object() {
                return Ok(obj.clone());
            }
        }
    }
    let s = text.trim();
    let vals = react_core::json_repair::extract_all_json_values(s, 16);
    let mut last_obj: Option<Map<String, Value>> = None;
    for vtxt in vals.iter().rev() {
        if let Ok(v) = serde_json::from_str::<Value>(vtxt) {
            if v.is_object() {
                if looks_like_patch_object(&v) {
                    if let Some(obj) = v.as_object() {
                        return Ok(obj.clone());
                    }
                }
                last_obj = v.as_object().cloned();
            }
        }
    }
    if let Some(v) = last_obj {
        return Ok(v);
    }
    Err("LLM response did not contain valid JSON object".to_string())
}

fn parse_llm_patch_response(
    text: &str,
    expected_rel_path: &str,
) -> Result<LlmSingleFilePatchResponse, String> {
    let v = parse_patch_json_from_llm(text)?;
    let mut parsed: LlmSingleFilePatchResponse = serde_json::from_value(Value::Object(v))
        .map_err(|e| format!("failed to parse patch response JSON: {}", e))?;
    let rel = project_fs::normalize_rel_path(parsed.path.as_str())?;
    if rel != expected_rel_path {
        return Err(format!(
            "path '{}' did not match expected_rel_path '{}'",
            rel, expected_rel_path
        ));
    }
    let normalized =
        normalize_hunks_only_patch_text(parsed.patch_text.as_str(), expected_rel_path)?;
    parsed.patch_text = normalized.patch_text;
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
        .keyspace()
        .scoped_prefix(ctx.scope(), &["dbt"])
        .trim_end_matches('/')
        .to_string();
    let key = format!("{}/{}", base, expected_rel_path);
    let existing_opt = ctx
        .storage()
        .get_bytes(&key)
        .await
        .ok()
        .map(|b| String::from_utf8_lossy(&b).to_string());
    let existed = existing_opt.is_some();
    let existing = existing_opt.unwrap_or_default();
    let base_sha256 = sha256_hex(&existing);
    let base_state = PatchBaseState {
        base_exists: existed,
        base_sha256: base_sha256.clone(),
    };
    // Prompt facts used to help the LLM produce stable hunks.
    // We cap numbered content to avoid doubling prompt size for large files.
    let (
        existing_content_with_line_numbers,
        existing_content_with_line_numbers_truncated,
        existing_line_count,
        existing_had_trailing_newline,
    ) = format_with_line_numbers(&existing, 200_000);

    // Always include the raw file content in the prompt payload.
    let user_payload_value = Value::Object(parse_json_object_from_llm(&user_payload_json)?);
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
        "instruction": "path MUST equal expected_rel_path. patch_text MUST be Cursor/Aider-style hunks-only unified diff starting with '@@ ... @@' and omitting ---/+++ headers. Do NOT use line-number hunk headers like '@@ -a,b +c,d @@'. The patch MUST modify ONLY expected_rel_path. If prior attempts produced no-op patches, rewrite the entire file using a single large hunk."
    })
    .to_string();

    let mut messages: Vec<ChatMessage> = vec![
        ChatMessage {
            role: ChatRole::System,
            content: sys_prompt,
        },
        ChatMessage {
            role: ChatRole::User,
            content: initial_user.clone(),
        },
    ];

    let patch_schema = react_core::schema_registry::OpenAiStrictSchema::for_type::<
        crate::patch_contract::LlmSingleFilePatchResponse,
    >("data_engineer.patch_response")
    .map_err(|e| format!("schema build error: {e}"))?;

    let mut call_opts = llm_options.unwrap_or_else(|| react_core::llm::LlmCallOptions {
        prompt_id: "data_engineer.patch_protocol.llm_patch_loop",
        reasoning_effort: Some(react_core::llm::ReasoningEffort::Low),
        ..Default::default()
    });
    call_opts.expected_format = react_core::llm::LlmExpectedFormat::JsonSchema(patch_schema);
    if call_opts.max_output_tokens.is_none() {
        call_opts.max_output_tokens = Some(default_patch_loop_max_output_tokens());
    }

    let mut last_err: Option<String> = None;
    let mut bumped_output_budget = false;
    let mut no_op_failures = 0usize;
    let mut seen_patch_signatures: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    let no_op_breaker_threshold = 2usize;
    for attempt in 1..=max_iters {
        let resp = ctx.llm_chat(&messages, &call_opts).await;
        let resp_text = match resp {
            Ok(t) => t,
            Err(e) => {
                if !bumped_output_budget && e.to_string().contains("max_output_tokens") {
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
                "instruction": "path MUST equal expected_rel_path. patch_text MUST be Cursor/Aider-style hunks-only unified diff starting with '@@ ... @@'. Do NOT use line-number hunk headers like '@@ -a,b +c,d @@'. The patch MUST modify ONLY expected_rel_path. Rewrite the entire file using one large hunk if needed."
            })
            .to_string();
            messages.push(ChatMessage {
                role: ChatRole::User,
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
                "instruction": "Correct the patch_text. Rewrite the entire file in one hunk if needed."
            })
            .to_string();
            messages.push(ChatMessage {
                role: ChatRole::User,
                content: repair,
            });
            continue;
        }

        // Deterministic idempotence guard: if the model repeats the same patch against the same
        // base for actual apply attempts, stop early to avoid churn loops.
        let patch_sig = format!(
            "{}:{}:{}",
            expected_rel_path,
            base_sha256,
            sha256_hex(parsed.patch_text.as_str())
        );
        if !seen_patch_signatures.insert(patch_sig) {
            return Err(format!(
                "patch_idempotence_guard: repeated identical patch_text for path='{}' and base_sha256='{}'; refusing to retry the same patch to avoid churn",
                expected_rel_path, base_sha256
            ));
        }

        match apply_single_file_patch_with_base(
            ctx,
            datasets,
            expected_rel_path,
            parsed.patch_text.as_str(),
            &base_state,
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
            "instruction": "path MUST equal expected_rel_path. patch_text MUST be Cursor/Aider-style hunks-only unified diff starting with '@@ ... @@'. Do NOT use line-number hunk headers like '@@ -a,b +c,d @@'. The patch MUST modify ONLY expected_rel_path. If repeated no-ops occur, rewrite the entire file using one large hunk."
        })
        .to_string();
        messages.push(ChatMessage {
            role: ChatRole::User,
            content: repair,
        });
    }

    Err(format!(
        "LLM patch repair failed after {} attempt(s): {}",
        max_iters,
        last_err.unwrap_or_else(|| "no patch error details were captured".to_string())
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx_ext::{ProvidersCfgCap, WarehouseCap};
    use crate::de_config;
    use crate::providers::warehouse::NullWarehouseProvider;
    use react_core::keyspace::{DefaultKeyspace, Keyspace};
    use react_core::llm::LargeLanguageModel;
    use react_core::scope::RequestScope;
    use react_core::storage::StorageAdapter;
    use react_module_storage_memory::InMemoryStorageAdapter;
    use std::sync::{Arc, Mutex};

    #[test]
    fn parse_llm_patch_response_accepts_patch_text() {
        let txt = r#"{
          "path": "models/schema.yml",
          "patch_text": "@@ ... @@\n- a\n+ b\n",
          "notes": ["ok"]
        }"#;
        let parsed = parse_llm_patch_response(txt, "models/schema.yml").expect("parse ok");
        assert_eq!(parsed.path.as_str(), "models/schema.yml");
        assert!(parsed.patch_text.contains("@@"));
        assert_eq!(parsed.notes, vec!["ok".to_string()]);
    }

    #[test]
    fn parse_llm_patch_response_rejects_wrong_path() {
        let txt = r#"{
          "path": "models/other.yml",
          "patch_text": "@@ ... @@\n- a\n+ b\n"
        }"#;
        let err = parse_llm_patch_response(txt, "models/schema.yml").unwrap_err();
        assert!(err.contains("expected_rel_path"));
    }

    #[test]
    fn parse_llm_patch_response_normalizes_line_number_hunks() {
        let txt = r#"{
          "path": "models/schema.yml",
          "patch_text": "@@ -1,1 +1,1 @@\n- a\n+ b\n"
        }"#;
        let parsed = parse_llm_patch_response(txt, "models/schema.yml").expect("parse ok");
        assert!(parsed.patch_text.starts_with("@@ ... @@"));
    }

    #[test]
    fn parse_llm_patch_response_strips_git_headers_for_expected_path() {
        let txt = r#"{
          "path": "models/schema.yml",
          "patch_text": "diff --git a/models/schema.yml b/models/schema.yml\n--- a/models/schema.yml\n+++ b/models/schema.yml\n@@ -1,1 +1,1 @@\n- a\n+ b\n"
        }"#;
        let parsed = parse_llm_patch_response(txt, "models/schema.yml").expect("parse ok");
        assert!(parsed.patch_text.starts_with("@@ ... @@"));
        assert!(!parsed.patch_text.contains("diff --git"));
        assert!(!parsed.patch_text.contains("--- "));
        assert!(!parsed.patch_text.contains("+++ "));
    }

    #[test]
    fn parse_llm_patch_response_rejects_git_headers_for_wrong_path() {
        let txt = r#"{
          "path": "models/schema.yml",
          "patch_text": "diff --git a/models/other.yml b/models/other.yml\n--- a/models/other.yml\n+++ b/models/other.yml\n@@ -1,1 +1,1 @@\n- a\n+ b\n"
        }"#;
        let err = parse_llm_patch_response(txt, "models/schema.yml").unwrap_err();
        assert!(err.contains("must target expected path"));
    }

    fn minimal_cfg() -> Arc<react_core::resolved_config::ReactResolvedConfig> {
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
                "warehouse": {
                    "kind": "athena",
                    "container": "AwsDataCatalog",
                    "namespace": "test_raw",
                    "extras": {"region":"eu-west-1","workgroup":"wg","result_s3":"s3://x/"}
                },
                "catalog": {
                    "enabled": false,
                    "refresh_secs": 60,
                    "max_concurrency": 8
                },
                "dbt": {
                    "enabled": true,
                    "target": "athena",
                    "naming": {
                        "target_schema": "test",
                        "silver_suffix": "silver",
                        "gold_suffix": "gold"
                    },
                    "runner": "host"
                },
                "vector": {
                    "enabled": false
                }
            }),
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
            let mut g = self
                .replies
                .lock()
                .map_err(|_| "mutex poisoned".to_string())?;
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
                    "patch_text": "@@ ... @@\n+version: 2\n+\n+models: []\n",
                    "notes": []
                })
                .to_string(),
                // Second attempt: same patch, now with required notes -> should succeed.
                serde_json::json!({
                    "path": "models/schema.yml",
                    "patch_text": "@@ ... @@\n+version: 2\n+\n+models: []\n",
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
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let mut ctx = react_core::agent::AgentCtxBuilder::new(
            llm,
            storage.clone(),
            scope.clone(),
            keyspace,
            Arc::new(react_core::agent::DefaultPolicy),
        )
        .top_k(1)
        .per_step_timeout_secs(1)
        .max_steps(2)
        .thread_id("tid".to_string())
        .agent_name("test".to_string())
        .resolved_config(Some(minimal_cfg()))
        .build();
        let providers =
            de_config::de_config_from_resolved(ctx.resolved_config().as_ref().unwrap()).unwrap();
        ctx.set_capability(Arc::new(ProvidersCfgCap(providers)));
        ctx.set_capability(Arc::new(WarehouseCap(
            Arc::new(NullWarehouseProvider) as Arc<dyn crate::providers::WarehouseProvider>
        )));

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
        assert!(notes
            .iter()
            .any(|n| n.to_ascii_lowercase().starts_with("business question")));
    }
}
