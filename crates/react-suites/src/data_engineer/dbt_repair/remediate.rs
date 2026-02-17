use crate::config::ReactResolvedConfig;
use react_core::agent::AgentCtx;
use react_core::llm::ChatMessage;
use react_core::llm::LlmCallOptions;
use react_core::llm_observability::{self, PartInput};
use react_core::providers::{DatasetCatalogProvider, DatasetId};
use react_core::session::{Observation, ThreadStep};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet as StdBTreeSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

async fn record_llm_call_observability_async(
    ctx: &AgentCtx,
    phase: &str,
    model: &str,
    messages: &[ChatMessage],
    parts: &[PartInput],
    response_raw: &str,
    ok: bool,
) {
    if !llm_observability::llm_calls_enabled() {
        return;
    }
    let Some(thread_id) = ctx.thread_id.as_deref() else {
        return;
    };
    let Some(store) = ctx.thread_store.as_ref() else {
        return;
    };
    let agent = ctx
        .agent_name
        .clone()
        .unwrap_or_else(|| "unknown".to_string());

    let call_id = llm_observability::next_call_id(thread_id);
    let prompt_hash = llm_observability::prompt_hash_for_messages(messages);
    let built = llm_observability::build_parts_for_thread(thread_id, parts);
    let response_hash = llm_observability::sha256_hex_str(response_raw);
    let response_text = if llm_observability::llm_response_text_enabled() {
        Some(llm_observability::redact_common_secrets(response_raw))
    } else {
        None
    };

    tracing::debug!(
        "LLM_CALL thread_id={} call_id={} agent={} phase={} model={} prompt_hash={} response_hash={}",
        thread_id,
        call_id,
        agent,
        phase,
        model,
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
                model: model.to_string(),
                phase: phase.to_string(),
                prompt_hash,
                parts: built.parts,
                part_hashes: built.part_hashes,
                response_hash,
                response_text,
                observation: if ok {
                    Observation::ok()
                } else {
                    Observation::fail(vec!["llm_call_failed".to_string()])
                },
                ts: chrono::Utc::now().to_rfc3339(),
                agent,
            },
        )
        .await;
}

fn record_llm_call_observability(
    ctx: &AgentCtx,
    phase: &str,
    model: &str,
    messages: &[ChatMessage],
    parts: &[PartInput],
    response_raw: &str,
    ok: bool,
) {
    // For non-async call sites: best-effort spawn when inside a tokio runtime.
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let ctx = ctx.clone();
        let phase = phase.to_string();
        let model = model.to_string();
        let messages = messages.to_vec();
        let parts = parts.to_vec();
        let response_raw = response_raw.to_string();
        handle.spawn(async move {
            record_llm_call_observability_async(
                &ctx,
                &phase,
                &model,
                &messages,
                &parts,
                &response_raw,
                ok,
            )
            .await;
        });
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct RemediationChange {
    pub key: String,
    pub reason: Option<String>,
    pub changed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct RemediationDiff {
    pub key: String,
    #[serde(default)]
    pub rel_path: String,
    #[serde(default)]
    pub base_sha256: String,
    #[serde(default)]
    pub new_sha256: String,
    #[serde(default)]
    pub lines_added: usize,
    #[serde(default)]
    pub lines_removed: usize,
    /// Truncated git-style unified diff for debugging. Omitted/empty when not available.
    #[serde(default)]
    pub diff: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct RemediationReport {
    pub dialect: String,
    pub phase: String,
    pub scanned_files: usize,
    pub changed_files: usize,
    #[serde(default)]
    pub changes: Vec<RemediationChange>,
    /// Per-file diff metadata for each applied change (sha256 + line counts + truncated diff).
    #[serde(default)]
    pub diffs: Vec<RemediationDiff>,
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default)]
    pub skipped: bool,
    #[serde(default)]
    pub error: Option<String>,
}

pub(crate) fn truncate_diff(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if s.len() <= max_chars {
        return s.to_string();
    }
    format!("{}…", &s[..max_chars])
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct LlmRemediationResponse {
    #[serde(default)]
    changes: Vec<LlmChange>,
    #[serde(default)]
    notes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct LlmChange {
    key: String,
    #[serde(default)]
    replace_file: Option<ReplaceFile>,
    #[serde(default)]
    replace_range: Option<ReplaceRange>,
    #[serde(default)]
    replace_list: Option<ReplaceList>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ReplaceFile {
    new_text: String,
    #[serde(default)]
    expected_sha256: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ReplaceRange {
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
    edits: Vec<ReplaceListEdit>,
    #[serde(default)]
    expected_sha256: Option<String>,
}


#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct LlmRemediationDecision {
    #[serde(default)]
    pub should_remediate: bool,
    #[serde(default)]
    pub confidence: f32,
    #[serde(default)]
    pub reason: String,
}

pub fn active_provider_dialect(cfg: &ReactResolvedConfig) -> String {
    // Human-readable label, consumed by the LLM prompt. Keep it stable (used in logs/tool outputs).
    match cfg
        .providers
        .warehouse
        .kind
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "athena" => "Amazon Athena (engine v3 / Trino SQL)".to_string(),
        "postgres" => "PostgreSQL".to_string(),
        "mssql" | "sqlserver" => "Microsoft SQL Server (T-SQL)".to_string(),
        "snowflake" => "Snowflake SQL".to_string(),
        "bigquery" => "Google BigQuery (Standard SQL)".to_string(),
        _ => "Unknown SQL dialect".to_string(),
    }
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn sha256_hex(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    let out = hasher.finalize();
    hex::encode(out)
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

pub fn llm_should_remediate_sql(
    ctx: &AgentCtx,
    dialect: &str,
    phase: &str,
    error_brief: &str,
) -> Result<LlmRemediationDecision, String> {
    // Strict JSON-only contract. This is a lightweight classifier to decide whether to run the
    // expensive, potentially invasive remediation pass.
    let sys = format!(
        "You are a SQL dialect expert.\n\
         Task: decide if dbt SQL files likely need *dialect/syntax* remediation.\n\
         Dialect: {dialect}\n\
         Phase: {phase}\n\
         Return ONLY valid JSON (no markdown, no commentary).\n\
         Output schema:\n\
         {{\"should_remediate\":true|false,\"confidence\":0.0-1.0,\"reason\":\"...\"}}\n\
         Rules:\n\
         - Only set should_remediate=true when you are confident errors are caused by dialect/syntax incompatibility.\n\
         - If errors are about missing nodes/models/sources, permissions, missing tables, or data issues, set should_remediate=false.\n"
    );
    let user = serde_json::json!({
        "error_brief": error_brief,
    })
    .to_string();

    let messages = vec![
        ChatMessage {
            role: "system".to_string(),
            content: sys.clone(),
        },
        ChatMessage {
            role: "user".to_string(),
            content: user.clone(),
        },
    ];
    let parts: Vec<PartInput> = vec![
        PartInput {
            name: "system".to_string(),
            text: sys.clone(),
        },
        PartInput {
            name: "user.phase".to_string(),
            text: phase.to_string(),
        },
        PartInput {
            name: "user.error_brief".to_string(),
            text: error_brief.to_string(),
        },
        PartInput {
            name: "user.payload".to_string(),
            text: user.clone(),
        },
    ];
    let call_opts = LlmCallOptions {
        prompt_id: "data_engineer.dbt_should_remediate",
        thread_id: ctx.thread_id.clone(),
        expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
        temperature: Some(0.0),
        top_p: Some(1.0),
        max_output_tokens: Some(1400),
        reasoning_effort: None,
    };
    let resp_text = match ctx.llm.chat(&messages, &call_opts) {
        Ok(t) => {
            record_llm_call_observability(ctx, phase, "unknown", &messages, &parts, &t, true);
            t
        }
        Err(e) => {
            let raw = format!("LLM_ERROR: {}", e);
            record_llm_call_observability(ctx, phase, "unknown", &messages, &parts, &raw, false);
            return Err(format!("llm should_remediate call failed: {}", e));
        }
    };
    let v = parse_json_from_llm(&resp_text)?;
    let mut parsed: LlmRemediationDecision = serde_json::from_value(v)
        .map_err(|e| format!("failed to parse remediation decision JSON: {}", e))?;
    if !(0.0..=1.0).contains(&parsed.confidence) {
        // Be conservative on malformed values.
        parsed.confidence = parsed.confidence.max(0.0).min(1.0);
    }
    Ok(parsed)
}

pub async fn list_sql_keys_for_scope(ctx: &AgentCtx) -> Result<Vec<String>, String> {
    let base = ctx
        .keyspace
        .dbt_prefix(&ctx.scope)
        .trim_end_matches('/')
        .to_string()
        + "/";
    let keys = ctx.storage.list_prefix(&base).await.unwrap_or_default();
    let mut out: Vec<String> = Vec::new();
    for k in keys {
        if !k.ends_with(".sql") {
            continue;
        }
        if k.contains("/target/") || k.contains("/_versions/") {
            continue;
        }
        out.push(k);
    }
    out.sort();
    Ok(out)
}

pub async fn remediate_dbt_sql_keys_with_llm(
    ctx: &AgentCtx,
    phase: &str,
    keys: &[String],
) -> Result<RemediationReport, String> {
    let Some(cfg) = crate::config::resolved_config_from_ctx(ctx) else {
        return Ok(RemediationReport {
            dialect: "Unknown SQL dialect".to_string(),
            phase: phase.to_string(),
            scanned_files: 0,
            changed_files: 0,
            skipped: true,
            error: Some("resolved_config missing".to_string()),
            ..Default::default()
        });
    };
    let dialect = active_provider_dialect(cfg);

    let mut keys: Vec<String> = keys.iter().cloned().collect();
    keys.sort();
    keys.dedup();
    let scanned_files = keys.len();
    if scanned_files == 0 {
        return Ok(RemediationReport {
            dialect,
            phase: phase.to_string(),
            scanned_files,
            changed_files: 0,
            skipped: true,
            ..Default::default()
        });
    }

    tracing::info!(
        target: "dbt_sql_remediate",
        phase = %phase,
        dialect = %dialect,
        scanned_files = scanned_files,
        "starting"
    );

    // Chunking: keep each LLM call bounded.
    // We bias toward fewer files per call rather than truncating any file.
    let max_chars_per_batch: usize = 45_000;
    let mut idx: usize = 0;
    let mut report = RemediationReport {
        dialect: dialect.clone(),
        phase: phase.to_string(),
        scanned_files,
        ..Default::default()
    };

    while idx < keys.len() {
        let mut batch: Vec<(String, String)> = Vec::new(); // (key, content)
        let mut chars: usize = 0;
        while idx < keys.len() {
            let k = &keys[idx];
            let bytes = ctx.storage.get_bytes(k).await.unwrap_or_default();
            let text = String::from_utf8_lossy(&bytes).to_string();
            let add = k.len() + text.len();
            if !batch.is_empty() && chars + add > max_chars_per_batch {
                break;
            }
            chars += add;
            batch.push((k.clone(), text));
            idx += 1;
        }

        let input_files: Vec<Value> = batch
            .iter()
            .map(|(k, c)| serde_json::json!({ "key": k, "content": c }))
            .collect();
        let mut content_by_key: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for (k, c) in batch.iter() {
            content_by_key.insert(k.clone(), c.clone());
        }

        // Strict JSON-only contract so we can apply changes deterministically.
        let sys = format!(
            "You are a meticulous SQL dialect remediation assistant.\n\
             Task: rewrite dbt SQL files so they are valid for the configured warehouse dialect.\n\
             Dialect: {dialect}\n\
             Constraints:\n\
             - Do not change business logic or semantics.\n\
             - Apply the smallest edit necessary to make SQL valid for the dialect.\n\
             - Do not invent new tables/columns.\n\
             - Output MUST be valid JSON only (no markdown, no commentary).\n\
             Output schema:\n\
             {{\"changes\":[{{\"key\":\"...\",\"replace_file\":{{\"new_text\":\"...\"}}|null,\"replace_range\":{{\"start_line\":1,\"end_line\":1,\"new_text\":\"...\"}}|null,\"replace_list\":{{\"edits\":[{{\"start_line\":1,\"end_line\":1,\"new_text\":\"...\"}}]}}|null,\"reason\":\"...\"}}],\"notes\":[\"...\"]}}\n\
             Rules:\n\
             - For each change, choose EXACTLY ONE of replace_file / replace_range / replace_list (the others must be null).\n\
             - Do NOT include expected_sha256; the suite enforces drift safety from grounded file content.\n\
             Only include a file in changes if you actually modify it.\n"
        );
        let user = serde_json::json!({
            "phase": phase,
            "files": input_files,
        })
        .to_string();

        let messages = vec![
            ChatMessage {
                role: "system".to_string(),
                content: sys.clone(),
            },
            ChatMessage {
                role: "user".to_string(),
                content: user.clone(),
            },
        ];
        let parts: Vec<PartInput> = vec![
            PartInput {
                name: "system".to_string(),
                text: sys.clone(),
            },
            PartInput {
                name: "user.phase".to_string(),
                text: phase.to_string(),
            },
            PartInput {
                name: "user.files".to_string(),
                text: serde_json::to_string_pretty(&input_files)
                    .unwrap_or_else(|_| "[]".to_string()),
            },
        ];
        let call_opts = LlmCallOptions {
        prompt_id: "data_engineer.dbt_dialect_remediation",
            thread_id: ctx.thread_id.clone(),
            expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
            temperature: Some(0.0),
            top_p: Some(1.0),
            max_output_tokens: Some(1400),
            reasoning_effort: None,
        };
        let resp_text = match ctx.llm.chat(&messages, &call_opts) {
            Ok(t) => {
                record_llm_call_observability_async(
                    ctx, phase, "unknown", &messages, &parts, &t, true,
                )
                .await;
                t
            }
            Err(e) => {
                let raw = format!("LLM_ERROR: {}", e);
                record_llm_call_observability_async(
                    ctx, phase, "unknown", &messages, &parts, &raw, false,
                )
                .await;
                return Err(format!("dialect remediation LLM call failed: {}", e));
            }
        };

        let v = parse_json_from_llm(&resp_text)?;
        let parsed: LlmRemediationResponse = serde_json::from_value(v)
            .map_err(|e| format!("failed to parse remediation JSON: {}", e))?;

        for n in parsed.notes.iter() {
            if !n.trim().is_empty() {
                report.notes.push(n.clone());
            }
        }

        for ch in parsed.changes.into_iter() {
            if ch.key.trim().is_empty() {
                continue;
            }
            // Only apply if key is within the scanned set (safety).
            if !keys.iter().any(|k| k == &ch.key) {
                continue;
            }
            let base = ctx
                .keyspace
                .dbt_prefix(&ctx.scope)
                .trim_end_matches('/')
                .to_string();
            let rel = ch
                .key
                .strip_prefix(&(base.clone() + "/"))
                .ok_or_else(|| format!("remediation key not under dbt prefix: {}", ch.key))?
                .to_string();
            let expected_base = content_by_key
                .get(&ch.key)
                .map(|s| sha256_hex(s))
                .unwrap_or_else(|| String::new());
            let existing = content_by_key.get(&ch.key).cloned().unwrap_or_default();
            let mut provided = 0usize;
            if ch.replace_file.is_some() {
                provided += 1;
            }
            if ch.replace_range.is_some() {
                provided += 1;
            }
            if ch.replace_list.is_some() {
                provided += 1;
            }
            if provided != 1 {
                return Err(
                    "remediation change must include exactly one of: replace_file | replace_range | replace_list"
                        .to_string(),
                );
            }
            let patch_text = if let Some(rf) = ch.replace_file.as_ref() {
                crate::data_engineer::project_fs::create_git_patch_text(
                    &existing,
                    &rf.new_text,
                    &rel,
                    true,
                )?
            } else if let Some(rr) = ch.replace_range.as_ref() {
                let new_text = crate::data_engineer::project_fs::apply_replace_range(
                    &existing,
                    rr.start_line,
                    rr.end_line,
                    &rr.new_text,
                )?;
                crate::data_engineer::project_fs::create_git_patch_text(
                    &existing, &new_text, &rel, true,
                )?
            } else if let Some(rl) = ch.replace_list.as_ref() {
                let edits: Vec<crate::data_engineer::project_fs::ReplaceListEdit> = rl
                    .edits
                    .iter()
                    .map(|e| crate::data_engineer::project_fs::ReplaceListEdit {
                        start_line: e.start_line,
                        end_line: e.end_line,
                        new_text: e.new_text.clone(),
                    })
                    .collect();
                let new_text = crate::data_engineer::project_fs::apply_replace_list(
                    &existing,
                    &edits,
                )?;
                crate::data_engineer::project_fs::create_git_patch_text(
                    &existing, &new_text, &rel, true,
                )?
            } else {
                return Err("invalid remediation change".to_string());
            };
            let outcome = crate::data_engineer::project_fs::apply_patch(
                ctx,
                None,
                &rel,
                &patch_text,
                if expected_base.is_empty() { None } else { Some(expected_base.as_str()) },
                Some(!expected_base.is_empty()),
                crate::data_engineer::project_fs::PatchApplyKind::UnifiedDiff,
            )
            .await?;
            ctx.storage
                .put_bytes(&outcome.key, outcome.content.as_bytes(), "text/sql")
                .await?;
            report.changed_files += 1;
            report.changes.push(RemediationChange {
                key: ch.key,
                reason: ch.reason,
                changed: true,
            });
        }
    }

    // Best-effort: add a note for traceability.
    if report.changed_files > 0 {
        report
            .notes
            .push(format!("remediation_epoch_secs={}", now_epoch_secs()));
    }

    tracing::info!(
        target: "dbt_sql_remediate",
        phase = %phase,
        dialect = %dialect,
        changed_files = report.changed_files,
        "finished"
    );
    Ok(report)
}

pub async fn remediate_dbt_sql_with_llm(
    ctx: &AgentCtx,
    phase: &str,
) -> Result<RemediationReport, String> {
    let keys = list_sql_keys_for_scope(ctx).await?;
    remediate_dbt_sql_keys_with_llm(ctx, phase, &keys).await
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct GroundedRepairResponse {
    #[serde(default)]
    changes: Vec<GroundedRepairChange>,
    #[serde(default)]
    notes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct GroundedRepairChange {
    key: String,
    #[serde(default)]
    replace_file: Option<ReplaceFile>,
    #[serde(default)]
    replace_range: Option<ReplaceRange>,
    #[serde(default)]
    replace_list: Option<ReplaceList>,
    #[serde(default)]
    reason: Option<String>,
}

async fn best_effort_samples_for_source(
    ctx: &AgentCtx,
    source_schema: &str,
    source_table: &str,
    limit: usize,
) -> Option<Value> {
    let cfg = crate::config::resolved_config_from_ctx(ctx);
    let catalog = cfg
        .map(|c| c.providers.warehouse.container.clone())
        .unwrap_or_default();
    let fqn = format!("{}.{}.{}", catalog, source_schema, source_table);

    if limit == 0 {
        return None;
    }

    let header: Vec<String> = ctx
        .warehouse
        .schema(&fqn)
        .await
        .ok()
        .unwrap_or_default()
        .into_iter()
        .map(|(n, _t)| n)
        .collect();

    let rows = ctx.warehouse.sample(&fqn, limit).await.ok()?;
    Some(serde_json::json!({
        "dataset_fqn": fqn,
        "limit": limit,
        "header": header,
        "rows": rows
    }))
}

fn errors_suggest_uncertainty(errors: &[String]) -> bool {
    let s = crate::data_engineer::dbt_error::compact_brief(errors, 8, 2000).to_lowercase();
    // Any of these typically indicate we must rely on ground truth schema and/or real data.
    s.contains("cannot be resolved")
        || s.contains("$operator$cast(row")
        || s.contains("cast(row")
        || s.contains("json_extract")
        || s.contains("mismatched input")
        || s.contains("cannot cast")
}

fn extract_run_model_schema_map(errors: &[String]) -> BTreeMap<String, String> {
    // Best-effort parse of dbt run/build log lines like:
    //   "52 of 58 START sql view model test_silver.stg_foo .... [RUN]"
    //   "52 of 58 ERROR creating sql view model test_gold.fct_bar ... [ERROR]"
    //
    // We map model_name -> most frequent schema observed in the logs.
    let mut counts: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    for line in errors.join("\n").lines() {
        let s = line.trim();
        if s.is_empty() {
            continue;
        }
        let lower = s.to_ascii_lowercase();
        let Some(pos) = lower.find("model ") else {
            continue;
        };
        let rest = s[pos + "model ".len()..].trim();
        let Some(tok) = rest.split_whitespace().next() else {
            continue;
        };
        let tok = tok.trim_matches(|c: char| {
            c == '(' || c == ')' || c == '"' || c == '\'' || c == ',' || c == ';'
        });
        // Expect <schema>.<model_name>
        let Some((schema, model)) = tok.split_once('.') else {
            continue;
        };
        let schema = schema.trim().to_string();
        let model = model.trim().to_string();
        if schema.is_empty() || model.is_empty() {
            continue;
        }
        *counts.entry(model).or_default().entry(schema).or_insert(0) += 1;
    }

    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for (model, by_schema) in counts.into_iter() {
        let mut best: Option<(String, usize)> = None;
        for (schema, n) in by_schema.into_iter() {
            match best.as_ref() {
                None => best = Some((schema, n)),
                Some((_bs, bn)) if n > *bn => best = Some((schema, n)),
                _ => {}
            }
        }
        if let Some((schema, _n)) = best {
            out.insert(model, schema);
        }
    }
    out
}

fn infer_schema_for_ref(errors: &[String], ref_name: &str) -> Option<String> {
    let ref_name = ref_name.trim();
    if ref_name.is_empty() {
        return None;
    }
    let map = extract_run_model_schema_map(errors);
    if let Some(s) = map.get(ref_name) {
        return Some(s.clone());
    }
    // Fallback: prefer a schema that appears to be silver for stg_* refs, otherwise gold if present.
    let mut saw_silver: Option<String> = None;
    let mut saw_gold: Option<String> = None;
    for (_m, schema) in map.into_iter() {
        let sl = schema.to_ascii_lowercase();
        if saw_silver.is_none() && sl.contains("silver") {
            saw_silver = Some(schema.clone());
        }
        if saw_gold.is_none() && sl.contains("gold") {
            saw_gold = Some(schema.clone());
        }
    }
    if ref_name.to_ascii_lowercase().starts_with("stg_") {
        return saw_silver.or(saw_gold);
    }
    saw_gold.or(saw_silver)
}

async fn best_effort_schema_columns_for_relation(
    ctx: &AgentCtx,
    fqn: &str,
) -> Vec<(String, String)> {
    let Some(q) = ctx.query.as_ref() else {
        return vec![];
    };
    q.schema(fqn).await.ok().unwrap_or_default()
}

/// Single, grounded LLM repair pass for dbt failures.
///
/// Contract:
/// - Provide immutable facts (dialect, errors, failing + related files, source schemas, optional samples).
/// - LLM must return a single JSON object and propose only edits required to fix the provided errors.
/// - LLM returns structured patch primitives per changed key; we canonicalize and apply patches deterministically.
pub async fn remediate_dbt_failures_grounded_with_llm(
    ctx: &AgentCtx,
    phase: &str,
    errors: &[String],
    keys: &[String],
    datasets: Option<&Arc<dyn DatasetCatalogProvider>>,
) -> Result<RemediationReport, String> {
    let Some(cfg) = crate::config::resolved_config_from_ctx(ctx) else {
        return Ok(RemediationReport {
            dialect: "Unknown SQL dialect".to_string(),
            phase: phase.to_string(),
            scanned_files: 0,
            changed_files: 0,
            skipped: true,
            error: Some("resolved_config missing".to_string()),
            ..Default::default()
        });
    };
    let dialect = active_provider_dialect(cfg);
    let catalog = cfg.providers.warehouse.container.clone();

    let mut keys: Vec<String> = keys.iter().cloned().collect();
    keys.sort();
    keys.dedup();

    // Only consider keys under dbt prefix and never in target/_versions.
    let base = ctx
        .keyspace
        .dbt_prefix(&ctx.scope)
        .trim_end_matches('/')
        .to_string();
    keys.retain(|k| {
        k.starts_with(&(base.clone() + "/"))
            && !k.contains("/target/")
            && !k.contains("/_versions/")
    });

    // Add core project context files if present (editable, but must be justified by the error).
    for rel in crate::data_engineer::project_files::CORE_PROJECT_CONTEXT_FILES.iter() {
        let k = format!("{}/{}", base, rel);
        if !keys.iter().any(|x| x == &k) {
            if ctx.storage.get_bytes(&k).await.is_ok() {
                keys.push(k);
            }
        }
    }
    keys.sort();
    keys.dedup();

    let scanned_files = keys.len();
    if scanned_files == 0 {
        return Ok(RemediationReport {
            dialect,
            phase: phase.to_string(),
            scanned_files,
            changed_files: 0,
            skipped: true,
            ..Default::default()
        });
    }

    // Load contents.
    let mut files: Vec<Value> = Vec::new();
    let mut content_by_key: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for k in keys.iter() {
        let bytes = ctx.storage.get_bytes(k).await.unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes).to_string();
        if text.trim().is_empty() {
            continue;
        }
        let rel_path = k
            .strip_prefix(&(base.clone() + "/"))
            .unwrap_or(k)
            .to_string();
        files.push(serde_json::json!({
            "key": k,
            "rel_path": rel_path,
            "content": text
        }));
        content_by_key.insert(
            k.clone(),
            files
                .last()
                .unwrap()
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        );
    }

    // Collect source schema facts for any source() calls we can detect.
    let mut sources_set: std::collections::BTreeSet<(String, String)> =
        std::collections::BTreeSet::new();
    for f in files.iter() {
        let Some(content) = f.get("content").and_then(|v| v.as_str()) else {
            continue;
        };
        for (src_schema, src_table) in
            crate::data_engineer::naming::extract_source_calls(content).into_iter()
        {
            if !src_schema.trim().is_empty() && !src_table.trim().is_empty() {
                sources_set.insert((src_schema, src_table));
            }
        }
    }

    let include_samples = errors_suggest_uncertainty(errors);
    let mut sources: Vec<Value> = Vec::new();
    for (source_schema, source_table) in sources_set.into_iter() {
        let schema_cols: Vec<Value> =
            best_effort_schema_columns_for_source(ctx, datasets, &source_schema, &source_table)
                .await
                .into_iter()
                .map(|(n, t)| serde_json::json!({"name": n, "type": t}))
                .collect();
        let samples = if include_samples {
            best_effort_samples_for_source(ctx, &source_schema, &source_table, 5).await
        } else {
            None
        };
        sources.push(serde_json::json!({
            "source_schema": source_schema,
            "source_table": source_table,
            "schema_columns": schema_cols,
            "data_samples": samples
        }));
    }

    // Preflight: for any ref() dependencies found in the provided files, attach live schema facts
    // (when the warehouse relations exist) so the LLM does not invent columns like event_timestamp/session_id.
    let mut ref_names: StdBTreeSet<String> = StdBTreeSet::new();
    for f in files.iter() {
        let Some(content) = f.get("content").and_then(|v| v.as_str()) else {
            continue;
        };
        for r in crate::data_engineer::naming::extract_ref_calls(content).into_iter() {
            ref_names.insert(r);
        }
    }
    let mut ref_models: Vec<Value> = Vec::new();
    for ref_name in ref_names.into_iter().take(15) {
        let Some(schema) = infer_schema_for_ref(errors, &ref_name) else {
            continue;
        };
        let fqn = format!("{}.{}.{}", catalog, schema, ref_name);
        let cols = best_effort_schema_columns_for_relation(ctx, &fqn).await;
        if cols.is_empty() {
            continue;
        }
        let schema_columns: Vec<Value> = cols
            .into_iter()
            .map(|(n, t)| serde_json::json!({"name": n, "type": t}))
            .collect();
        ref_models.push(serde_json::json!({
            "ref_name": ref_name,
            "relation": fqn,
            "schema_columns": schema_columns,
        }));
    }

    let error_brief = crate::data_engineer::dbt_error::compact_brief(errors, 8, 2400);

    let sys = format!(
        "You are a meticulous dbt auto-repair assistant.\n\
         Task: fix the provided dbt validation/build errors by proposing the smallest necessary edits.\n\
         Dialect: {dialect}\n\
         \n\
         IMMUTABLE FACTS:\n\
         - The warehouse schema facts in `sources[].schema_columns` are authoritative.\n\
         - The warehouse schema facts in `ref_models[].schema_columns` (when present) are authoritative for model outputs.\n\
         - If `sources[].data_samples` is present, treat it as ground truth evidence of real data values/types.\n\
         \n\
         CRITICAL RULES:\n\
         - Only fix the errors provided. Do NOT refactor, rename, or delete unrelated models/sources/macros.\n\
         - Prefer minimal edits (quote identifiers, adjust casts, use correct struct dereference) over rewrites.\n\
         - Do NOT invent tables/columns.\n\
         - If you are not absolutely sure the change is correct given the provided schema and data samples, return NO changes and explain what additional evidence would be required.\n\
         - Output MUST be valid JSON only (no markdown, no commentary).\n\
         Output schema:\n\
        {{\"changes\":[{{\"key\":\"...\",\"replace_file\":{{\"new_text\":\"...\"}}|null,\"replace_range\":{{\"start_line\":1,\"end_line\":1,\"new_text\":\"...\"}}|null,\"replace_list\":{{\"edits\":[{{\"start_line\":1,\"end_line\":1,\"new_text\":\"...\"}}]}}|null,\"reason\":\"...\"}}],\"notes\":[\"...\"]}}\n\
         Rules:\n\
        - For each change, choose EXACTLY ONE of replace_file / replace_range / replace_list (the others must be null).\n\
         - Do NOT include expected_sha256; the suite enforces drift safety from grounded file content.\n\
         Only include a file in changes if you actually modify it.\n"
    );

    let user = serde_json::json!({
        "phase": phase,
        "dbt_error_brief": error_brief,
        "errors": errors,
        "files": files,
        "sources": sources,
        "ref_models": ref_models
    })
    .to_string();

    let messages = vec![
        ChatMessage {
            role: "system".to_string(),
            content: sys.clone(),
        },
        ChatMessage {
            role: "user".to_string(),
            content: user.clone(),
        },
    ];
    let parts: Vec<PartInput> = vec![
        PartInput {
            name: "system".to_string(),
            text: sys.clone(),
        },
        PartInput {
            name: "user.phase".to_string(),
            text: phase.to_string(),
        },
        PartInput {
            name: "user.dbt_error_brief".to_string(),
            text: error_brief.clone(),
        },
        PartInput {
            name: "user.errors".to_string(),
            text: serde_json::to_string_pretty(&errors).unwrap_or_else(|_| "[]".to_string()),
        },
        PartInput {
            name: "user.files".to_string(),
            text: serde_json::to_string_pretty(&files).unwrap_or_else(|_| "[]".to_string()),
        },
        PartInput {
            name: "user.sources".to_string(),
            text: serde_json::to_string_pretty(&sources).unwrap_or_else(|_| "[]".to_string()),
        },
        PartInput {
            name: "user.ref_models".to_string(),
            text: serde_json::to_string_pretty(&ref_models).unwrap_or_else(|_| "[]".to_string()),
        },
    ];
    let call_opts = LlmCallOptions {
        prompt_id: "data_engineer.dbt_find_missing_sources",
        thread_id: ctx.thread_id.clone(),
        expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
        temperature: Some(0.0),
        top_p: Some(1.0),
        max_output_tokens: Some(1400),
        reasoning_effort: None,
    };
    let resp_text = match ctx.llm.chat(&messages, &call_opts) {
        Ok(t) => {
            record_llm_call_observability_async(ctx, phase, "unknown", &messages, &parts, &t, true)
                .await;
            t
        }
        Err(e) => {
            let raw = format!("LLM_ERROR: {}", e);
            record_llm_call_observability_async(
                ctx, phase, "unknown", &messages, &parts, &raw, false,
            )
            .await;
            return Err(format!("grounded dbt repair LLM call failed: {}", e));
        }
    };

    let v = parse_json_from_llm(&resp_text)?;
    let parsed: GroundedRepairResponse = serde_json::from_value(v)
        .map_err(|e| format!("failed to parse grounded repair JSON: {}", e))?;

    let mut report = RemediationReport {
        dialect: dialect.clone(),
        phase: phase.to_string(),
        scanned_files,
        ..Default::default()
    };
    for n in parsed.notes.iter() {
        if !n.trim().is_empty() {
            report.notes.push(n.clone());
        }
    }

    for ch in parsed.changes.into_iter() {
        if ch.key.trim().is_empty() {
            continue;
        }
        if !content_by_key.contains_key(&ch.key) {
            // Safety: only apply to provided keys.
            continue;
        }
        let rel = ch
            .key
            .strip_prefix(&(base.clone() + "/"))
            .ok_or_else(|| format!("remediation key not under dbt prefix: {}", ch.key))?
            .to_string();
        let existing = content_by_key.get(&ch.key).cloned().unwrap_or_default();
        let expected_base = sha256_hex(&existing);
        let mut provided = 0usize;
        if ch.replace_file.is_some() {
            provided += 1;
        }
        if ch.replace_range.is_some() {
            provided += 1;
        }
        if ch.replace_list.is_some() {
            provided += 1;
        }
        if provided != 1 {
            return Err(
                "grounded repair change must include exactly one of: replace_file | replace_range | replace_list"
                    .to_string(),
            );
        }
        let patch_text = if let Some(rf) = ch.replace_file.as_ref() {
            crate::data_engineer::project_fs::create_git_patch_text(
                &existing,
                &rf.new_text,
                &rel,
                true,
            )?
        } else if let Some(rr) = ch.replace_range.as_ref() {
            let new_text = crate::data_engineer::project_fs::apply_replace_range(
                &existing,
                rr.start_line,
                rr.end_line,
                &rr.new_text,
            )?;
            crate::data_engineer::project_fs::create_git_patch_text(
                &existing, &new_text, &rel, true,
            )?
        } else if let Some(rl) = ch.replace_list.as_ref() {
            let edits: Vec<crate::data_engineer::project_fs::ReplaceListEdit> = rl
                .edits
                .iter()
                .map(|e| crate::data_engineer::project_fs::ReplaceListEdit {
                    start_line: e.start_line,
                    end_line: e.end_line,
                    new_text: e.new_text.clone(),
                })
                .collect();
            let new_text =
                crate::data_engineer::project_fs::apply_replace_list(&existing, &edits)?;
            crate::data_engineer::project_fs::create_git_patch_text(
                &existing, &new_text, &rel, true,
            )?
        } else {
            return Err("invalid grounded repair change".to_string());
        };
        let outcome = crate::data_engineer::project_fs::apply_patch(
            ctx,
            None,
            &rel,
            &patch_text,
            Some(expected_base.as_str()),
            Some(true),
            crate::data_engineer::project_fs::PatchApplyKind::UnifiedDiff,
        )
        .await?;
        ctx.storage
            .put_bytes(&outcome.key, outcome.content.as_bytes(), "text/sql")
            .await?;

        report.changed_files += 1;
        report.changes.push(RemediationChange {
            key: ch.key,
            reason: ch.reason,
            changed: true,
        });
        report.diffs.push(RemediationDiff {
            key: outcome.key.clone(),
            rel_path: outcome.rel_path.clone(),
            base_sha256: outcome.base_sha256.clone(),
            new_sha256: outcome.new_sha256.clone(),
            lines_added: outcome.lines_added,
            lines_removed: outcome.lines_removed,
            diff: truncate_diff(&outcome.git_patch, 8_000),
        });
    }

    if report.changed_files > 0 {
        report
            .notes
            .push(format!("remediation_epoch_secs={}", now_epoch_secs()));
    }

    Ok(report)
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct LlmUnresolvedColumnsResponse {
    #[serde(default)]
    changes: Vec<LlmUnresolvedColumnsChange>,
    #[serde(default)]
    notes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct LlmUnresolvedColumnsChange {
    key: String,
    #[serde(default)]
    replace_file: Option<ReplaceFile>,
    #[serde(default)]
    replace_range: Option<ReplaceRange>,
    #[serde(default)]
    replace_list: Option<ReplaceList>,
    #[serde(default)]
    reason: Option<String>,
}

async fn best_effort_schema_columns_for_source(
    ctx: &AgentCtx,
    datasets: Option<&Arc<dyn DatasetCatalogProvider>>,
    source_schema: &str,
    source_table: &str,
) -> Vec<(String, String)> {
    let cfg = crate::config::resolved_config_from_ctx(ctx);
    let catalog = cfg
        .map(|c| c.providers.warehouse.container.clone())
        .unwrap_or_default();

    // Prefer the source warehouse provider (ground truth for the current connection).
    let ds_id = format!("{}.{}.{}", catalog, source_schema, source_table);
    if let Ok(cols) = ctx.warehouse.schema(&ds_id).await {
        return cols;
    }

    // Fall back to dataset catalog provider (may be cached/partial).
    if let Some(ds) = datasets {
        let did = DatasetId {
            catalog,
            database: source_schema.to_string(),
            table: source_table.to_string(),
        };
        if let Ok(cols) = ds.get_dataset_schema(&did).await {
            return cols;
        }
    }

    vec![]
}

pub async fn remediate_unresolved_columns_with_llm(
    ctx: &AgentCtx,
    phase: &str,
    errors: &[String],
    unresolved_columns: &[String],
    keys: &[String],
    datasets: Option<&Arc<dyn DatasetCatalogProvider>>,
) -> Result<RemediationReport, String> {
    let Some(cfg) = crate::config::resolved_config_from_ctx(ctx) else {
        return Ok(RemediationReport {
            dialect: "Unknown SQL dialect".to_string(),
            phase: phase.to_string(),
            scanned_files: 0,
            changed_files: 0,
            skipped: true,
            error: Some("resolved_config missing".to_string()),
            ..Default::default()
        });
    };
    let dialect = active_provider_dialect(cfg);
    let catalog = cfg.providers.warehouse.container.clone();

    let mut keys: Vec<String> = keys.iter().cloned().collect();
    keys.sort();
    keys.dedup();
    let scanned_files = keys.len();
    if scanned_files == 0 || unresolved_columns.is_empty() {
        return Ok(RemediationReport {
            dialect,
            phase: phase.to_string(),
            scanned_files,
            changed_files: 0,
            skipped: true,
            ..Default::default()
        });
    }

    // Only consider model SQL files under models/ (never target/ or versions).
    let keys: Vec<String> = keys
        .into_iter()
        .filter(|k| {
            k.contains("/models/")
                && k.ends_with(".sql")
                && !k.contains("/target/")
                && !k.contains("/_versions/")
        })
        .collect();

    // Load files and pre-filter to those that mention the unresolved token(s).
    let mut candidates: Vec<Value> = Vec::new();
    let mut content_by_key: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for k in keys.iter() {
        let bytes = ctx.storage.get_bytes(k).await.unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes).to_string();
        if text.trim().is_empty() {
            continue;
        }
        let mut hit = false;
        for col in unresolved_columns.iter() {
            let c = col.trim();
            if c.is_empty() {
                continue;
            }
            if text.contains(c) {
                hit = true;
                break;
            }
        }
        if !hit {
            continue;
        }

        let sources = crate::data_engineer::naming::extract_source_calls(&text);
        let (source_schema, source_table) = if sources.len() == 1 {
            (sources[0].0.clone(), sources[0].1.clone())
        } else {
            ("".to_string(), "".to_string())
        };

        let schema_cols: Vec<Value> = if !source_schema.is_empty() && !source_table.is_empty() {
            best_effort_schema_columns_for_source(ctx, datasets, &source_schema, &source_table)
                .await
                .into_iter()
                .map(|(n, t)| serde_json::json!({"name": n, "type": t}))
                .collect()
        } else {
            vec![]
        };

        candidates.push(serde_json::json!({
            "key": k,
            "content": text,
            "source_schema": source_schema,
            "source_table": source_table,
            "schema_columns": schema_cols
        }));
        content_by_key.insert(
            k.clone(),
            candidates
                .last()
                .unwrap()
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        );
    }

    if candidates.is_empty() {
        return Ok(RemediationReport {
            dialect,
            phase: phase.to_string(),
            scanned_files,
            changed_files: 0,
            skipped: true,
            ..Default::default()
        });
    }

    let error_brief = crate::data_engineer::dbt_error::compact_brief(errors, 6, 900);

    // Preflight: attach live schema facts for any ref() dependencies mentioned in candidate files.
    let mut ref_names: StdBTreeSet<String> = StdBTreeSet::new();
    for f in candidates.iter() {
        let Some(content) = f.get("content").and_then(|v| v.as_str()) else {
            continue;
        };
        for r in crate::data_engineer::naming::extract_ref_calls(content).into_iter() {
            ref_names.insert(r);
        }
    }
    let mut ref_models: Vec<Value> = Vec::new();
    for ref_name in ref_names.into_iter().take(15) {
        let Some(schema) = infer_schema_for_ref(errors, &ref_name) else {
            continue;
        };
        let fqn = format!("{}.{}.{}", catalog, schema, ref_name);
        let cols = best_effort_schema_columns_for_relation(ctx, &fqn).await;
        if cols.is_empty() {
            continue;
        }
        let schema_columns: Vec<Value> = cols
            .into_iter()
            .map(|(n, t)| serde_json::json!({"name": n, "type": t}))
            .collect();
        ref_models.push(serde_json::json!({
            "ref_name": ref_name,
            "relation": fqn,
            "schema_columns": schema_columns,
        }));
    }

    // Strict JSON-only contract. LLM returns structured patch primitives; we apply patches deterministically.
    let provider_dialect_rules = {
        let mut out = String::new();
        for rule in ctx.warehouse.sql_remediation_rules().into_iter() {
            out.push_str("         - ");
            out.push_str(rule);
            out.push('\n');
        }
        out
    };
    let sys = format!(
        "You are a meticulous dbt SQL auto-remediation assistant.\n\
         Task: fix unresolved column errors (e.g. Trino/Athena: Column 'x' cannot be resolved; BigQuery: Unrecognized name: x).\n\
         Dialect: {dialect}\n\
         Constraints:\n\
         - Only fix the unresolved column reference form; do not change business logic.\n\
         - Do not invent new tables/columns.\n\
         - If `ref_models[].schema_columns` is present for a ref()'d model, treat it as authoritative.\n\
         - If the unresolved column token contains dots and schema_columns contains an EXACT matching column name, treat it as a literal column name and quote it as a single identifier (e.g. \\\"context.session.id\\\").\n\
         - Only use struct dereference (e.g. context.session.id) when schema_columns indicate a struct/row parent exists AND there is no exact dotted column name.\n\
{provider_dialect_rules}\
         - Return ONLY valid JSON (no markdown, no commentary).\n\
        Output schema:\n\
         {{\"changes\":[{{\"key\":\"...\",\"replace_file\":{{\"new_text\":\"...\",\"expected_sha256\":\"...\"}}|null,\"replace_range\":{{\"start_line\":1,\"end_line\":1,\"new_text\":\"...\",\"expected_sha256\":\"...\"}}|null,\"replace_list\":{{\"edits\":[{{\"start_line\":1,\"end_line\":1,\"new_text\":\"...\"}}],\"expected_sha256\":\"...\"}}|null,\"reason\":\"...\"}}],\"notes\":[\"...\"]}}\n\
         Rules:\n\
         - For each change, choose EXACTLY ONE of replace_file / replace_range / replace_list (the others must be null).\n\
         - expected_sha256 MUST match the sha256 of the provided file content for that key.\n\
         Only include a file in changes if you actually modify it.\n",
        provider_dialect_rules = provider_dialect_rules
    );

    let user = serde_json::json!({
        "phase": phase,
        "dbt_error_brief": error_brief,
        "unresolved_columns": unresolved_columns,
        "files": candidates,
        "ref_models": ref_models
    })
    .to_string();

    let messages = vec![
        ChatMessage {
            role: "system".to_string(),
            content: sys.clone(),
        },
        ChatMessage {
            role: "user".to_string(),
            content: user.clone(),
        },
    ];
    let parts: Vec<PartInput> = vec![
        PartInput {
            name: "system".to_string(),
            text: sys.clone(),
        },
        PartInput {
            name: "user.phase".to_string(),
            text: phase.to_string(),
        },
        PartInput {
            name: "user.dbt_error_brief".to_string(),
            text: error_brief.clone(),
        },
        PartInput {
            name: "user.unresolved_columns".to_string(),
            text: serde_json::to_string_pretty(&unresolved_columns)
                .unwrap_or_else(|_| "[]".to_string()),
        },
        PartInput {
            name: "user.files".to_string(),
            text: serde_json::to_string_pretty(&candidates).unwrap_or_else(|_| "[]".to_string()),
        },
        PartInput {
            name: "user.ref_models".to_string(),
            text: serde_json::to_string_pretty(&ref_models).unwrap_or_else(|_| "[]".to_string()),
        },
    ];
    let call_opts = LlmCallOptions {
        prompt_id: "data_engineer.dbt_resolve_missing_columns",
        thread_id: ctx.thread_id.clone(),
        expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
        temperature: Some(0.0),
        top_p: Some(1.0),
        max_output_tokens: Some(1400),
        reasoning_effort: None,
    };
    let resp_text = match ctx.llm.chat(&messages, &call_opts) {
        Ok(t) => {
            record_llm_call_observability_async(ctx, phase, "unknown", &messages, &parts, &t, true)
                .await;
            t
        }
        Err(e) => {
            let raw = format!("LLM_ERROR: {}", e);
            record_llm_call_observability_async(
                ctx, phase, "unknown", &messages, &parts, &raw, false,
            )
            .await;
            return Err(format!(
                "unresolved-column remediation LLM call failed: {}",
                e
            ));
        }
    };

    let v = parse_json_from_llm(&resp_text)?;
    let parsed: LlmUnresolvedColumnsResponse = serde_json::from_value(v)
        .map_err(|e| format!("failed to parse unresolved-columns remediation JSON: {}", e))?;

    let mut report = RemediationReport {
        dialect: dialect.clone(),
        phase: phase.to_string(),
        scanned_files,
        ..Default::default()
    };
    for n in parsed.notes.iter() {
        if !n.trim().is_empty() {
            report.notes.push(n.clone());
        }
    }

    for ch in parsed.changes.into_iter() {
        if ch.key.trim().is_empty() {
            continue;
        }
        if !content_by_key.contains_key(&ch.key) {
            continue;
        }
        let base = ctx
            .keyspace
            .dbt_prefix(&ctx.scope)
            .trim_end_matches('/')
            .to_string();
        let rel = ch
            .key
            .strip_prefix(&(base.clone() + "/"))
            .ok_or_else(|| format!("remediation key not under dbt prefix: {}", ch.key))?
            .to_string();

        let existing = content_by_key.get(&ch.key).cloned().unwrap_or_default();
        let expected_base = sha256_hex(&existing);
        let mut provided = 0usize;
        if ch.replace_file.is_some() {
            provided += 1;
        }
        if ch.replace_range.is_some() {
            provided += 1;
        }
        if ch.replace_list.is_some() {
            provided += 1;
        }
        if provided != 1 {
            return Err(
                "unresolved-columns repair change must include exactly one of: replace_file | replace_range | replace_list"
                    .to_string(),
            );
        }
        let patch_text = if let Some(rf) = ch.replace_file.as_ref() {
            crate::data_engineer::project_fs::create_git_patch_text(
                &existing,
                &rf.new_text,
                &rel,
                true,
            )?
        } else if let Some(rr) = ch.replace_range.as_ref() {
            let new_text = crate::data_engineer::project_fs::apply_replace_range(
                &existing,
                rr.start_line,
                rr.end_line,
                &rr.new_text,
            )?;
            crate::data_engineer::project_fs::create_git_patch_text(
                &existing, &new_text, &rel, true,
            )?
        } else if let Some(rl) = ch.replace_list.as_ref() {
            let edits: Vec<crate::data_engineer::project_fs::ReplaceListEdit> = rl
                .edits
                .iter()
                .map(|e| crate::data_engineer::project_fs::ReplaceListEdit {
                    start_line: e.start_line,
                    end_line: e.end_line,
                    new_text: e.new_text.clone(),
                })
                .collect();
            let new_text =
                crate::data_engineer::project_fs::apply_replace_list(&existing, &edits)?;
            crate::data_engineer::project_fs::create_git_patch_text(
                &existing, &new_text, &rel, true,
            )?
        } else {
            return Err("invalid unresolved-columns repair change".to_string());
        };
        let outcome = crate::data_engineer::project_fs::apply_patch(
            ctx,
            None,
            &rel,
            &patch_text,
            Some(expected_base.as_str()),
            Some(true),
            crate::data_engineer::project_fs::PatchApplyKind::UnifiedDiff,
        )
        .await?;
        ctx.storage
            .put_bytes(&outcome.key, outcome.content.as_bytes(), "text/sql")
            .await?;

        report.changed_files += 1;
        report.changes.push(RemediationChange {
            key: ch.key,
            reason: ch.reason,
            changed: true,
        });
    }

    if report.changed_files > 0 {
        report
            .notes
            .push(format!("remediation_epoch_secs={}", now_epoch_secs()));
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use react_core::agent::DefaultPolicy;
    use react_core::keyspace::{DefaultKeyspace, Keyspace};
    use react_core::llm::LargeLanguageModel;
    use react_core::scope::RequestScope;
    use react_core::storage::{InMemoryStorageAdapter, StorageAdapter};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct MockLlm {
        // Queue of responses to return from chat()
        chat_responses: Mutex<Vec<String>>,
        calls: Mutex<usize>,
    }

    impl LargeLanguageModel for MockLlm {
        fn chat(
            &self,
            _messages: &[ChatMessage],
            _options: &react_core::llm::LlmCallOptions,
        ) -> Result<String, String> {
            let mut c = self.calls.lock().unwrap();
            *c += 1;
            let mut q = self.chat_responses.lock().unwrap();
            if q.is_empty() {
                return Err("no mock responses remaining".to_string());
            }
            Ok(q.remove(0))
        }
        fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            Ok(vec![])
        }
    }

    fn minimal_cfg_athena() -> Arc<ReactResolvedConfig> {
        Arc::new(ReactResolvedConfig {
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
                    namespace: "src".to_string(),
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
                        target_schema: "src".to_string(),
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

    fn make_ctx(storage: Arc<dyn StorageAdapter>, llm: Arc<dyn LargeLanguageModel>) -> AgentCtx {
        let scope = RequestScope {
            tenant: "t".to_string(),
            workspace: "w".to_string(),
            project_id: "p".to_string(),
        };
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
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
            llm,
            storage,
            scope: scope.clone(),
            keyspace,
            query: None,
            warehouse: Arc::new(react_core::providers::NullWarehouseProvider::default()),
            dbt: None,
            vector: None,
            thread_store: None,
            exec_ctx: None,
            runtime: Some(minimal_cfg_athena() as Arc<dyn std::any::Any + Send + Sync>),
        }
    }

    #[test]
    fn extract_run_model_schema_map_parses_model_lines() {
        let errors = vec![
            "12:00:00  1 of 2 START sql view model test_silver.stg_orders ........ [RUN]"
                .to_string(),
            "12:00:01  2 of 2 START sql view model test_gold.fct_orders .......... [RUN]"
                .to_string(),
            "12:00:02  1 of 2 OK created sql view model test_silver.stg_orders ... [OK]"
                .to_string(),
            "12:00:03  2 of 2 ERROR creating sql view model test_gold.fct_orders . [ERROR]"
                .to_string(),
        ];
        let map = super::extract_run_model_schema_map(&errors);
        assert_eq!(
            map.get("stg_orders").cloned(),
            Some("test_silver".to_string())
        );
        assert_eq!(
            map.get("fct_orders").cloned(),
            Some("test_gold".to_string())
        );
    }

    #[test]
    fn infer_schema_for_ref_prefers_direct_match() {
        let errors =
            vec!["1 of 1 START sql view model test_silver.stg_users .... [RUN]".to_string()];
        assert_eq!(
            super::infer_schema_for_ref(&errors, "stg_users"),
            Some("test_silver".to_string())
        );
    }

    #[tokio::test]
    async fn list_sql_keys_filters_target_and_versions() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let llm: Arc<dyn LargeLanguageModel> = Arc::new(MockLlm::default());
        let ctx = make_ctx(storage.clone(), llm);
        let base = ctx
            .keyspace
            .dbt_prefix(&ctx.scope)
            .trim_end_matches('/')
            .to_string();
        storage
            .put_bytes(&format!("{}/models/a.sql", base), b"select 1", "text/sql")
            .await
            .unwrap();
        storage
            .put_bytes(&format!("{}/target/manifest.sql", base), b"no", "text/sql")
            .await
            .unwrap();
        storage
            .put_bytes(
                &format!("{}/models/_versions/1.sql", base),
                b"no",
                "text/sql",
            )
            .await
            .unwrap();
        let keys = list_sql_keys_for_scope(&ctx).await.unwrap();
        assert_eq!(keys.len(), 1);
        assert!(keys[0].ends_with("/models/a.sql"));
    }

    #[tokio::test]
    async fn remediation_applies_llm_changes_to_storage() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let mock = MockLlm::default();
        *mock.chat_responses.lock().unwrap() = vec![serde_json::json!({
            "changes": [
                {"key":"t/w/p/dbt/models/m.sql","replace_file":{"new_text":"select 2","expected_sha256": sha256_hex("select 1")},"reason":"minimal"}
            ],
            "notes": ["ok"]
        })
        .to_string()];
        let llm: Arc<dyn LargeLanguageModel> = Arc::new(mock);
        let ctx = make_ctx(storage.clone(), llm);
        storage
            .put_bytes("t/w/p/dbt/models/m.sql", b"select 1", "text/sql")
            .await
            .unwrap();
        let rep = remediate_dbt_sql_with_llm(&ctx, "pre_validate")
            .await
            .unwrap();
        assert_eq!(rep.changed_files, 1);
        let bytes = storage.get_bytes("t/w/p/dbt/models/m.sql").await.unwrap();
        let got = String::from_utf8_lossy(&bytes);
        assert!(got.contains("select 2"));
        // Hard-cutover portability: do not inject `schema=` into model configs (dbt_project.yml governs schema).
        assert!(!got.contains("config(schema="));
    }
}
