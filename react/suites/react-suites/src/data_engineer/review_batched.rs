use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

use react_core::agent::AgentCtx;
use react_core::control_flow::{PhaseReasonCode, ReviewDecision, ReviewTier};
use react_core::llm::{ChatMessage, LlmCallOptions, LlmExpectedFormat, ReasoningEffort};
use react_core::session::{Observation, ThreadStep, ThreadStore};
use react_core::tools::Tool;

use crate::flow_frame::FlowFrame;
use crate::suite::SuiteCtx;

use super::control_flow::Phase;
use super::plan_kind::PlanKind;
use super::plan as de_plan;
use super::tools::dbt_files::DbtFilesTool;
use super::tools::json_file::JsonFileTool;
use super::tools::sql_schema::SqlSchemaTool;
use crate::data_engineer::{facts, naming};

const REVIEW_SNAPSHOT_VERSION: i64 = 1;
const DEFAULT_BATCH_SIZE: usize = 5;
const MAX_BATCHES_SAVED: usize = 40;
const MAX_NOTES_PER_BATCH: usize = 20;
const MAX_PROJECT_NOTES: usize = 40;
const MAX_SCHEMA_COLS_PER_ITEM: usize = 250;

#[derive(Clone, Debug)]
struct ProjectFile {
    path: String,
    content: String,
}

fn utc_ts() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn phase_plan_kind(phase: Phase) -> Option<PlanKind> {
    match phase {
        Phase::CleanseReview => Some(PlanKind::Cleanse),
        Phase::ModelReview => Some(PlanKind::Model),
        _ => None,
    }
}

fn clamp_lines(mut lines: Vec<String>, max: usize) -> Vec<String> {
    if lines.len() > max {
        lines.truncate(max);
    }
    lines
}

fn str_list(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .filter(|s| !s.trim().is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn take_head_tail(text: &str, max_chars: usize) -> String {
    if max_chars == 0 || text.len() <= max_chars {
        return text.to_string();
    }
    let head_chars = max_chars / 2;
    let tail_chars = max_chars - head_chars;
    let head = text.chars().take(head_chars).collect::<String>();
    let tail = text
        .chars()
        .rev()
        .take(tail_chars)
        .collect::<Vec<char>>()
        .into_iter()
        .rev()
        .collect::<String>();
    format!(
        "{head}\n\n... (truncated for review; total_chars={total}, showing head+tail)\n\n{tail}",
        head = head,
        tail = tail,
        total = text.len()
    )
}

/// Conservative JSON repair: escape raw control characters inside string literals.
///
/// Some providers occasionally emit JSON-like output containing literal newlines/tabs inside string
/// values, which is invalid JSON. This function repairs only those cases; it does not attempt to
/// fix truncation (EOF) or other structural issues.
fn escape_control_chars_in_json_strings(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    let mut in_str = false;
    let mut esc = false;
    for ch in s.chars() {
        if in_str {
            if esc {
                out.push(ch);
                esc = false;
                continue;
            }
            if ch == '\\' {
                out.push(ch);
                esc = true;
                continue;
            }
            match ch {
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '\u{08}' => out.push_str("\\b"),
                '\u{0C}' => out.push_str("\\f"),
                '"' => {
                    out.push(ch);
                    in_str = false;
                }
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                _ => out.push(ch),
            }
            continue;
        }
        if esc {
            out.push(ch);
            esc = false;
            continue;
        }
        match ch {
            '"' => {
                out.push(ch);
                in_str = true;
            }
            '\\' => {
                out.push(ch);
                esc = true;
            }
            _ => out.push(ch),
        }
    }
    out
}

async fn read_project_file(actx: &AgentCtx, path: &str, max_chars: usize) -> Option<ProjectFile> {
    let tool = DbtFilesTool { datasets: None };
    let obs = tool
        .call(
            serde_json::json!({"op":"get","path": path, "max_chars": 0}),
            actx,
        )
        .await
        .ok()?;
    if obs.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return None;
    }
    let content = obs
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Some(ProjectFile {
        path: path.to_string(),
        content: take_head_tail(&content, max_chars),
    })
}

async fn list_model_files(actx: &AgentCtx, prefix: &str, limit: usize) -> Vec<String> {
    let tool = DbtFilesTool { datasets: None };
    let obs = tool
        .call(
            serde_json::json!({"op":"list","prefix": prefix, "limit": (limit as u64).min(2000)}),
            actx,
        )
        .await
        .unwrap_or(Value::Null);
    if obs.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return Vec::new();
    }
    let mut out: Vec<String> = obs
        .get("items")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|it| {
                    it.get("path")
                        .and_then(|p| p.as_str())
                        .map(|s| s.to_string())
                })
                .filter(|p| p.ends_with(".sql"))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    out.sort();
    out.dedup();
    out
}

fn chunk_vec<T: Clone>(items: &[T], chunk_size: usize) -> Vec<Vec<T>> {
    let mut out: Vec<Vec<T>> = Vec::new();
    if chunk_size == 0 {
        return out;
    }
    let mut i = 0usize;
    while i < items.len() {
        let end = (i + chunk_size).min(items.len());
        out.push(items[i..end].to_vec());
        i = end;
    }
    out
}

async fn append_review_step(
    store: &ThreadStore,
    thread_id: &str,
    phase: Phase,
    agent: &str,
    code: PhaseReasonCode,
    detail: Value,
) -> Result<(), String> {
    store
        .append_step(
            thread_id,
            ThreadStep::Phase {
                phase: phase.as_str().to_string(),
                from_phase: Some(phase.as_str().to_string()),
                reason_code: Some(code),
                reason_detail: Some(detail),
                observation: Observation::ok(),
                ts: utc_ts(),
                agent: agent.to_string(),
            },
        )
        .await
        .map_err(|e| format!("failed to append review step: {e}"))?;
    Ok(())
}

fn system_prompt_for_summary(plan_kind: Option<PlanKind>) -> String {
    if plan_kind == Some(PlanKind::Cleanse) {
        return r#"You are a read-only reviewer for a DBT analytics project.

You will be given:
- The original goal and review context (brief)
- A small, deterministic project snapshot (dbt_project.yml, sources list, model file index, and minimal manifest metadata)

Output one JSON object with this schema:
{
  "project_notes": [string, ...],
  "project_risks": [string, ...]
}

Rules:
- Be pragmatic, not pedantic. Focus on business correctness and usability.
- Do NOT suggest edits in-line; just describe risks/gaps.
- Keep notes concise and high-signal.
- Tier focus (CRITICAL): this is a SILVER (cleanse) review. Do NOT penalize missing GOLD models.
- Insightfulness check (CRITICAL):
  - Call out whether the SILVER layer is usable as a stable, row-preserving cleanse foundation (explicit fields, safe casting, quality flags, stable naming).
  - If you mention GOLD at all, frame it as an optional future improvement, not a blocker."#
            .to_string();
    }
    r#"You are a read-only reviewer for a DBT analytics project.

You will be given:
- The original goal and review context (brief)
- A small, deterministic project snapshot (dbt_project.yml, sources list, model file index, and minimal manifest metadata)

Output one JSON object with this schema:
{
  "project_notes": [string, ...],
  "project_risks": [string, ...]
}

Rules:
- Be pragmatic, not pedantic. Focus on business correctness and usability.
- Do NOT suggest edits in-line; just describe risks/gaps.
- Keep notes concise and high-signal.
- Insightfulness check (CRITICAL):
  - Call out whether the current GOLD layer enables meaningful business decisions (not just technically-correct SQL).
  - If GOLD is present but naive, list the top 2 missing “business semantics” gaps (definitions, time axis, entity meaning, join contracts) that block real analytics."#
        .to_string()
}

fn system_prompt_for_batch(plan_kind: Option<PlanKind>) -> String {
    if plan_kind == Some(PlanKind::Cleanse) {
        return r#"You are a read-only reviewer for a DBT analytics project.

You will be given:
- The original goal and review context
- A batch of items (datasets or model names)
- The expected model file paths and their contents (bounded)
- Any invariants/notes from planning
- The authoritative schema (columns/types) for each dataset in the batch (when available)

Output one JSON object with this schema:
{
  "notes": [string, ...],
  "actionable_hints": [string, ...]
}

Rules:
- CRITICAL: The planning artifacts you receive (invariants/notes and any implementation_spec) are the authoritative design contract for this phase.
  - Your primary job is CONFORMANCE REVIEW: does the SQL/YAML implement the provided implementation_spec and obey prohibited_ops?
  - Do NOT propose changing the contract as part of review. If you believe the contract itself is wrong/ambiguous, record it as a "requires plan change" note (see below) but DO NOT propose an implementation change that deviates from the contract.
- Conformance-first ordering:
  1) Identify any plan/spec conformance violations (blockers). These are always high-signal.
  2) Then (optionally) include at most one additional high-value business-risk observation that does NOT require changing the plan/spec.
- If you think a finding requires changing the plan/spec, label it explicitly with prefix:
  - "REQUIRES PLAN CHANGE: ..."
  and do NOT include an implementation hint for it.
- High-signal only: do NOT cover every batch item. Report only blocker/high business-risk findings or one small, clearly high-value quick win.
- If no high-value findings exist for this batch, return:
  - "notes": []
  - "actionable_hints": []
- Hard cap: at most 3 findings in "notes" total.
- Keep "actionable_hints" tightly scoped to the findings: at most 1 hint per finding (max 3 total).
- Prefer concrete feedback tied to specific models/columns when visible.
- IMPORTANT: Do NOT suggest adding/selecting fields that are not present in the provided authoritative schema.
  If a desired field is missing from the schema, call that out as a gap and suggest the nearest available alternative.
- Tier focus (CRITICAL): this is a SILVER (cleanse) review. Do NOT critique missing GOLD models.
- Each finding must be decision-oriented and include:
  - Impacted metric/decision.
  - Concrete evidence from provided SQL/schema.
  - Smallest next action to reduce risk.
- No tool calls and no file edits."#
            .to_string();
    }
    r#"You are a read-only reviewer for a DBT analytics project.

You will be given:
- The original goal and review context
- A batch of items (datasets or model names)
- The expected model file paths and their contents (bounded)
- Any invariants/notes from planning
- The authoritative schema (columns/types) for each dataset in the batch (when available)

Output one JSON object with this schema:
{
  "notes": [string, ...],
  "actionable_hints": [string, ...]
}

Rules:
- CRITICAL: The planning artifacts you receive (invariants/notes and any implementation_spec) are the authoritative design contract for this phase.
  - Your primary job is CONFORMANCE REVIEW: does the SQL/YAML implement the provided implementation_spec and obey prohibited_ops?
  - Do NOT propose changing the contract as part of review. If you believe the contract itself is wrong/ambiguous, record it as a "requires plan change" note (see below) but DO NOT propose an implementation change that deviates from the contract.
- Conformance-first ordering:
  1) Identify any plan/spec conformance violations (blockers). These are always high-signal.
  2) Then (optionally) include at most one additional high-value business-risk observation that does NOT require changing the plan/spec.
- If you think a finding requires changing the plan/spec, label it explicitly with prefix:
  - "REQUIRES PLAN CHANGE: ..."
  and do NOT include an implementation hint for it.
- High-signal only: do NOT cover every batch item. Report only blocker/high business-risk findings or one small, clearly high-value quick win.
- If no high-value findings exist for this batch, return:
  - "notes": []
  - "actionable_hints": []
- Hard cap: at most 3 findings in "notes" total.
- Keep "actionable_hints" tightly scoped to the findings: at most 1 hint per finding (max 3 total).
- Prefer concrete feedback tied to specific models/columns when visible.
- IMPORTANT: Do NOT suggest adding/selecting fields that are not present in the provided authoritative schema.
  If a desired field is missing from the schema, call that out as a gap and suggest the nearest available alternative.
- Each finding must be decision-oriented and include:
  - Impacted metric/decision.
  - Concrete evidence from provided SQL/schema.
  - Smallest next action to reduce risk.
- No tool calls and no file edits."#
        .to_string()
}

fn system_prompt_for_unify(plan_kind: Option<PlanKind>) -> String {
    if plan_kind == Some(PlanKind::Cleanse) {
        return r#"You are a read-only reviewer for a DBT analytics project.

You will be given:
- The original goal and review context
- Project-level notes/risks
- Notes from ALL review batches

You must output one JSON object with this schema:
{
  "decision": "proceed" | "patch_plan" | "patch_impl",
  "tier": "silver" | "gold" | "unknown",
  "dataset_ids": [string],
  "final_review_text": string
}

Interpretation rules (CRITICAL):
- decision="proceed" means: no action required now; the implementation conforms and there are no net-new/still-unresolved high-value issues.
- decision="patch_impl" means: a concrete implementation change is required NOW to match the approved plan/spec (conformance/correctness fix), without changing the plan/spec.
- decision="patch_plan" means: the plan/spec is wrong or ambiguous and must be revised before implementation can proceed safely.
  - If decision="patch_plan", final_review_text MUST be limited to describing the plan defect and the smallest fix to the plan/spec.
  - Do NOT propose implementation edits that deviate from the current plan/spec.

Tier rules: this is a SILVER (cleanse) review. Set tier="silver".

Unify requirements (CRITICAL):
- Produce a concise, business-focused review that prioritizes decision usefulness.
- Delta-first output: include only net-new or still-unresolved high-value issues since prior review context. Suppress repeated advice that has no meaningful change in evidence or priority.
- If no net-new/still-unresolved blocker/high items exist, set decision="proceed" and keep the body brief.
- Hard cap: list at most 5 issues total across the final review body.
- Include a short “Insightfulness summary” section:
  - What operational/analytical use-cases the SILVER layer supports today.
  - The top 2 missing semantics gaps (definitions, time axis meaning, entity meaning, join contracts, quality flags) blocking higher-value analysis even at SILVER.
- Include an “Assumptions & evidence gaps” section listing the most important semantic assumptions and the smallest probes to validate them."#
            .to_string();
    }
    r#"You are a read-only reviewer for a DBT analytics project.

You will be given:
- The original goal and review context
- Project-level notes/risks
- Notes from ALL review batches

You must output one JSON object with this schema:
{
  "decision": "proceed" | "patch_plan" | "patch_impl",
  "tier": "silver" | "gold" | "unknown",
  "dataset_ids": [string],
  "final_review_text": string
}

Interpretation rules (CRITICAL):
- decision="proceed" means: no action required now; the implementation conforms and there are no net-new/still-unresolved high-value issues.
- decision="patch_impl" means: a concrete implementation change is required NOW to match the approved plan/spec (conformance/correctness fix), without changing the plan/spec.
- decision="patch_plan" means: the plan/spec is wrong or ambiguous and must be revised before implementation can proceed safely.
  - If decision="patch_plan", final_review_text MUST be limited to describing the plan defect and the smallest fix to the plan/spec.
  - Do NOT propose implementation edits that deviate from the current plan/spec.

Tier rules: this is a GOLD/model review. Set tier="gold".

Unify requirements (CRITICAL):
- Produce a concise, business-focused review that prioritizes decision usefulness.
- Delta-first output: include only net-new or still-unresolved high-value issues since prior review context. Suppress repeated advice that has no meaningful change in evidence or priority.
- If no net-new/still-unresolved blocker/high items exist, set decision="proceed" and keep the body brief.
- Hard cap: list at most 5 issues total across the final review body.
- Include a short “Insightfulness summary” section:
  - What business decisions the current GOLD layer enables today.
  - The top 2 missing business semantics gaps blocking higher-value analytics (definitions, time axis, entity meaning, join contracts).
- Include an “Assumptions & evidence gaps” section listing the most important semantic assumptions and the smallest probes to validate them."#
        .to_string()
}

async fn llm_json(
    ctx: &SuiteCtx,
    _actx: &AgentCtx,
    thread_id: &str,
    phase: Phase,
    name: &str,
    user: String,
) -> Result<Value, String> {
    fn parse_review_reasoning_effort(var: &str) -> Option<ReasoningEffort> {
        match std::env::var(var)
            .ok()
            .map(|s| s.trim().to_lowercase())
            .as_deref()
        {
            Some("none") => Some(ReasoningEffort::None),
            Some("low") => Some(ReasoningEffort::Low),
            Some("medium") => Some(ReasoningEffort::Medium),
            Some("high") => Some(ReasoningEffort::High),
            _ => None,
        }
    }

    // Default to MEDIUM for reviews: we do want some reasoning, but Responses output_tokens includes
    // reasoning tokens, so max_output_tokens must be high enough to still emit the JSON payload.
    let review_reasoning_effort = parse_review_reasoning_effort("LLM_REVIEW_REASONING_EFFORT")
        .unwrap_or(ReasoningEffort::Medium);

    fn parse_u32_env(key: &str) -> Option<u32> {
        std::env::var(key)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
    }

    // Provide ample headroom by default. Review prompts are structured JSON and truncation is costly.
    // Env knobs (optional):
    // - LLM_REVIEW_MAX_TOKENS (global default for non-unify)
    // - LLM_REVIEW_MAX_TOKENS_UNIFY (global default for unify)
    // - LLM_REVIEW_MAX_TOKENS_CLEANSE / _MODEL / _POSTPUBLISH
    // - LLM_REVIEW_MAX_TOKENS_CLEANSE_UNIFY / _MODEL_UNIFY / _POSTPUBLISH_UNIFY
    let default_non_unify: u32 = parse_u32_env("LLM_REVIEW_MAX_TOKENS")
        .unwrap_or(12_000)
        .max(2_000)
        .min(32_000);
    let default_unify: u32 = parse_u32_env("LLM_REVIEW_MAX_TOKENS_UNIFY")
        .unwrap_or(18_000)
        .max(2_000)
        .min(32_000);
    let is_unify = name == "unify";
    let phase_budget: u32 = match phase {
        Phase::CleanseReview => {
            let k = if is_unify {
                "LLM_REVIEW_MAX_TOKENS_CLEANSE_UNIFY"
            } else {
                "LLM_REVIEW_MAX_TOKENS_CLEANSE"
            };
            parse_u32_env(k).unwrap_or(if is_unify {
                default_unify
            } else {
                default_non_unify
            })
        }
        Phase::ModelReview => {
            let k = if is_unify {
                "LLM_REVIEW_MAX_TOKENS_MODEL_UNIFY"
            } else {
                "LLM_REVIEW_MAX_TOKENS_MODEL"
            };
            parse_u32_env(k).unwrap_or(if is_unify {
                default_unify
            } else {
                default_non_unify
            })
        }
        Phase::PostPublishReview => {
            let k = if is_unify {
                "LLM_REVIEW_MAX_TOKENS_POSTPUBLISH_UNIFY"
            } else {
                "LLM_REVIEW_MAX_TOKENS_POSTPUBLISH"
            };
            parse_u32_env(k).unwrap_or(if is_unify {
                default_unify
            } else {
                default_non_unify
            })
        }
        _ => {
            if is_unify {
                default_unify
            } else {
                default_non_unify
            }
        }
    }
    .max(2_000)
    .min(32_000);

    let prompt_id: &'static str = match (phase, name) {
        (Phase::CleanseReview, "summary") => "data_engineer.cleanse_review.summary",
        (Phase::CleanseReview, "batch") => "data_engineer.cleanse_review.batch",
        (Phase::CleanseReview, "unify") => "data_engineer.cleanse_review.unify",
        (Phase::ModelReview, "summary") => "data_engineer.model_review.summary",
        (Phase::ModelReview, "batch") => "data_engineer.model_review.batch",
        (Phase::ModelReview, "unify") => "data_engineer.model_review.unify",
        (Phase::PostPublishReview, "summary") => "data_engineer.post_publish_review.summary",
        (Phase::PostPublishReview, "batch") => "data_engineer.post_publish_review.batch",
        (Phase::PostPublishReview, "unify") => "data_engineer.post_publish_review.unify",
        // Fallback: keep stable even if new names are introduced.
        (Phase::CleanseReview, _) => "data_engineer.cleanse_review.other",
        (Phase::ModelReview, _) => "data_engineer.model_review.other",
        (Phase::PostPublishReview, _) => "data_engineer.post_publish_review.other",
        _ => "data_engineer.review.other_phase",
    };

    let system_prompt = match name {
        "summary" => system_prompt_for_summary(phase_plan_kind(phase)),
        "batch" => system_prompt_for_batch(phase_plan_kind(phase)),
        _ => system_prompt_for_unify(phase_plan_kind(phase)),
    };

    // Dynamic sizing: bigger review contexts need bigger output budgets, otherwise Responses truncates
    // the JSON payload after spending many tokens on reasoning.
    //
    // Heuristic: estimate input tokens from chars and add a fixed cushion depending on call kind.
    // Use a conservative chars->tokens heuristic. `usage.input_tokens` is frequently closer to
    // chars/2 (not chars/4) once we include large JSON payloads, identifiers, and schema text.
    let approx_in_tokens: u32 = ((system_prompt.len() + user.len()) as u32 / 2).max(1);
    let extra_out_tokens: u32 = match name {
        "summary" => 6_000,
        "batch" => 12_000,
        _ => 16_000, // unify
    };
    let dynamic_budget: u32 = approx_in_tokens
        .saturating_add(extra_out_tokens)
        .max(2_000)
        .min(32_000);
    let phase_budget: u32 = phase_budget.max(dynamic_budget).min(32_000);

    // Tune output budgets by (phase, call kind). Unify frequently needs a bit more headroom because it
    // returns a single `final_review_text` field which can be longer than the per-batch notes.
    let llm_options = match phase {
        Phase::CleanseReview => Some(LlmCallOptions {
            prompt_id,
            thread_id: Some(thread_id.to_string()),
            expected_format: LlmExpectedFormat::JsonObject,
            temperature: Some(0.15),
            top_p: Some(1.0),
            max_output_tokens: Some(phase_budget),
            reasoning_effort: Some(review_reasoning_effort),
        }),
        Phase::ModelReview => Some(LlmCallOptions {
            prompt_id,
            thread_id: Some(thread_id.to_string()),
            expected_format: LlmExpectedFormat::JsonObject,
            temperature: Some(0.25),
            top_p: Some(1.0),
            max_output_tokens: Some(phase_budget),
            reasoning_effort: Some(review_reasoning_effort),
        }),
        Phase::PostPublishReview => Some(LlmCallOptions {
            prompt_id,
            thread_id: Some(thread_id.to_string()),
            expected_format: LlmExpectedFormat::JsonObject,
            temperature: Some(0.20),
            top_p: Some(1.0),
            max_output_tokens: Some(phase_budget),
            reasoning_effort: Some(review_reasoning_effort),
        }),
        _ => None,
    };

    let messages = vec![
        ChatMessage {
            role: "system".to_string(),
            content: system_prompt,
        },
        ChatMessage {
            role: "user".to_string(),
            content: user,
        },
    ];

    // Persist LLM observability via core module when enabled.
    // Best-effort: we log the full messages as prompt parts (system+user).
    let store = ThreadStore::new(ctx.storage.clone(), ctx.scope.clone(), ctx.keyspace.clone());
    if react_core::llm_observability::llm_calls_enabled() {
        let call_id = react_core::llm_observability::next_call_id(thread_id);
        let prompt_hash = react_core::llm_observability::prompt_hash_for_messages(&messages);
        let built = react_core::llm_observability::build_parts_for_thread(
            thread_id,
            &[
                react_core::llm_observability::PartInput {
                    name: format!("review.{}.system", name),
                    text: messages[0].content.clone(),
                },
                react_core::llm_observability::PartInput {
                    name: format!("review.{}.user", name),
                    text: messages[1].content.clone(),
                },
            ],
        );

        // We append the llm_call step after we have response_text below.
        let mut opts = llm_options.unwrap_or_else(|| {
            react_core::llm::LlmCallOptions::new(
                "data_engineer.review_batched.llm_json",
                LlmExpectedFormat::JsonObject,
            )
        });
        opts.expected_format = LlmExpectedFormat::JsonObject;
        let res = ctx.llm.chat(&messages, &opts);
        let (ok, raw) = match &res {
            Ok(t) => (true, t.clone()),
            Err(e) => (false, format!("LLM_ERROR: {}", e)),
        };
        let response_hash = react_core::llm_observability::sha256_hex_str(&raw);
        let response_text = if react_core::llm_observability::llm_response_text_enabled() {
            Some(react_core::llm_observability::redact_common_secrets(&raw))
        } else {
            None
        };
        let agent = "review".to_string();
        let ts = utc_ts();
        let _ = store
            .append_step(
                thread_id,
                ThreadStep::LlmCall {
                    call_id,
                    model: "unknown".to_string(),
                    phase: phase.as_str().to_string(),
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
                    ts,
                    agent,
                },
            )
            .await;
        if !ok {
            return Err(raw);
        }
        // Be robust to rare truncation/control-char issues: try a conservative repair and a single retry
        // with a tighter "JSON only" reminder + higher max_output_tokens (for unify).
        match serde_json::from_str::<Value>(&raw) {
            Ok(v) => return Ok(v),
            Err(e1) => {
                let repaired = escape_control_chars_in_json_strings(&raw);
                if let Ok(v) = serde_json::from_str::<Value>(&repaired) {
                    return Ok(v);
                }

                let mut retry_messages = messages.clone();
                retry_messages.push(ChatMessage {
                    role: "user".to_string(),
                    content: "IMPORTANT: Return ONLY a single JSON object. Keep all strings concise; if `final_review_text` would be long, summarize and cap it. No markdown, no code fences.".to_string(),
                });
                let mut retry_opts = opts;
                if name == "unify" {
                    retry_opts.max_output_tokens =
                        Some(retry_opts.max_output_tokens.unwrap_or(0).max(3600));
                }
                let raw2 = ctx
                    .llm
                    .chat(&retry_messages, &retry_opts)
                    .map_err(|e| e.to_string())?;
                return serde_json::from_str::<Value>(&raw2).map_err(|e2| {
                    format!(
                        "review {}: expected JSON, got parse error: {} (first_error: {})",
                        name, e2, e1
                    )
                });
            }
        }
    }

    let mut opts = llm_options.unwrap_or_else(|| {
        react_core::llm::LlmCallOptions::new(
            "data_engineer.review_batched.llm_json",
            LlmExpectedFormat::JsonObject,
        )
    });
    opts.expected_format = LlmExpectedFormat::JsonObject;
    let raw = ctx.llm.chat(&messages, &opts).map_err(|e| e.to_string())?;
    match serde_json::from_str::<Value>(&raw) {
        Ok(v) => Ok(v),
        Err(e1) => {
            let repaired = escape_control_chars_in_json_strings(&raw);
            if let Ok(v) = serde_json::from_str::<Value>(&repaired) {
                return Ok(v);
            }
            // One retry (same idea as observability branch, minus logging).
            let mut retry_messages = messages.clone();
            retry_messages.push(ChatMessage {
                role: "user".to_string(),
                content: "IMPORTANT: Return ONLY a single JSON object. Keep all strings concise. No markdown, no code fences.".to_string(),
            });
            let mut retry_opts = opts;
            if name == "unify" {
                retry_opts.max_output_tokens =
                    Some(retry_opts.max_output_tokens.unwrap_or(0).max(3600));
            }
            let raw2 = ctx
                .llm
                .chat(&retry_messages, &retry_opts)
                .map_err(|e| e.to_string())?;
            serde_json::from_str::<Value>(&raw2).map_err(|e2| {
                format!(
                    "review {}: expected JSON, got parse error: {} (first_error: {})",
                    name, e2, e1
                )
            })
        }
    }
}

async fn load_global_semantic_context_json(actx: &AgentCtx, sctx: &SuiteCtx) -> serde_json::Value {
    let key = sctx.keyspace.semantic_key(
        &sctx.scope,
        react_core::providers::catalog::types::GLOBAL_SEMANTIC_DATASET_ID,
    );
    actx.storage
        .get_json(&key)
        .await
        .ok()
        .unwrap_or(Value::Null)
}

fn include_global_semantic_context(phase: Phase) -> bool {
    // Global semantic context is intended for GOLD model planning/review and post-publish review only.
    // It must not be injected into cleanse (silver) prompts.
    matches!(phase, Phase::ModelReview | Phase::PostPublishReview)
}

fn upsert_review_snapshot(obj: &mut serde_json::Map<String, Value>, patch: Value) {
    // Store under project_snapshot.review (bounded).
    let review = obj.entry("review").or_insert_with(|| serde_json::json!({}));
    if review.is_null() {
        *review = serde_json::json!({});
    }
    let review_obj = review.as_object_mut().unwrap();
    // Merge patch keys shallowly.
    if let Some(patch_obj) = patch.as_object() {
        for (k, v) in patch_obj.iter() {
            review_obj.insert(k.clone(), v.clone());
        }
    }
    review_obj.insert(
        "review_version".to_string(),
        serde_json::json!(REVIEW_SNAPSHOT_VERSION),
    );
}

fn push_batch_entry(review_obj: &mut serde_json::Map<String, Value>, entry: Value) {
    let batches = review_obj
        .entry("batches")
        .or_insert_with(|| serde_json::Value::Array(vec![]));
    let Some(arr) = batches.as_array_mut() else {
        return;
    };
    arr.push(entry);
    while arr.len() > MAX_BATCHES_SAVED {
        arr.remove(0);
    }
}

async fn persist_review_summary_to_plan(
    actx: &AgentCtx,
    phase: Phase,
    plan_kind: PlanKind,
    plan_key: &str,
    project_notes: Vec<String>,
    project_risks: Vec<String>,
) -> Result<(), String> {
    let ts = utc_ts();
    if plan_kind == PlanKind::Cleanse {
        if let Some(mut p) = de_plan::load_cleanse_plan_by_key(actx, plan_key).await {
            if p.project_snapshot.is_null() {
                p.project_snapshot = serde_json::json!({});
            }
            if let Some(obj) = p.project_snapshot.as_object_mut() {
                upsert_review_snapshot(
                    obj,
                    serde_json::json!({
                        "phase": phase.as_str(),
                        "project_notes": project_notes,
                        "project_risks": project_risks,
                        "ts": ts,
                    }),
                );
            }
            de_plan::save_cleanse_plan(actx, &p)
                .await
                .map_err(|e| format!("failed to persist cleanse review summary: {e}"))?;
        }
    } else if plan_kind == PlanKind::Model {
        if let Some(mut p) = de_plan::load_model_plan_by_key(actx, plan_key).await {
            if p.project_snapshot.is_null() {
                p.project_snapshot = serde_json::json!({});
            }
            if let Some(obj) = p.project_snapshot.as_object_mut() {
                upsert_review_snapshot(
                    obj,
                    serde_json::json!({
                        "phase": phase.as_str(),
                        "project_notes": project_notes,
                        "project_risks": project_risks,
                        "ts": ts,
                    }),
                );
            }
            de_plan::save_model_plan(actx, &p)
                .await
                .map_err(|e| format!("failed to persist model review summary: {e}"))?;
        }
    }
    Ok(())
}

async fn persist_review_batch_to_plan(
    actx: &AgentCtx,
    plan_kind: PlanKind,
    plan_key: &str,
    batch_idx: usize,
    batch_items: Vec<String>,
    notes: Vec<String>,
) -> Result<(), String> {
    let ts = utc_ts();
    let entry = serde_json::json!({
        "batch_idx": batch_idx,
        "batch_items": batch_items,
        "notes": notes,
        "ts": ts,
    });
    if plan_kind == PlanKind::Cleanse {
        if let Some(mut p) = de_plan::load_cleanse_plan_by_key(actx, plan_key).await {
            if p.project_snapshot.is_null() {
                p.project_snapshot = serde_json::json!({});
            }
            if let Some(obj) = p.project_snapshot.as_object_mut() {
                let review = obj.entry("review").or_insert_with(|| serde_json::json!({}));
                if review.is_null() {
                    *review = serde_json::json!({});
                }
                let review_obj = review.as_object_mut().unwrap();
                review_obj.insert(
                    "review_version".to_string(),
                    serde_json::json!(REVIEW_SNAPSHOT_VERSION),
                );
                push_batch_entry(review_obj, entry);
            }
            de_plan::save_cleanse_plan(actx, &p)
                .await
                .map_err(|e| format!("failed to persist cleanse review batch: {e}"))?;
        }
    } else if plan_kind == PlanKind::Model {
        if let Some(mut p) = de_plan::load_model_plan_by_key(actx, plan_key).await {
            if p.project_snapshot.is_null() {
                p.project_snapshot = serde_json::json!({});
            }
            if let Some(obj) = p.project_snapshot.as_object_mut() {
                let review = obj.entry("review").or_insert_with(|| serde_json::json!({}));
                if review.is_null() {
                    *review = serde_json::json!({});
                }
                let review_obj = review.as_object_mut().unwrap();
                review_obj.insert(
                    "review_version".to_string(),
                    serde_json::json!(REVIEW_SNAPSHOT_VERSION),
                );
                push_batch_entry(review_obj, entry);
            }
            de_plan::save_model_plan(actx, &p)
                .await
                .map_err(|e| format!("failed to persist model review batch: {e}"))?;
        }
    }
    Ok(())
}

async fn persist_review_final_to_plan(
    actx: &AgentCtx,
    plan_kind: PlanKind,
    plan_key: &str,
    decision: ReviewDecision,
    tier: ReviewTier,
    dataset_ids: Vec<String>,
    text: String,
) -> Result<(), String> {
    let ts = utc_ts();
    if plan_kind == PlanKind::Cleanse {
        if let Some(mut p) = de_plan::load_cleanse_plan_by_key(actx, plan_key).await {
            if p.project_snapshot.is_null() {
                p.project_snapshot = serde_json::json!({});
            }
            if let Some(obj) = p.project_snapshot.as_object_mut() {
                let review = obj.entry("review").or_insert_with(|| serde_json::json!({}));
                if review.is_null() {
                    *review = serde_json::json!({});
                }
                let review_obj = review.as_object_mut().unwrap();
                review_obj.insert(
                    "review_version".to_string(),
                    serde_json::json!(REVIEW_SNAPSHOT_VERSION),
                );
                review_obj.insert(
                    "final".to_string(),
                    serde_json::json!({
                        "decision": decision,
                        "tier": tier,
                        "dataset_ids": dataset_ids,
                        "text": text,
                        "ts": ts
                    }),
                );
            }
            de_plan::save_cleanse_plan(actx, &p)
                .await
                .map_err(|e| format!("failed to persist cleanse review final: {e}"))?;
        }
    } else if plan_kind == PlanKind::Model {
        if let Some(mut p) = de_plan::load_model_plan_by_key(actx, plan_key).await {
            if p.project_snapshot.is_null() {
                p.project_snapshot = serde_json::json!({});
            }
            if let Some(obj) = p.project_snapshot.as_object_mut() {
                let review = obj.entry("review").or_insert_with(|| serde_json::json!({}));
                if review.is_null() {
                    *review = serde_json::json!({});
                }
                let review_obj = review.as_object_mut().unwrap();
                review_obj.insert(
                    "review_version".to_string(),
                    serde_json::json!(REVIEW_SNAPSHOT_VERSION),
                );
                review_obj.insert(
                    "final".to_string(),
                    serde_json::json!({
                        "decision": decision,
                        "tier": tier,
                        "dataset_ids": dataset_ids,
                        "text": text,
                        "ts": ts
                    }),
                );
            }
            de_plan::save_model_plan(actx, &p)
                .await
                .map_err(|e| format!("failed to persist model review final: {e}"))?;
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewUnifyOutput {
    decision: ReviewDecision,
    #[serde(default)]
    tier: ReviewTier,
    #[serde(default)]
    dataset_ids: Vec<String>,
    final_review_text: String,
}

fn resolve_cleanse_batch_paths(
    plan: &de_plan::CleansePlan,
    batch: &[String],
) -> Vec<(
    String,
    String,
    Vec<String>,
    Vec<de_plan::PlanChecklistItem>,
    Option<de_plan::CleanseImplementationSpec>,
)> {
    // returns (dataset_id, expected_path, invariants, checklist, implementation_spec)
    let mut out: Vec<(
        String,
        String,
        Vec<String>,
        Vec<de_plan::PlanChecklistItem>,
        Option<de_plan::CleanseImplementationSpec>,
    )> = Vec::new();
    for ds in batch.iter() {
        if let Some(t) = plan.tasks.iter().find(|t| t.dataset_id == *ds) {
            let p = t.expected_model_path.clone().unwrap_or_default();
            out.push((
                ds.clone(),
                p,
                t.invariants.clone(),
                t.checklist.clone(),
                Some(t.implementation_spec.clone()),
            ));
        } else {
            out.push((ds.clone(), String::new(), vec![], vec![], None));
        }
    }
    out
}

fn resolve_model_batch_paths(
    plan: &de_plan::ModelPlan,
    batch: &[String],
) -> Vec<(
    String,
    String,
    Vec<String>,
    Vec<de_plan::PlanChecklistItem>,
    Option<de_plan::ModelImplementationSpec>,
)> {
    // returns (name, expected_path, invariants, checklist, implementation_spec)
    let mut out: Vec<(
        String,
        String,
        Vec<String>,
        Vec<de_plan::PlanChecklistItem>,
        Option<de_plan::ModelImplementationSpec>,
    )> = Vec::new();
    for name in batch.iter() {
        if let Some(t) = plan.tasks.iter().find(|t| t.name == *name) {
            let p = t.expected_model_path.clone().unwrap_or_default();
            out.push((
                name.clone(),
                p,
                t.invariants.clone(),
                t.checklist.clone(),
                Some(t.implementation_spec.clone()),
            ));
        } else {
            out.push((name.clone(), String::new(), vec![], vec![], None));
        }
    }
    out
}

async fn build_project_context(actx: &AgentCtx) -> Vec<ProjectFile> {
    let mut out: Vec<ProjectFile> = Vec::new();
    // Intentionally keep this to truly "skeleton" files only.
    // NOTE: do NOT include target/manifest.json as a raw file; it can be enormous and effectively
    // dumps the entire project graph + SQL into the prompt. Use structured/summary metadata instead.
    for p in ["dbt_project.yml", "packages.yml", "models/schema.yml"].iter() {
        if let Some(f) = read_project_file(actx, p, 0).await {
            out.push(f);
        }
    }
    out
}

async fn read_project_json_pointer(actx: &AgentCtx, path: &str, pointer: &str) -> Option<Value> {
    let tool = JsonFileTool;
    let obs = tool
        .call(
            serde_json::json!({"op":"get_item","path": path, "pointer": pointer}),
            actx,
        )
        .await
        .ok()?;
    if obs.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return None;
    }
    obs.get("json").cloned()
}

async fn schema_for_dataset_fqn(sctx: &SuiteCtx, actx: &AgentCtx, dataset_fqn: &str) -> Value {
    let Some(q) = sctx.query.as_ref() else {
        return serde_json::json!({"ok": false, "error": "query provider missing"});
    };
    let tool = SqlSchemaTool {
        query: q.clone(),
        datasets: sctx.datasets.clone(),
        catalog: sctx.catalog.clone(),
    };
    tool.call(serde_json::json!({"table": dataset_fqn}), actx)
        .await
        .unwrap_or_else(|e| serde_json::json!({"ok": false, "error": e}))
}

fn cap_schema_columns(schema_obs: &Value) -> Value {
    // Normalize to {ok, columns:[{name,type}], ...} with deterministic column cap.
    let mut v = schema_obs.clone();
    if let Some(obj) = v.as_object_mut() {
        if let Some(cols) = obj.get_mut("columns") {
            if let Some(arr) = cols.as_array_mut() {
                let total = arr.len();
                if total > MAX_SCHEMA_COLS_PER_ITEM {
                    arr.truncate(MAX_SCHEMA_COLS_PER_ITEM);
                    obj.insert("columns_truncated".to_string(), serde_json::json!(true));
                    obj.insert("columns_total".to_string(), serde_json::json!(total));
                } else {
                    obj.insert("columns_truncated".to_string(), serde_json::json!(false));
                    obj.insert("columns_total".to_string(), serde_json::json!(total));
                }
            }
        }
    }
    v
}

async fn dependency_schemas_for_sql(sctx: &SuiteCtx, actx: &AgentCtx, sql: &str) -> Value {
    // Derive authoritative schemas for dependencies referenced by the model SQL:
    // - ref('...') -> relation FQN via manifest index
    // - source('schema','table') -> relation FQN via manifest source index
    //
    // Keep bounded and deterministic.
    let mut out: Vec<Value> = Vec::new();

    let mut ref_names = naming::extract_ref_calls(sql);
    ref_names.sort();
    ref_names.dedup();
    ref_names.truncate(20);

    let source_calls = naming::extract_source_calls(sql);

    // Resolve refs via manifest model index.
    let ref_fqns = facts::resolve_model_names_to_fqns(actx, &ref_names).await;
    for (i, fqn) in ref_fqns.iter().enumerate() {
        // Pair with ref name best-effort (same order as ref_names after mapping isn't guaranteed).
        let name = ref_names.get(i).cloned().unwrap_or_else(|| "".to_string());
        let schema = cap_schema_columns(&schema_for_dataset_fqn(sctx, actx, fqn).await);
        out.push(serde_json::json!({
            "kind": "ref",
            "ref_name": name,
            "relation_fqn": fqn,
            "schema": schema
        }));
        if out.len() >= 25 {
            break;
        }
    }

    // Resolve sources via manifest source index (more reliable than guessing catalog/schema).
    let src_idx = facts::load_manifest_source_index(actx).await;
    for (src_name, table_name) in source_calls.into_iter().take(25) {
        if let Some(fqn) = src_idx.get(&(src_name.clone(), table_name.clone())) {
            let schema = cap_schema_columns(&schema_for_dataset_fqn(sctx, actx, fqn).await);
            out.push(serde_json::json!({
                "kind": "source",
                "source_name": src_name,
                "table_name": table_name,
                "relation_fqn": fqn,
                "schema": schema
            }));
        }
        if out.len() >= 50 {
            break;
        }
    }

    Value::Array(out)
}

fn render_path_list(label: &str, paths: &[String], max_items: usize) -> String {
    let mut p = paths.to_vec();
    p.sort();
    p.dedup();
    let total = p.len();
    let keep = total.min(max_items);
    let shown = &p[..keep];
    let mut s = String::new();
    s.push_str(label);
    s.push_str(&format!(" (total={}):\n", total));
    for it in shown {
        s.push_str("- ");
        s.push_str(it);
        s.push('\n');
    }
    if total > keep {
        s.push_str(&format!(
            "... omitted {} more (deterministic head)\n",
            total - keep
        ));
    }
    s
}

fn compact_review_context_for_summary(question_with_context: &str) -> String {
    // Deterministic compaction: avoid embedding large JSON blobs from thread history (e.g. prior review batch notes)
    // in the summary prompt. This is NOT random truncation; it turns the detail into hashes + small key excerpts.
    let mut entry_reason_code: Option<String> = None;
    let mut entry_reason_detail_raw: Option<String> = None;

    for line in question_with_context.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("- entry_reason_code:") {
            let v = rest.trim();
            if !v.is_empty() {
                entry_reason_code = Some(v.to_string());
            }
        }
        if let Some(rest) = t.strip_prefix("- entry_reason_detail:") {
            let v = rest.trim();
            if !v.is_empty() {
                entry_reason_detail_raw = Some(v.to_string());
            }
        }
    }

    let mut out = String::new();
    if let Some(rc) = entry_reason_code.as_deref() {
        out.push_str("entry_reason_code: ");
        out.push_str(rc);
        out.push('\n');
    }

    if let Some(raw) = entry_reason_detail_raw.as_deref() {
        let hash = react_core::llm_observability::sha256_hex_str(raw);
        out.push_str("entry_reason_detail_sha256: ");
        out.push_str(&hash);
        out.push('\n');

        // Best-effort parse + extract small, stable fields.
        if let Ok(v) = serde_json::from_str::<Value>(raw) {
            if let Some(idx) = v.get("batch_idx").and_then(|x| x.as_u64()) {
                out.push_str(&format!("entry_reason_detail.batch_idx: {}\n", idx));
            }
            if let Some(items) = v.get("batch_items").and_then(|x| x.as_array()) {
                let mut names: Vec<String> = items
                    .iter()
                    .filter_map(|it| it.as_str().map(|s| s.to_string()))
                    .collect();
                names.sort();
                names.dedup();
                let keep = names.len().min(5);
                out.push_str(&format!(
                    "entry_reason_detail.batch_items_head (sorted, {} of {}): {}\n",
                    keep,
                    names.len(),
                    names.into_iter().take(keep).collect::<Vec<_>>().join(", ")
                ));
            }
            if let Some(keys) = v.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()) {
                let mut kk = keys;
                kk.sort();
                out.push_str(&format!(
                    "entry_reason_detail.keys_sorted: {}\n",
                    kk.into_iter().take(20).collect::<Vec<_>>().join(", ")
                ));
            }
        }
    }

    if out.trim().is_empty() {
        // Fall back to the original question context if we couldn't extract anything.
        question_with_context.to_string()
    } else {
        out
    }
}

fn render_files(files: &[ProjectFile]) -> String {
    let mut s = String::new();
    for f in files {
        s.push_str("\n---\npath: ");
        s.push_str(&f.path);
        s.push_str("\n\n");
        s.push_str(&f.content);
        s.push_str("\n");
    }
    s
}

pub async fn run_batched_review(
    thread_id: &str,
    original_question_with_context: &str,
    phase: Phase,
    sctx: &SuiteCtx,
) -> Result<Vec<FlowFrame>, String> {
    // AgentCtx for deterministic storage reads/writes and thread store appends.
    let thread_store = ThreadStore::new(
        sctx.storage.clone(),
        sctx.scope.clone(),
        sctx.keyspace.clone(),
    );
    let actx = AgentCtx {
        top_k: 1,
        per_step_timeout_secs: 10,
        max_steps: 1,
        thread_id: Some(thread_id.to_string()),
        progress_tx: None,
        pre_step_tx: None,
        trace_tx: sctx.trace_tx.clone(),
        agent_name: Some("review".to_string()),
        policy: Arc::new(react_core::agent::DefaultPolicy),
        llm: sctx.llm.clone(),
        storage: sctx.storage.clone(),
        scope: sctx.scope.clone(),
        keyspace: sctx.keyspace.clone(),
        query: sctx.query.clone(),
        warehouse: sctx.warehouse.clone(),
        dbt: sctx.dbt.clone(),
        vector: sctx.vector.clone(),
        thread_store: Some(thread_store.clone()),
        exec_ctx: None,
        runtime: sctx
            .resolved_config
            .clone()
            .map(|c| c as Arc<dyn std::any::Any + Send + Sync>),
    };

    // Determine which plan (if any) to use for batching + persistence target.
    let (plan_kind, plan_key, batches, item_to_path_inv_notes): (
        Option<PlanKind>,
        Option<String>,
        Vec<Vec<String>>,
        Option<Value>,
    ) = match phase {
        Phase::CleanseReview => {
            let key = de_plan::newest_plan_key_any(&actx, "_cleanse.json").await;
            if let Some(k) = key.clone() {
                if let Some(p) = de_plan::load_cleanse_plan_by_key(&actx, &k).await {
                    let batches = if !p.batches.is_empty() {
                        p.batches.clone()
                    } else {
                        vec![]
                    };
                    let mut map: Vec<Value> = Vec::new();
                    for b in batches.iter() {
                        let resolved = resolve_cleanse_batch_paths(&p, b);
                        for (ds, path, inv, checklist, implementation_spec) in resolved {
                            map.push(serde_json::json!({
                                "item": ds,
                                "expected_model_path": path,
                                "invariants": inv,
                                "checklist": checklist,
                                "implementation_spec": implementation_spec
                            }));
                        }
                    }
                    (
                        Some(PlanKind::Cleanse),
                        Some(k),
                        batches,
                        Some(Value::Array(map)),
                    )
                } else {
                    (None, None, vec![], None)
                }
            } else {
                (None, None, vec![], None)
            }
        }
        Phase::ModelReview => {
            let key = de_plan::newest_plan_key_any(&actx, "_model.json").await;
            if let Some(k) = key.clone() {
                if let Some(p) = de_plan::load_model_plan_by_key(&actx, &k).await {
                    let batches = if !p.batches.is_empty() {
                        p.batches.clone()
                    } else {
                        vec![]
                    };
                    let mut map: Vec<Value> = Vec::new();
                    for b in batches.iter() {
                        let resolved = resolve_model_batch_paths(&p, b);
                        for (name, path, inv, checklist, implementation_spec) in resolved {
                            map.push(serde_json::json!({
                                "item": name,
                                "expected_model_path": path,
                                "invariants": inv,
                                "checklist": checklist,
                                "implementation_spec": implementation_spec
                            }));
                        }
                    }
                    (
                        Some(PlanKind::Model),
                        Some(k),
                        batches,
                        Some(Value::Array(map)),
                    )
                } else {
                    (None, None, vec![], None)
                }
            } else {
                (None, None, vec![], None)
            }
        }
        Phase::PostPublishReview => {
            // Prefer newest plan of either kind (same as WS snapshot fallback).
            let kc = de_plan::newest_plan_key_any(&actx, "_cleanse.json").await;
            let km = de_plan::newest_plan_key_any(&actx, "_model.json").await;
            let choose: Option<PlanKind> = match (&kc, &km) {
                (Some(c), Some(m)) => {
                    if c >= m {
                        Some(PlanKind::Cleanse)
                    } else {
                        Some(PlanKind::Model)
                    }
                }
                (Some(_), None) => Some(PlanKind::Cleanse),
                (None, Some(_)) => Some(PlanKind::Model),
                (None, None) => None,
            };
            match choose {
                Some(PlanKind::Cleanse) => {
                    let k = kc.clone().unwrap();
                    if let Some(p) = de_plan::load_cleanse_plan_by_key(&actx, &k).await {
                        let batches = if !p.batches.is_empty() {
                            p.batches.clone()
                        } else {
                            vec![]
                        };
                        let mut map: Vec<Value> = Vec::new();
                        for b in batches.iter() {
                            let resolved = resolve_cleanse_batch_paths(&p, b);
                            for (ds, path, inv, checklist, implementation_spec) in resolved {
                                map.push(serde_json::json!({
                                    "item": ds,
                                    "expected_model_path": path,
                                    "invariants": inv,
                                    "checklist": checklist,
                                    "implementation_spec": implementation_spec
                                }));
                            }
                        }
                        (
                            Some(PlanKind::Cleanse),
                            Some(k),
                            batches,
                            Some(Value::Array(map)),
                        )
                    } else {
                        (None, None, vec![], None)
                    }
                }
                Some(PlanKind::Model) => {
                    let k = km.clone().unwrap();
                    if let Some(p) = de_plan::load_model_plan_by_key(&actx, &k).await {
                        let batches = if !p.batches.is_empty() {
                            p.batches.clone()
                        } else {
                            vec![]
                        };
                        let mut map: Vec<Value> = Vec::new();
                        for b in batches.iter() {
                            let resolved = resolve_model_batch_paths(&p, b);
                            for (name, path, inv, checklist, implementation_spec) in resolved {
                                map.push(serde_json::json!({
                                    "item": name,
                                    "expected_model_path": path,
                                    "invariants": inv,
                                    "checklist": checklist,
                                    "implementation_spec": implementation_spec
                                }));
                            }
                        }
                        (
                            Some(PlanKind::Model),
                            Some(k),
                            batches,
                            Some(Value::Array(map)),
                        )
                    } else {
                        (None, None, vec![], None)
                    }
                }
                None => (None, None, vec![], None),
            }
        }
        _ => (None, None, vec![], None),
    };

    // Fallback: group by file names if no plan batches exist.
    let batches = if !batches.is_empty() {
        batches
    } else {
        let files = match phase {
            Phase::CleanseReview => list_model_files(&actx, "models/staging/", 500).await,
            Phase::ModelReview | Phase::PostPublishReview => {
                let mut f = list_model_files(&actx, "models/marts/", 500).await;
                f.extend(list_model_files(&actx, "models/core/", 500).await);
                f.sort();
                f.dedup();
                f
            }
            _ => vec![],
        };
        // Keep file paths as items so we can always fetch the right file in review.
        chunk_vec(&files, DEFAULT_BATCH_SIZE)
    };

    // 1) Project summary pass (bounded, and MUST NOT dump the whole project).
    let proj_files = build_project_context(&actx).await;
    let staging_paths = list_model_files(&actx, "models/staging/", 2000).await;
    let marts_paths = list_model_files(&actx, "models/marts/", 2000).await;
    let core_paths = list_model_files(&actx, "models/core/", 2000).await;

    // Minimal manifest metadata only (no nodes/raw_code dump).
    let manifest_meta = read_project_json_pointer(&actx, "target/manifest.json", "/metadata").await;
    let manifest_meta_small = manifest_meta
        .as_ref()
        .and_then(|m| m.as_object())
        .map(|m| {
            let pick = |k: &str| m.get(k).cloned().unwrap_or(Value::Null);
            serde_json::json!({
                "dbt_schema_version": pick("dbt_schema_version"),
                "dbt_version": pick("dbt_version"),
                "generated_at": pick("generated_at"),
                "adapter_type": pick("adapter_type"),
                "project_name": pick("project_name"),
                "invocation_id": pick("invocation_id"),
            })
        })
        .unwrap_or(Value::Null);

    let global_ctx_txt = if include_global_semantic_context(phase) {
        let global_ctx = load_global_semantic_context_json(&actx, sctx).await;
        serde_json::to_string_pretty(&global_ctx).unwrap_or_else(|_| "null".to_string())
    } else {
        String::new()
    };
    let global_ctx_block = if include_global_semantic_context(phase) {
        format!(
            "IMMUTABLE CONTEXT (global_semantic_context):\n{gctx}\n\n",
            gctx = global_ctx_txt
        )
    } else {
        String::new()
    };

    let review_context_brief = compact_review_context_for_summary(original_question_with_context);
    let summary_user = format!(
        "Phase: {phase}\n\nOriginal goal + review context (brief):\n{q}\n\n{gctx_block}Project skeleton files:\n{files}\n\nProject index:\n{idx}\n\nManifest metadata (minimal):\n{meta}\n",
        phase = phase.as_str(),
        q = review_context_brief,
        gctx_block = global_ctx_block,
        files = render_files(&proj_files),
        idx = format!(
            "{}\n{}\n{}",
            render_path_list("models/staging", &staging_paths, 250),
            render_path_list("models/marts", &marts_paths, 150),
            render_path_list("models/core", &core_paths, 150),
        ),
        meta = serde_json::to_string_pretty(&manifest_meta_small).unwrap_or_else(|_| "null".to_string()),
    );
    let summary_v = llm_json(sctx, &actx, thread_id, phase, "summary", summary_user).await?;
    let project_notes = clamp_lines(
        str_list(summary_v.get("project_notes").unwrap_or(&Value::Null)),
        MAX_PROJECT_NOTES,
    );
    let project_risks = clamp_lines(
        str_list(summary_v.get("project_risks").unwrap_or(&Value::Null)),
        MAX_PROJECT_NOTES,
    );

    append_review_step(
        &thread_store,
        thread_id,
        phase,
        "review",
        PhaseReasonCode::ReviewProjectSummary,
        serde_json::json!({
            "project_notes": project_notes,
            "project_risks": project_risks
        }),
    )
    .await?;
    if let (Some(pk), Some(plan_key)) = (plan_kind, plan_key.as_deref()) {
        persist_review_summary_to_plan(
            &actx,
            phase,
            pk,
            plan_key,
            project_notes.clone(),
            project_risks.clone(),
        )
        .await?;
    }

    // 2) Per-batch pass (bounded, complete coverage).
    let mut all_batch_notes: Vec<Value> = Vec::new();
    for (bidx, batch) in batches.iter().enumerate() {
        let mut files: Vec<ProjectFile> = Vec::new();
        let mut batch_detail: Vec<Value> = Vec::new();

        // When plan exists, include path+invariants/notes mapping (best-effort).
        let mapping = item_to_path_inv_notes.clone().unwrap_or(Value::Null);
        let mapping_arr = mapping.as_array().cloned().unwrap_or_default();
        let map_for = |item: &str| -> Option<Value> {
            mapping_arr
                .iter()
                .find(|it| it.get("item").and_then(|v| v.as_str()) == Some(item))
                .cloned()
        };

        // Build batch payload, reading only expected paths when known.
        for item in batch.iter() {
            let mut expected_path = String::new();
            let mut invariants: Vec<String> = Vec::new();
            let mut notes: Vec<String> = Vec::new();
            let mut implementation_spec: Value = Value::Null;
            if let Some(m) = map_for(item) {
                expected_path = m
                    .get("expected_model_path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                invariants = str_list(m.get("invariants").unwrap_or(&Value::Null));
                notes = str_list(m.get("notes").unwrap_or(&Value::Null));
                implementation_spec = m.get("implementation_spec").cloned().unwrap_or(Value::Null);
            }
            if expected_path.trim().is_empty() {
                // Fallback: if item looks like a path, treat it as such. Otherwise assume models/<name>.sql.
                let it = item.trim();
                if it.starts_with("models/") || it.ends_with(".sql") || it.contains('/') {
                    expected_path = it.to_string();
                } else {
                    expected_path = format!("models/{}.sql", it);
                }
            }
            if let Some(f) = read_project_file(&actx, &expected_path, 60_000).await {
                files.push(f);
            }

            // Authoritative schema for dataset-like items (e.g., AwsDataCatalog.schema.table).
            // Best-effort: only fetch schema when the item looks like a dataset FQN.
            let schema_obs = if item.split('.').count() >= 3 {
                let raw = schema_for_dataset_fqn(sctx, &actx, item).await;
                cap_schema_columns(&raw)
            } else {
                Value::Null
            };

            // Also attach dependency schemas derived from the model file content (ref/source schemas),
            // so review suggestions do not invent fields for upstream relations.
            let deps = if let Some(f) = files
                .iter()
                .find(|ff| ff.path.trim() == expected_path.trim())
            {
                dependency_schemas_for_sql(sctx, &actx, &f.content).await
            } else {
                Value::Array(vec![])
            };
            batch_detail.push(serde_json::json!({
                "item": item,
                "expected_model_path": expected_path,
                "invariants": invariants,
                "task_notes": notes,
                "implementation_spec": implementation_spec,
                "authoritative_schema": schema_obs
                ,"dependency_schemas": deps
            }));
        }

        let batch_user = format!(
            "Phase: {phase}\nBatch {i}/{n}\n\nOriginal goal + review context:\n{q}\n\n{gctx_block}Batch items:\n{items}\n\nBatch file contents (bounded):\n{files}\n",
            phase = phase.as_str(),
            i = bidx + 1,
            n = batches.len(),
            q = original_question_with_context,
            gctx_block = global_ctx_block,
            items = serde_json::to_string_pretty(&batch_detail).unwrap_or_else(|_| "[]".to_string()),
            files = render_files(&files),
        );
        let v = llm_json(sctx, &actx, thread_id, phase, "batch", batch_user).await?;
        let notes = clamp_lines(
            str_list(v.get("notes").unwrap_or(&Value::Null)),
            MAX_NOTES_PER_BATCH,
        );
        let actionable_hints = clamp_lines(
            str_list(v.get("actionable_hints").unwrap_or(&Value::Null)),
            MAX_NOTES_PER_BATCH,
        );

        let detail = serde_json::json!({
            "batch_idx": bidx,
            "batch_items": batch,
            "notes": notes,
            "actionable_hints": actionable_hints
        });
        append_review_step(
            &thread_store,
            thread_id,
            phase,
            "review",
            PhaseReasonCode::ReviewBatch,
            detail.clone(),
        )
        .await?;

        all_batch_notes.push(detail.clone());
        if let (Some(pk), Some(plan_key)) = (plan_kind, plan_key.as_deref()) {
            persist_review_batch_to_plan(
                &actx,
                pk,
                plan_key,
                bidx,
                batch.clone(),
                notes.clone(),
            )
            .await?;
        }
    }

    // 3) Final unify pass (META-compatible output).
    // Keep unify input bounded: it is easy for pretty-printed batch detail to grow large, which
    // increases the risk of truncated JSON responses.
    let mut unify_batches = all_batch_notes.clone();
    if unify_batches.len() > 20 {
        unify_batches = unify_batches[unify_batches.len() - 20..].to_vec();
    }
    // Strip to the minimal fields the unifier needs.
    let unify_batches = unify_batches
        .into_iter()
        .map(|v| {
            let batch_items = v.get("batch_items").cloned().unwrap_or(Value::Null);
            let notes = v.get("notes").cloned().unwrap_or(Value::Null);
            let actionable_hints = v.get("actionable_hints").cloned().unwrap_or(Value::Null);
            serde_json::json!({
                "batch_items": batch_items,
                "notes": notes,
                "actionable_hints": actionable_hints
            })
        })
        .collect::<Vec<_>>();
    let unify_user = format!(
        "Phase: {phase}\n\nOriginal goal + review context:\n{q}\n\n{gctx_block}Project notes:\n{proj}\n\nBatch notes:\n{batches}\n",
        phase = phase.as_str(),
        q = original_question_with_context,
        gctx_block = global_ctx_block,
        proj = serde_json::json!({"project_notes": project_notes, "project_risks": project_risks}),
        batches = serde_json::to_string_pretty(&unify_batches).unwrap_or_else(|_| "[]".to_string()),
    );
    let unify_v = llm_json(sctx, &actx, thread_id, phase, "unify", unify_user).await?;
    let unify: ReviewUnifyOutput = serde_json::from_value(unify_v)
        .map_err(|e| format!("batched review unify output did not match schema: {e}"))?;
    let final_review_text = unify.final_review_text;
    if final_review_text.trim().is_empty() {
        return Err("batched review unify produced empty final_review_text".to_string());
    }

    // Persist full review text to storage; keep thread steps small (store only a reference).
    let decision = unify.decision;
    let tier = unify.tier;
    let dataset_ids = unify.dataset_ids;
    let review_sha256 = react_core::llm_observability::sha256_hex_str(&final_review_text);
    let review_bytes = final_review_text.as_bytes().len() as u64;
    let review_key = {
        // Store alongside other thread-scoped artifacts (plans/, dbt/, etc), not under dbt/.
        let root = actx
            .keyspace
            .threads_prefix(&actx.scope)
            .trim_end_matches("/threads")
            .trim_end_matches('/')
            .to_string();
        let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let sha8 = review_sha256.chars().take(8).collect::<String>();
        format!("{}/reviews/{}/{}_{}.txt", root, thread_id, ts, sha8)
    };
    sctx.storage
        .put_bytes(&review_key, final_review_text.as_bytes(), "text/plain")
        .await
        .map_err(|e| format!("failed to persist final review artifact: {e}"))?;

    let review_ref = serde_json::json!({
        "key": review_key,
        "sha256": review_sha256,
        "bytes": review_bytes
    });

    append_review_step(
        &thread_store,
        thread_id,
        phase,
        "review",
        PhaseReasonCode::ReviewFinalUnify,
        serde_json::json!({
            "review_ref": review_ref.clone(),
            "meta": {
                "decision": decision,
                "tier": tier,
                "dataset_ids": dataset_ids.clone()
            },
            "review_phase": phase.as_str()
        }),
    )
    .await?;
    if let (Some(pk), Some(plan_key)) = (plan_kind, plan_key.as_deref()) {
        persist_review_final_to_plan(
            &actx,
            pk,
            plan_key,
            decision,
            tier,
            dataset_ids.clone(),
            final_review_text.clone(),
        )
        .await?;
    }

    Ok(vec![FlowFrame::Final {
        kind: "generic".to_string(),
        payload: serde_json::json!({
            "text": final_review_text.clone(),
            "meta": {
                "decision": decision,
                "tier": tier,
                "dataset_ids": dataset_ids,
                "review_ref": review_ref
            }
        }),
        display: Some(final_review_text),
    }])
}

#[cfg(test)]
mod tests {
    use super::*;
    use react_core::keyspace::{DefaultKeyspace, Keyspace};
    use react_core::scope::RequestScope;
    use react_core::storage::InMemoryStorageAdapter;
    use std::sync::{Arc, Mutex};

    struct ScriptedModel {
        replies: Arc<Mutex<Vec<String>>>,
    }

    impl react_core::llm::LargeLanguageModel for ScriptedModel {
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

    struct CapturingModel {
        replies: Arc<Mutex<Vec<String>>>,
        captured_user_prompts: Arc<Mutex<Vec<String>>>,
    }

    impl react_core::llm::LargeLanguageModel for CapturingModel {
        fn chat(
            &self,
            messages: &[react_core::llm::ChatMessage],
            _options: &react_core::llm::LlmCallOptions,
        ) -> Result<String, String> {
            if let Some(u) = messages.iter().find(|m| m.role == "user") {
                if let Ok(mut g) = self.captured_user_prompts.lock() {
                    g.push(u.content.clone());
                }
            }
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

    fn make_suite_ctx(
        storage: Arc<dyn react_core::storage::StorageAdapter>,
        llm: Arc<dyn react_core::llm::LargeLanguageModel>,
    ) -> SuiteCtx {
        let scope = RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        };
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        SuiteCtx::new(
            storage,
            Arc::new(react_core::providers::NullSecretsProvider::default()),
            llm,
            scope,
            keyspace,
        )
    }

    #[tokio::test]
    async fn batched_review_persists_project_snapshot_batches_and_final() {
        let storage: Arc<dyn react_core::storage::StorageAdapter> =
            Arc::new(InMemoryStorageAdapter::default());
        let llm = Arc::new(ScriptedModel {
            replies: Arc::new(Mutex::new(vec![
                // summary
                serde_json::json!({"project_notes":["n1"],"project_risks":["r1"]}).to_string(),
                // batch 1
                serde_json::json!({"notes":["b1n"],"actionable_hints":["h1"]}).to_string(),
                // batch 2
                serde_json::json!({"notes":["b2n"],"actionable_hints":["h2"]}).to_string(),
                // unify
                serde_json::json!({"decision":"proceed","tier":"silver","dataset_ids":[],"final_review_text":"All good."}).to_string(),
            ])),
        });
        let sctx = make_suite_ctx(storage.clone(), llm);

        // Seed a minimal dbt project and a cleanse plan with 2 batches.
        let actx = AgentCtx {
            top_k: 1,
            per_step_timeout_secs: 1,
            max_steps: 1,
            thread_id: Some("tid".to_string()),
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: None,
            agent_name: Some("test".to_string()),
            policy: Arc::new(react_core::agent::DefaultPolicy),
            llm: sctx.llm.clone(),
            storage: sctx.storage.clone(),
            scope: sctx.scope.clone(),
            keyspace: sctx.keyspace.clone(),
            query: None,
            warehouse: std::sync::Arc::new(react_core::providers::NullWarehouseProvider::default()),
            dbt: None,
            vector: None,
            thread_store: None,
            exec_ctx: None,
            runtime: None,
        };

        // dbt files
        let base = actx
            .keyspace
            .dbt_prefix(&actx.scope)
            .trim_end_matches('/')
            .to_string();
        let put = |rel: &str, content: &str| {
            let key = format!("{}/{}", base, rel);
            let storage = storage.clone();
            let content = content.to_string();
            async move {
                storage
                    .put_bytes(&key, content.as_bytes(), "text/plain")
                    .await
                    .unwrap();
            }
        };
        put("dbt_project.yml", "name: x\n").await;
        put("models/schema.yml", "version: 2\n").await;
        put("models/staging/stg_a.sql", "select 1 as a\n").await;
        put("models/staging/stg_b.sql", "select 1 as b\n").await;

        let mut plan = de_plan::CleansePlan {
            plan_key: "k_cleanse".to_string(),
            status: de_plan::PlanStatus::Approved,
            project_snapshot: serde_json::json!({}),
            tasks: vec![
                de_plan::CleanseTask {
                    dataset_id: "a.b.a".to_string(),
                    expected_model_path: Some("models/staging/stg_a.sql".to_string()),
                    invariants: vec!["inv".to_string()],
                    implementation_spec: de_plan::CleanseImplementationSpec {
                        spec_version: 1,
                        row_preserving: true,
                        output_fields: vec![de_plan::OutputFieldSpec {
                            name: "a".to_string(),
                            kind: de_plan::FieldKind::Derived,
                            source_columns: vec![],
                            expression: "select 1 as a (test stub)".to_string(),
                            data_type: None,
                            nullable: true,
                            description: None,
                        }],
                        prohibited_ops: vec![],
                    },
                    status: de_plan::TaskStatus::Done,
                    checklist: vec![
                        de_plan::PlanChecklistItem {
                            checklist_item_id: "sql_model".to_string(),
                            label: "Author staging SQL".to_string(),
                            details: None,
                            status: de_plan::ChecklistItemStatus::Done,
                            origin: de_plan::ChecklistOrigin::Initial,
                            origin_step_idx: None,
                            evidence: vec![],
                        },
                        de_plan::PlanChecklistItem {
                            checklist_item_id: "schema_contract".to_string(),
                            label: "Author schema contract".to_string(),
                            details: None,
                            status: de_plan::ChecklistItemStatus::Done,
                            origin: de_plan::ChecklistOrigin::Initial,
                            origin_step_idx: None,
                            evidence: vec![],
                        },
                        de_plan::PlanChecklistItem {
                            checklist_item_id: "validate".to_string(),
                            label: "Validate model".to_string(),
                            details: None,
                            status: de_plan::ChecklistItemStatus::Done,
                            origin: de_plan::ChecklistOrigin::Initial,
                            origin_step_idx: None,
                            evidence: vec![],
                        },
                    ],
                },
                de_plan::CleanseTask {
                    dataset_id: "a.b.b".to_string(),
                    expected_model_path: Some("models/staging/stg_b.sql".to_string()),
                    invariants: vec![],
                    implementation_spec: de_plan::CleanseImplementationSpec {
                        spec_version: 1,
                        row_preserving: true,
                        output_fields: vec![de_plan::OutputFieldSpec {
                            name: "b".to_string(),
                            kind: de_plan::FieldKind::Derived,
                            source_columns: vec![],
                            expression: "select 1 as b (test stub)".to_string(),
                            data_type: None,
                            nullable: true,
                            description: None,
                        }],
                        prohibited_ops: vec![],
                    },
                    status: de_plan::TaskStatus::Done,
                    checklist: vec![
                        de_plan::PlanChecklistItem {
                            checklist_item_id: "sql_model".to_string(),
                            label: "Author staging SQL".to_string(),
                            details: None,
                            status: de_plan::ChecklistItemStatus::Done,
                            origin: de_plan::ChecklistOrigin::Initial,
                            origin_step_idx: None,
                            evidence: vec![],
                        },
                        de_plan::PlanChecklistItem {
                            checklist_item_id: "schema_contract".to_string(),
                            label: "Author schema contract".to_string(),
                            details: None,
                            status: de_plan::ChecklistItemStatus::Done,
                            origin: de_plan::ChecklistOrigin::Initial,
                            origin_step_idx: None,
                            evidence: vec![],
                        },
                        de_plan::PlanChecklistItem {
                            checklist_item_id: "validate".to_string(),
                            label: "Validate model".to_string(),
                            details: None,
                            status: de_plan::ChecklistItemStatus::Done,
                            origin: de_plan::ChecklistOrigin::Initial,
                            origin_step_idx: None,
                            evidence: vec![],
                        },
                    ],
                },
            ],
            batches: vec![vec!["a.b.a".to_string()], vec!["a.b.b".to_string()]],
            work_groups: vec![
                de_plan::PlanWorkGroup {
                    group_id: "wg_sql".to_string(),
                    label: "Author SQL".to_string(),
                    kind: de_plan::WorkGroupKind::AuthorSql,
                    items: vec![
                        de_plan::WorkGroupItemRef {
                            task_id: "a.b.a".to_string(),
                            checklist_item_id: "sql_model".to_string(),
                        },
                        de_plan::WorkGroupItemRef {
                            task_id: "a.b.b".to_string(),
                            checklist_item_id: "sql_model".to_string(),
                        },
                    ],
                    depends_on_group_ids: None,
                },
                de_plan::PlanWorkGroup {
                    group_id: "wg_schema".to_string(),
                    label: "Author schema".to_string(),
                    kind: de_plan::WorkGroupKind::AuthorSchema,
                    items: vec![
                        de_plan::WorkGroupItemRef {
                            task_id: "a.b.a".to_string(),
                            checklist_item_id: "schema_contract".to_string(),
                        },
                        de_plan::WorkGroupItemRef {
                            task_id: "a.b.b".to_string(),
                            checklist_item_id: "schema_contract".to_string(),
                        },
                    ],
                    depends_on_group_ids: Some(vec!["wg_sql".to_string()]),
                },
                de_plan::PlanWorkGroup {
                    group_id: "wg_validate".to_string(),
                    label: "Validate".to_string(),
                    kind: de_plan::WorkGroupKind::Validate,
                    items: vec![
                        de_plan::WorkGroupItemRef {
                            task_id: "a.b.a".to_string(),
                            checklist_item_id: "validate".to_string(),
                        },
                        de_plan::WorkGroupItemRef {
                            task_id: "a.b.b".to_string(),
                            checklist_item_id: "validate".to_string(),
                        },
                    ],
                    depends_on_group_ids: Some(vec!["wg_schema".to_string()]),
                },
            ],
            mutations: vec![],
            progress: de_plan::PlanProgress::default(),
        };
        // Persist plan under standard plans prefix so loader finds it.
        plan.plan_key = de_plan::new_cleanse_plan_key(&actx);
        de_plan::save_cleanse_plan(&actx, &plan).await.unwrap();

        let out = run_batched_review("tid", "goal", Phase::CleanseReview, &sctx)
            .await
            .expect("ok");
        assert!(matches!(out[0], FlowFrame::Final { .. }));

        // Thread log step should store review by reference (not the full text).
        let thread_store =
            ThreadStore::new(storage.clone(), sctx.scope.clone(), sctx.keyspace.clone());
        let log = thread_store.get("tid").await.expect("thread log");
        let last = log.steps.last().cloned().expect("step");
        match last {
            react_core::session::ThreadStep::Phase { reason_detail, .. } => {
                let d = reason_detail.expect("reason_detail");
                assert!(d.get("review_ref").is_some());
                assert!(d.get("review_ref").and_then(|v| v.get("key")).is_some());
                assert!(d.get("meta").is_some());
                assert!(d.get("final_review_text").is_none());
            }
            other => panic!("expected Phase step, got {other:?}"),
        }

        let loaded = de_plan::load_cleanse_plan_by_key(&actx, &plan.plan_key)
            .await
            .expect("plan");
        let review = loaded
            .project_snapshot
            .get("review")
            .cloned()
            .unwrap_or(Value::Null);
        assert_eq!(
            review.get("review_version").and_then(|v| v.as_i64()),
            Some(REVIEW_SNAPSHOT_VERSION)
        );
        assert!(review.get("project_notes").is_some());
        assert!(
            review
                .get("batches")
                .and_then(|v| v.as_array())
                .unwrap_or(&vec![])
                .len()
                >= 2
        );
        assert!(review.get("final").is_some());
    }

    #[tokio::test]
    async fn global_semantic_context_is_not_injected_for_cleanse_review_but_is_for_model_review() {
        let storage: Arc<dyn react_core::storage::StorageAdapter> =
            Arc::new(InMemoryStorageAdapter::default());

        // Seed global semantic context.
        let scope = RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        };
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let gkey = keyspace.semantic_key(
            &scope,
            react_core::providers::catalog::types::GLOBAL_SEMANTIC_DATASET_ID,
        );
        storage
            .put_bytes(
                &gkey,
                serde_json::json!({"domain":"should_not_leak_to_cleanse"})
                    .to_string()
                    .as_bytes(),
                "application/json",
            )
            .await
            .unwrap();

        // Seed minimal dbt files expected by project snapshot.
        let base = keyspace
            .dbt_prefix(&scope)
            .trim_end_matches('/')
            .to_string();
        storage
            .put_bytes(
                &format!("{}/dbt_project.yml", base),
                "name: x\n".as_bytes(),
                "text/plain",
            )
            .await
            .unwrap();
        storage
            .put_bytes(
                &format!("{}/models/schema.yml", base),
                "version: 2\n".as_bytes(),
                "text/plain",
            )
            .await
            .unwrap();

        // Cleanse review should NOT include global_semantic_context in any prompt.
        let captured_cleanse: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
        let llm_cleanse = Arc::new(CapturingModel {
            captured_user_prompts: captured_cleanse.clone(),
            replies: Arc::new(Mutex::new(vec![
                serde_json::json!({"project_notes":[],"project_risks":[]}).to_string(), // summary
                serde_json::json!({"decision":"proceed","tier":"silver","dataset_ids":[],"final_review_text":"ok"}).to_string(), // unify
            ])),
        });
        let sctx_cleanse = SuiteCtx::new(
            storage.clone(),
            Arc::new(react_core::providers::NullSecretsProvider::default()),
            llm_cleanse,
            scope.clone(),
            keyspace.clone(),
        );
        let _ = run_batched_review("tid1", "goal", Phase::CleanseReview, &sctx_cleanse)
            .await
            .expect("ok");
        let prompts = captured_cleanse.lock().unwrap().clone();
        assert!(!prompts.is_empty());
        for p in prompts.iter() {
            assert!(!p.contains("IMMUTABLE CONTEXT (global_semantic_context)"));
            assert!(!p.contains("should_not_leak_to_cleanse"));
        }

        // Model review SHOULD include global_semantic_context.
        let captured_model: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
        let llm_model = Arc::new(CapturingModel {
            captured_user_prompts: captured_model.clone(),
            replies: Arc::new(Mutex::new(vec![
                serde_json::json!({"project_notes":[],"project_risks":[]}).to_string(), // summary
                serde_json::json!({"decision":"proceed","tier":"gold","dataset_ids":[],"final_review_text":"ok"}).to_string(), // unify
            ])),
        });
        let sctx_model = SuiteCtx::new(
            storage.clone(),
            Arc::new(react_core::providers::NullSecretsProvider::default()),
            llm_model,
            scope.clone(),
            keyspace.clone(),
        );
        let _ = run_batched_review("tid2", "goal", Phase::ModelReview, &sctx_model)
            .await
            .expect("ok");
        let prompts = captured_model.lock().unwrap().clone();
        assert!(!prompts.is_empty());
        assert!(prompts
            .iter()
            .any(|p| p.contains("IMMUTABLE CONTEXT (global_semantic_context)")));
        assert!(prompts
            .iter()
            .any(|p| p.contains("should_not_leak_to_cleanse")));
    }
}
