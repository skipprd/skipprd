use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;

use crate::domain_types::{
    ReviewArtifactRef, ReviewBatchOutput, ReviewSummaryOutput, ReviewTier, ReviewUnifyOutput,
};
use react_core::agent::AgentCtx;
use react_core::keyspace::encode_key_component;
use react_core::llm::{ChatMessage, ChatRole, LlmCallOptions, LlmExpectedFormat, ReasoningEffort};
use react_core::session::ThreadStore;
use react_core::tools::Tool;

use react_core::suite::{FlowFrame, FlowKind, SuiteCtx};

use super::control_flow::Phase;
use super::plan as de_plan;
use super::plan_kind::PlanKind;
use super::tools::files_tool::FilesTool;
use super::tools::json_file::JsonFileTool;
use super::tools::sql_schema::SqlSchemaTool;
use crate::{facts, naming};

use super::review_persistence::*;
use super::review_prompts::*;

/// Typed container for a single batch item passed to the review LLM.
/// Serialized to JSON only at the prompt boundary — everything before that is
/// compile-time checked.
#[derive(Serialize)]
pub(crate) struct ReviewBatchItem {
    pub item: String,
    pub expected_model_path: String,
    pub invariants: Vec<String>,
    pub implementation_spec: Value,
    pub authoritative_schema: Value,
    pub dependency_schemas: Value,
}

pub(super) const REVIEW_SNAPSHOT_VERSION: i64 = 1;
const DEFAULT_BATCH_SIZE: usize = 5;
pub(super) const MAX_BATCHES_SAVED: usize = 40;
const MAX_NOTES_PER_BATCH: usize = 20;
const MAX_PROJECT_NOTES: usize = 40;
const MAX_SCHEMA_COLS_PER_ITEM: usize = 250;

#[derive(Clone, Debug)]
pub(super) struct ProjectFile {
    pub(super) path: String,
    pub(super) content: String,
}

pub(super) fn utc_ts() -> String {
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

async fn read_project_file(actx: &AgentCtx, path: &str, max_chars: usize) -> Option<ProjectFile> {
    let tool = FilesTool { datasets: None };
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
    let tool = FilesTool { datasets: None };
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
        Some("extra_high") | Some("xhigh") => Some(ReasoningEffort::ExtraHigh),
        _ => None,
    }
}

/// Typed LLM configuration for a single review step (summary, batch, or unify).
///
/// `for_phase` is pure — it computes prompt ID, system prompt, token budget, reasoning effort,
/// and temperature deterministically from the phase + step name (plus env-var overrides).
/// `call` is a thin async wrapper that finalises the dynamic output budget and dispatches.
struct ReviewLlmConfig {
    prompt_id: &'static str,
    system_prompt: String,
    phase_budget: u32,
    reasoning_effort: ReasoningEffort,
    /// `Some(temp)` for known review phases; `None` triggers the generic fallback options.
    temperature: Option<f32>,
    step_name: String,
}

impl ReviewLlmConfig {
    fn for_phase(phase: Phase, step_name: &str) -> Self {
        use crate::env_util::{env_keys, env_u32 as parse_u32_env};

        let reasoning_effort = parse_review_reasoning_effort(env_keys::LLM_REVIEW_REASONING_EFFORT)
            .unwrap_or(ReasoningEffort::High);

        let default_non_unify: u32 = parse_u32_env(env_keys::LLM_REVIEW_MAX_TOKENS)
            .unwrap_or(12_000)
            .max(2_000)
            .min(32_000);
        let default_unify: u32 = parse_u32_env(env_keys::LLM_REVIEW_MAX_TOKENS_UNIFY)
            .unwrap_or(18_000)
            .max(2_000)
            .min(32_000);
        let is_unify = step_name == "unify";

        let phase_budget: u32 = match phase {
            Phase::CleanseReview => {
                let k = if is_unify {
                    env_keys::LLM_REVIEW_MAX_TOKENS_CLEANSE_UNIFY
                } else {
                    env_keys::LLM_REVIEW_MAX_TOKENS_CLEANSE
                };
                parse_u32_env(k).unwrap_or(if is_unify {
                    default_unify
                } else {
                    default_non_unify
                })
            }
            Phase::ModelReview => {
                let k = if is_unify {
                    env_keys::LLM_REVIEW_MAX_TOKENS_MODEL_UNIFY
                } else {
                    env_keys::LLM_REVIEW_MAX_TOKENS_MODEL
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

        let prompt_id: &'static str = match (phase, step_name) {
            (Phase::CleanseReview, "summary") => "data_engineer.cleanse_review.summary",
            (Phase::CleanseReview, "batch") => "data_engineer.cleanse_review.batch",
            (Phase::CleanseReview, "unify") => "data_engineer.cleanse_review.unify",
            (Phase::ModelReview, "summary") => "data_engineer.model_review.summary",
            (Phase::ModelReview, "batch") => "data_engineer.model_review.batch",
            (Phase::ModelReview, "unify") => "data_engineer.model_review.unify",
            (Phase::CleanseReview, _) => "data_engineer.cleanse_review.other",
            (Phase::ModelReview, _) => "data_engineer.model_review.other",
            _ => "data_engineer.review.other_phase",
        };

        let system_prompt = match step_name {
            "summary" => system_prompt_for_summary(phase_plan_kind(phase)),
            "batch" => system_prompt_for_batch(phase_plan_kind(phase)),
            _ => system_prompt_for_unify(phase_plan_kind(phase)),
        };

        let temperature = match phase {
            Phase::CleanseReview => Some(0.15),
            Phase::ModelReview => Some(0.25),
            _ => None,
        };

        Self {
            prompt_id,
            system_prompt,
            phase_budget,
            reasoning_effort,
            temperature,
            step_name: step_name.to_string(),
        }
    }

    async fn call(
        &self,
        actx: &AgentCtx,
        thread_id: &str,
        user_message: String,
    ) -> Result<Value, String> {
        // Dynamic sizing: bigger review contexts need bigger output budgets, otherwise Responses
        // truncates the JSON payload after spending many tokens on reasoning.
        let approx_in_tokens: u32 =
            ((self.system_prompt.len() + user_message.len()) as u32 / 2).max(1);
        let extra_out_tokens: u32 = match self.step_name.as_str() {
            "summary" => 6_000,
            "batch" => 12_000,
            _ => 16_000,
        };
        let dynamic_budget: u32 = approx_in_tokens
            .saturating_add(extra_out_tokens)
            .max(2_000)
            .min(32_000);
        let final_budget: u32 = self.phase_budget.max(dynamic_budget).min(32_000);

        let messages = vec![
            ChatMessage {
                role: ChatRole::System,
                content: self.system_prompt.clone(),
            },
            ChatMessage {
                role: ChatRole::User,
                content: user_message,
            },
        ];

        let expected_format = self.expected_format()?;
        let mut opts = if self.temperature.is_some() {
            LlmCallOptions {
                prompt_id: self.prompt_id,
                thread_id: Some(thread_id.to_string()),
                expected_format: expected_format.clone(),
                max_output_tokens: Some(final_budget),
                reasoning_effort: Some(self.reasoning_effort),
                ..Default::default()
            }
        } else {
            LlmCallOptions {
                prompt_id: "data_engineer.review_batched.llm_json",
                expected_format: expected_format.clone(),
                ..Default::default()
            }
        };
        opts.expected_format = expected_format;
        actx.llm_chat_json::<Value>(&messages, &opts)
            .await
            .map_err(|e| e.to_string())
    }

    fn expected_format(&self) -> Result<LlmExpectedFormat, String> {
        let schema = match self.step_name.as_str() {
            "summary" => react_core::schema_registry::OpenAiStrictSchema::for_type::<
                ReviewSummaryOutput,
            >(format!("{}.summary_output.v1", self.prompt_id)),
            "batch" => {
                react_core::schema_registry::OpenAiStrictSchema::for_type::<ReviewBatchOutput>(
                    format!("{}.batch_output.v1", self.prompt_id),
                )
            }
            _ => react_core::schema_registry::OpenAiStrictSchema::for_type::<ReviewUnifyOutput>(
                format!("{}.unify_output.v1", self.prompt_id),
            ),
        }
        .map_err(|e| e.to_string())?;
        Ok(LlmExpectedFormat::JsonSchema(schema))
    }

    fn parse_typed<T: DeserializeOwned>(&self, value: Value, label: &str) -> Result<T, String> {
        serde_json::from_value(value)
            .map_err(|e| format!("review {label} output did not match schema: {e}"))
    }
}

async fn load_global_semantic_context_json(actx: &AgentCtx, sctx: &SuiteCtx) -> serde_json::Value {
    let key = sctx.keyspace().scoped_key(
        sctx.scope(),
        &[
            "semantic",
            &format!(
                "{}.yaml",
                encode_key_component(crate::providers::GLOBAL_SEMANTIC_DATASET_ID)
            ),
        ],
    );
    actx.storage()
        .get_json(&key)
        .await
        .ok()
        .unwrap_or(Value::Null)
}

fn include_global_semantic_context(phase: Phase) -> bool {
    // Global semantic context is intended for GOLD model planning/review only.
    // It must not be injected into cleanse (silver) prompts.
    matches!(phase, Phase::ModelReview)
}

fn deterministic_unify_defaults(
    phase: Phase,
    all_batch_notes: &[Value],
) -> (ReviewTier, Vec<String>) {
    let tier = match phase {
        Phase::CleanseReview => ReviewTier::Silver,
        Phase::ModelReview => ReviewTier::Gold,
        _ => ReviewTier::Unknown,
    };

    let mut dataset_ids: Vec<String> = all_batch_notes
        .iter()
        .flat_map(|v| {
            v.get("batch_items")
                .and_then(|vv| vv.as_array())
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
        })
        .collect();
    dataset_ids.sort();
    dataset_ids.dedup();

    (tier, dataset_ids)
}

/// Resolved fields for a single cleanse batch item.
type CleanseBatchResolved = (
    String,                                     // dataset_id
    String,                                     // expected_path
    Vec<String>,                                // invariants
    Option<de_plan::CleanseImplementationSpec>, // implementation_spec
    Vec<crate::plan_types::SourceColumnDef>,    // source_schema
);

fn resolve_cleanse_batch_paths(
    plan: &de_plan::CleansePlan,
    batch: &[String],
) -> Vec<CleanseBatchResolved> {
    let mut out = Vec::new();
    for ds in batch.iter() {
        if let Some(t) = plan.tasks.iter().find(|t| t.dataset_id == *ds) {
            out.push((
                ds.clone(),
                t.expected_model_path.clone().unwrap_or_default(),
                t.invariants.clone(),
                t.implementation_spec.clone(),
                t.source_schema.clone(),
            ));
        } else {
            out.push((ds.clone(), String::new(), vec![], None, vec![]));
        }
    }
    out
}

/// Resolved fields for a single model batch item.
type ModelBatchResolved = (
    String,                                   // name
    String,                                   // expected_path
    Vec<String>,                              // invariants
    Option<de_plan::ModelImplementationSpec>, // implementation_spec
    Vec<crate::plan_types::SourceColumnDef>,  // source_schema
);

fn resolve_model_batch_paths(
    plan: &de_plan::ModelPlan,
    batch: &[String],
) -> Vec<ModelBatchResolved> {
    let mut out = Vec::new();
    for name in batch.iter() {
        if let Some(t) = plan.tasks.iter().find(|t| t.name == *name) {
            out.push((
                name.clone(),
                t.expected_model_path.clone().unwrap_or_default(),
                t.invariants.clone(),
                t.implementation_spec.clone(),
                t.source_schema.clone(),
            ));
        } else {
            out.push((name.clone(), String::new(), vec![], None, vec![]));
        }
    }
    out
}

type PlanAndBatches = (
    Option<PlanKind>,
    Option<String>,
    Vec<Vec<String>>,
    Option<Value>,
);

fn load_cleanse_plan_and_batches_from(p: de_plan::CleansePlan) -> PlanAndBatches {
    let batches = if !p.batches.is_empty() {
        p.batches.clone()
    } else {
        vec![]
    };
    let key = p.plan_key.clone();
    let mut map: Vec<Value> = Vec::new();
    for b in batches.iter() {
        for (ds, path, inv, implementation_spec, source_schema) in
            resolve_cleanse_batch_paths(&p, b)
        {
            let schema_cols: Vec<Value> = source_schema
                .iter()
                .map(|c| serde_json::json!({"name": &c.name, "type": &c.data_type}))
                .collect();
            map.push(serde_json::json!({
                "item": ds,
                "expected_model_path": path,
                "invariants": inv,
                "implementation_spec": implementation_spec,
                "plan_source_schema": schema_cols
            }));
        }
    }
    (
        Some(PlanKind::Cleanse),
        Some(key),
        batches,
        Some(Value::Array(map)),
    )
}

fn load_model_plan_and_batches_from(p: de_plan::ModelPlan) -> PlanAndBatches {
    let batches = if !p.batches.is_empty() {
        p.batches.clone()
    } else {
        vec![]
    };
    let key = p.plan_key.clone();
    let mut map: Vec<Value> = Vec::new();
    for b in batches.iter() {
        for (name, path, inv, implementation_spec, source_schema) in
            resolve_model_batch_paths(&p, b)
        {
            let schema_cols: Vec<Value> = source_schema
                .iter()
                .map(|c| serde_json::json!({"name": &c.name, "type": &c.data_type}))
                .collect();
            map.push(serde_json::json!({
                "item": name,
                "expected_model_path": path,
                "invariants": inv,
                "implementation_spec": implementation_spec,
                "plan_source_schema": schema_cols
            }));
        }
    }
    (
        Some(PlanKind::Model),
        Some(key),
        batches,
        Some(Value::Array(map)),
    )
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
    let Some(q) = crate::ctx_ext::sctx_query(sctx) else {
        return serde_json::json!({"ok": false, "error": "query provider missing"});
    };
    let tool = SqlSchemaTool {
        query: q.clone(),
        datasets: crate::ctx_ext::sctx_datasets(sctx),
        catalog: crate::ctx_ext::sctx_catalog(sctx),
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
    let manifest_indexes = facts::load_manifest_indexes(actx).await;

    // Resolve refs via manifest model index.
    let ref_fqns =
        facts::resolve_model_names_to_fqns_from_index(&manifest_indexes.models, &ref_names);
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
    for (src_name, table_name) in source_calls.into_iter().take(25) {
        if let Some(fqn) = manifest_indexes
            .sources
            .get(&(src_name.clone(), table_name.clone()))
        {
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

struct ProjectSummary {
    project_notes: Vec<String>,
}

async fn build_project_summary(
    actx: &AgentCtx,
    thread_store: &ThreadStore,
    thread_id: &str,
    phase: Phase,
    original_question_with_context: &str,
    plan_kind: Option<PlanKind>,
    plan_key: Option<&str>,
    global_ctx_block: &str,
) -> Result<ProjectSummary, String> {
    let proj_files = build_project_context(actx).await;
    let staging_paths = list_model_files(actx, "models/staging/", 2000).await;
    let marts_paths = list_model_files(actx, "models/marts/", 2000).await;
    let core_paths = list_model_files(actx, "models/core/", 2000).await;

    let manifest_meta = read_project_json_pointer(actx, "target/manifest.json", "/metadata").await;
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
        meta = serde_json::to_string_pretty(&manifest_meta_small)
            .unwrap_or_else(|_| "null".to_string()),
    );

    let config = ReviewLlmConfig::for_phase(phase, "summary");
    let summary_v = config.call(actx, thread_id, summary_user).await?;
    let summary_out: ReviewSummaryOutput = config.parse_typed(summary_v, "summary")?;
    let project_notes = clamp_lines(summary_out.project_notes, MAX_PROJECT_NOTES);

    append_review_step(
        thread_store,
        thread_id,
        phase,
        "review",
        crate::progress_controller::PhaseTransition::ReviewProjectSummary.as_reason_str(),
        serde_json::json!({
            "project_notes": project_notes,
        }),
    )
    .await?;
    if let (Some(pk), Some(key)) = (plan_kind, plan_key) {
        persist_review_summary_to_plan(actx, phase, pk, key, project_notes.clone()).await?;
    }

    Ok(ProjectSummary { project_notes })
}

#[allow(clippy::too_many_arguments)]
async fn review_single_batch(
    sctx: &SuiteCtx,
    actx: &AgentCtx,
    thread_store: &ThreadStore,
    thread_id: &str,
    phase: Phase,
    original_question_with_context: &str,
    plan_kind: Option<PlanKind>,
    plan_key: Option<&str>,
    global_ctx_block: &str,
    batch: &[String],
    batch_idx: usize,
    total_batches: usize,
    item_to_path_inv_notes: &Option<Value>,
) -> Result<Value, String> {
    let mut files: Vec<ProjectFile> = Vec::new();
    let mut batch_detail: Vec<ReviewBatchItem> = Vec::new();

    let mapping = item_to_path_inv_notes.clone().unwrap_or(Value::Null);
    let mapping_arr = mapping.as_array().cloned().unwrap_or_default();
    let map_for = |item: &str| -> Option<Value> {
        mapping_arr
            .iter()
            .find(|it| it.get("item").and_then(|v| v.as_str()) == Some(item))
            .cloned()
    };

    for item in batch.iter() {
        let mut expected_path = String::new();
        let mut invariants: Vec<String> = Vec::new();
        let mut implementation_spec: Value = Value::Null;
        if let Some(m) = map_for(item) {
            expected_path = m
                .get("expected_model_path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            invariants = str_list(m.get("invariants").unwrap_or(&Value::Null));
            implementation_spec = m.get("implementation_spec").cloned().unwrap_or(Value::Null);
        }
        if expected_path.trim().is_empty() {
            let it = item.trim();
            if it.starts_with("models/") || it.ends_with(".sql") || it.contains('/') {
                expected_path = it.to_string();
            } else {
                expected_path = format!("models/{}.sql", it);
            }
        }
        if let Some(f) = read_project_file(actx, &expected_path, 60_000).await {
            files.push(f);
        }

        // Prefer plan-recorded source_schema (captured at plan compilation).
        // Fall back to live warehouse query only when the plan doesn't have it.
        let plan_schema = map_for(item)
            .and_then(|m| m.get("plan_source_schema").cloned())
            .and_then(|v| {
                if v.as_array().map(|a| a.is_empty()).unwrap_or(true) {
                    None
                } else {
                    Some(v)
                }
            });
        let schema_obs = if let Some(ps) = plan_schema {
            ps
        } else if item.split('.').count() >= 3 {
            let raw = schema_for_dataset_fqn(sctx, actx, item).await;
            cap_schema_columns(&raw)
        } else {
            Value::Null
        };

        let deps = if let Some(f) = files
            .iter()
            .find(|ff| ff.path.trim() == expected_path.trim())
        {
            dependency_schemas_for_sql(sctx, actx, &f.content).await
        } else {
            Value::Array(vec![])
        };
        batch_detail.push(ReviewBatchItem {
            item: item.clone(),
            expected_model_path: expected_path,
            invariants,
            implementation_spec,
            authoritative_schema: schema_obs,
            dependency_schemas: deps,
        });
    }

    let batch_user = format!(
        "Phase: {phase}\nBatch {i}/{n}\n\nOriginal goal + review context:\n{q}\n\n{gctx_block}Batch items:\n{items}\n\nBatch file contents (bounded):\n{files}\n",
        phase = phase.as_str(),
        i = batch_idx + 1,
        n = total_batches,
        q = original_question_with_context,
        gctx_block = global_ctx_block,
        items = serde_json::to_string_pretty(&batch_detail).unwrap_or_else(|_| "[]".to_string()),
        files = render_files(&files),
    );

    let config = ReviewLlmConfig::for_phase(phase, "batch");
    let v = config.call(actx, thread_id, batch_user).await?;
    let batch_out: ReviewBatchOutput = config.parse_typed(v, "batch")?;
    let findings = clamp_lines(batch_out.findings, MAX_NOTES_PER_BATCH);

    let detail = serde_json::json!({
        "batch_idx": batch_idx,
        "batch_items": batch,
        "findings": findings,
    });
    append_review_step(
        thread_store,
        thread_id,
        phase,
        "review",
        crate::progress_controller::PhaseTransition::ReviewBatch.as_reason_str(),
        detail.clone(),
    )
    .await?;

    if let (Some(pk), Some(key)) = (plan_kind, plan_key) {
        persist_review_batch_to_plan(actx, pk, key, batch_idx, batch.to_vec(), findings.clone())
            .await?;
    }

    Ok(detail)
}

#[allow(clippy::too_many_arguments)]
async fn unify_reviews(
    sctx: &SuiteCtx,
    actx: &AgentCtx,
    thread_store: &ThreadStore,
    thread_id: &str,
    phase: Phase,
    original_question_with_context: &str,
    plan_kind: Option<PlanKind>,
    plan_key: Option<&str>,
    global_ctx_block: &str,
    summary: &ProjectSummary,
    all_batch_notes: &[Value],
) -> Result<Vec<FlowFrame>, String> {
    // Keep unify input bounded: it is easy for pretty-printed batch detail to grow large, which
    // increases the risk of truncated JSON responses.
    let mut unify_batches = all_batch_notes.to_vec();
    if unify_batches.len() > 20 {
        unify_batches = unify_batches[unify_batches.len() - 20..].to_vec();
    }
    let unify_batches = unify_batches
        .into_iter()
        .map(|v| {
            let batch_items = v.get("batch_items").cloned().unwrap_or(Value::Null);
            let findings = v.get("findings").cloned().unwrap_or(Value::Null);
            serde_json::json!({
                "batch_items": batch_items,
                "findings": findings,
            })
        })
        .collect::<Vec<_>>();
    let unify_user = format!(
        "Phase: {phase}\n\nOriginal goal + review context:\n{q}\n\n{gctx_block}Project notes:\n{proj}\n\nBatch findings:\n{batches}\n",
        phase = phase.as_str(),
        q = original_question_with_context,
        gctx_block = global_ctx_block,
        proj = serde_json::json!({"project_notes": summary.project_notes}),
        batches = serde_json::to_string_pretty(&unify_batches).unwrap_or_else(|_| "[]".to_string()),
    );

    let config = ReviewLlmConfig::for_phase(phase, "unify");
    let unify_v = config.call(actx, thread_id, unify_user).await?;
    let mut unify_out: ReviewUnifyOutput = config.parse_typed(unify_v, "unify")?;
    let (default_tier, default_dataset_ids) = deterministic_unify_defaults(phase, all_batch_notes);
    if unify_out.tier == ReviewTier::Unknown {
        unify_out.tier = default_tier;
    }
    if unify_out.dataset_ids.is_empty() {
        unify_out.dataset_ids = default_dataset_ids;
    } else {
        unify_out.dataset_ids.sort();
        unify_out.dataset_ids.dedup();
    }
    let final_review_text = unify_out.final_review_text.trim().to_string();
    if final_review_text.trim().is_empty() {
        return Err("batched review unify produced empty final_review_text".to_string());
    }

    let decision = unify_out.decision;
    let tier = unify_out.tier;
    let dataset_ids = unify_out.dataset_ids;
    let review_sha256 = react_core::llm_observability::sha256_hex_str(&final_review_text);
    let review_bytes = final_review_text.as_bytes().len() as u64;
    let review_key = {
        let root = actx
            .keyspace()
            .threads_prefix(actx.scope())
            .trim_end_matches("/threads")
            .trim_end_matches('/')
            .to_string();
        let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let sha8 = review_sha256.chars().take(8).collect::<String>();
        format!("{}/reviews/{}/{}_{}.txt", root, thread_id, ts, sha8)
    };
    sctx.storage()
        .put_bytes(&review_key, final_review_text.as_bytes(), "text/plain")
        .await
        .map_err(|e| format!("failed to persist final review artifact: {e}"))?;

    let review_ref = ReviewArtifactRef {
        key: review_key,
        sha256: review_sha256,
        bytes: review_bytes,
    };

    append_review_step(
        thread_store,
        thread_id,
        phase,
        "review",
        crate::progress_controller::PhaseTransition::ReviewFinalUnify.as_reason_str(),
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
    if let (Some(pk), Some(key)) = (plan_kind, plan_key) {
        persist_review_final_to_plan(
            actx,
            pk,
            key,
            decision,
            tier,
            dataset_ids.clone(),
            final_review_text.clone(),
        )
        .await?;
    }

    Ok(vec![FlowFrame::Complete {
        kind: FlowKind::new("generic"),
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

pub async fn run_batched_review(
    thread_id: &str,
    original_question_with_context: &str,
    phase: Phase,
    sctx: &SuiteCtx,
) -> Result<Vec<FlowFrame>, String> {
    let thread_store = ThreadStore::new(
        sctx.storage().clone(),
        sctx.scope().clone(),
        sctx.keyspace().clone(),
    );
    let mut actx = react_core::agent::AgentCtxBuilder::new(
        sctx.llm().clone(),
        sctx.storage().clone(),
        sctx.scope().clone(),
        sctx.keyspace().clone(),
        Arc::new(react_core::agent::DefaultPolicy),
    )
    .top_k(1)
    .per_step_timeout_secs(10)
    .max_steps(1)
    .thread_id(thread_id.to_string())
    .trace_tx(sctx.trace_tx().clone())
    .agent_name("review")
    .vector(sctx.vector().clone())
    .thread_store(thread_store.clone())
    .resolved_config(sctx.resolved_config().clone())
    .build();
    crate::ctx_ext::copy_capabilities_to_actx(sctx, &mut actx);

    // Determine which plan (if any) to use for batching + persistence target.
    let (plan_kind, plan_key, batches, item_to_path_inv_notes): (
        Option<PlanKind>,
        Option<String>,
        Vec<Vec<String>>,
        Option<Value>,
    ) = match phase {
        Phase::CleanseReview => {
            // Use the same plan-loading path as authoring (oldest active, non-terminal)
            // to ensure review sees the exact same plan version.
            if let Some(p) = de_plan::load_cleanse_plan(&actx)
                .await
                .map_err(|e| e.to_string())?
            {
                load_cleanse_plan_and_batches_from(p)
            } else {
                (None, None, vec![], None)
            }
        }
        Phase::ModelReview => {
            if let Some(p) = de_plan::load_model_plan(&actx)
                .await
                .map_err(|e| e.to_string())?
            {
                load_model_plan_and_batches_from(p)
            } else {
                (None, None, vec![], None)
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
            Phase::ModelReview => {
                let mut f = list_model_files(&actx, "models/marts/", 500).await;
                f.extend(list_model_files(&actx, "models/core/", 500).await);
                f.sort();
                f.dedup();
                f
            }
            _ => vec![],
        };
        chunk_vec(&files, DEFAULT_BATCH_SIZE)
    };

    // Precompute global semantic context block (shared across all LLM calls).
    let global_ctx_block = if include_global_semantic_context(phase) {
        let global_ctx = load_global_semantic_context_json(&actx, sctx).await;
        let global_ctx_txt =
            serde_json::to_string_pretty(&global_ctx).unwrap_or_else(|_| "null".to_string());
        format!(
            "IMMUTABLE CONTEXT (global_semantic_context):\n{gctx}\n\n",
            gctx = global_ctx_txt
        )
    } else {
        String::new()
    };

    // 1) Project summary.
    let summary = build_project_summary(
        &actx,
        &thread_store,
        thread_id,
        phase,
        original_question_with_context,
        plan_kind,
        plan_key.as_deref(),
        &global_ctx_block,
    )
    .await?;

    // 2) Per-batch reviews.
    let mut all_batch_notes: Vec<Value> = Vec::new();
    for (bidx, batch) in batches.iter().enumerate() {
        let detail = review_single_batch(
            sctx,
            &actx,
            &thread_store,
            thread_id,
            phase,
            original_question_with_context,
            plan_kind,
            plan_key.as_deref(),
            &global_ctx_block,
            batch,
            bidx,
            batches.len(),
            &item_to_path_inv_notes,
        )
        .await?;
        all_batch_notes.push(detail);
    }

    // 3) Unification.
    unify_reviews(
        sctx,
        &actx,
        &thread_store,
        thread_id,
        phase,
        original_question_with_context,
        plan_kind,
        plan_key.as_deref(),
        &global_ctx_block,
        &summary,
        &all_batch_notes,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use react_core::keyspace::{DefaultKeyspace, Keyspace};
    use react_core::scope::RequestScope;
    use react_module_storage_memory::InMemoryStorageAdapter;
    use serde_json::Value;
    use std::sync::{Arc, Mutex};

    fn collect_ref_sibling_violations(v: &Value, path: &str, out: &mut Vec<String>) {
        match v {
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    collect_ref_sibling_violations(item, &format!("{path}[{i}]"), out);
                }
            }
            Value::Object(map) => {
                if map.contains_key("$ref") && map.len() > 1 {
                    let mut keys: Vec<String> = map.keys().cloned().collect();
                    keys.sort();
                    out.push(format!("{path}: {:?}", keys));
                }
                for (k, child) in map {
                    collect_ref_sibling_violations(child, &format!("{path}.{k}"), out);
                }
            }
            _ => {}
        }
    }

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
            if let Some(u) = messages
                .iter()
                .find(|m| m.role == react_core::llm::ChatRole::User)
            {
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

    #[test]
    fn review_unify_output_schema_is_openai_compatible() {
        let schema =
            react_core::schema_registry::OpenAiStrictSchema::for_type::<ReviewUnifyOutput>(
                "suite.review_unify_output.v1",
            )
            .expect("schema");
        let mut violations: Vec<String> = Vec::new();
        collect_ref_sibling_violations(schema.schema(), "$", &mut violations);
        assert!(
            violations.is_empty(),
            "OpenAI-incompatible $ref sibling nodes found:\n{}",
            violations.join("\n")
        );
    }

    fn make_suite_ctx(
        storage: Arc<dyn react_core::storage::StorageAdapter>,
        llm: Arc<dyn react_core::llm::LargeLanguageModel>,
    ) -> SuiteCtx {
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        SuiteCtx::new(
            storage,
            Arc::new(react_core::provider_traits::NullSecretsProvider::default()),
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
                serde_json::json!({"project_notes":["n1"]}).to_string(),
                // batch 1
                serde_json::json!({"findings":["b1n"]}).to_string(),
                // batch 2
                serde_json::json!({"findings":["b2n"]}).to_string(),
                // unify
                serde_json::json!({"decision":"proceed","tier":"silver","dataset_ids":[],"final_review_text":"All good."}).to_string(),
            ])),
        });
        let sctx = make_suite_ctx(storage.clone(), llm);

        // Seed a minimal dbt project and a cleanse plan with 2 batches.
        let actx = react_core::agent::AgentCtxBuilder::new(
            sctx.llm().clone(),
            sctx.storage().clone(),
            sctx.scope().clone(),
            sctx.keyspace().clone(),
            Arc::new(react_core::agent::DefaultPolicy),
        )
        .top_k(1)
        .per_step_timeout_secs(1)
        .max_steps(1)
        .thread_id("tid".to_string())
        .agent_name("test".to_string())
        .vector(sctx.vector().clone())
        .build();

        // dbt files
        let base = actx
            .keyspace()
            .scoped_prefix(actx.scope(), &["dbt"])
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
            project_snapshot: Default::default(),
            tasks: vec![
                de_plan::CleanseTask {
                    dataset_id: "a.b.a".to_string(),
                    expected_model_path: Some("models/staging/stg_a.sql".to_string()),
                    invariants: vec!["inv".to_string()],
                    implementation_spec: Some(de_plan::CleanseImplementationSpec {
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
                    }),
                    source_schema: vec![],
                    status: de_plan::TaskStatus::Done,
                    checklist: vec![
                        de_plan::PlanChecklistItem {
                            checklist_item_id: "sql_model".to_string(),
                            label: "Author staging SQL".to_string(),
                            details: None,
                            status: de_plan::ChecklistItemStatus::Done,
                            origin: de_plan::ChecklistOrigin::Initial,
                            evidence: vec![],
                        },
                        de_plan::PlanChecklistItem {
                            checklist_item_id: "schema_contract".to_string(),
                            label: "Author schema contract".to_string(),
                            details: None,
                            status: de_plan::ChecklistItemStatus::Done,
                            origin: de_plan::ChecklistOrigin::Initial,
                            evidence: vec![],
                        },
                        de_plan::PlanChecklistItem {
                            checklist_item_id: "validate".to_string(),
                            label: "Validate model".to_string(),
                            details: None,
                            status: de_plan::ChecklistItemStatus::Done,
                            origin: de_plan::ChecklistOrigin::Initial,
                            evidence: vec![],
                        },
                    ],
                },
                de_plan::CleanseTask {
                    dataset_id: "a.b.b".to_string(),
                    expected_model_path: Some("models/staging/stg_b.sql".to_string()),
                    invariants: vec![],
                    implementation_spec: Some(de_plan::CleanseImplementationSpec {
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
                    }),
                    source_schema: vec![],
                    status: de_plan::TaskStatus::Done,
                    checklist: vec![
                        de_plan::PlanChecklistItem {
                            checklist_item_id: "sql_model".to_string(),
                            label: "Author staging SQL".to_string(),
                            details: None,
                            status: de_plan::ChecklistItemStatus::Done,
                            origin: de_plan::ChecklistOrigin::Initial,
                            evidence: vec![],
                        },
                        de_plan::PlanChecklistItem {
                            checklist_item_id: "schema_contract".to_string(),
                            label: "Author schema contract".to_string(),
                            details: None,
                            status: de_plan::ChecklistItemStatus::Done,
                            origin: de_plan::ChecklistOrigin::Initial,
                            evidence: vec![],
                        },
                        de_plan::PlanChecklistItem {
                            checklist_item_id: "validate".to_string(),
                            label: "Validate model".to_string(),
                            details: None,
                            status: de_plan::ChecklistItemStatus::Done,
                            origin: de_plan::ChecklistOrigin::Initial,
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
        assert!(matches!(out[0], FlowFrame::Complete { .. }));

        // Thread log step should store review by reference (not the full text).
        let thread_store = ThreadStore::new(
            storage.clone(),
            sctx.scope().clone(),
            sctx.keyspace().clone(),
        );
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
            .expect("load failed")
            .expect("plan");
        let review = loaded
            .project_snapshot
            .extra
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
        assert!(review.get("result").is_some());
    }

    #[tokio::test]
    async fn global_semantic_context_is_not_injected_for_cleanse_review_but_is_for_model_review() {
        let storage: Arc<dyn react_core::storage::StorageAdapter> =
            Arc::new(InMemoryStorageAdapter::default());

        // Seed global semantic context.
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let gkey = keyspace.scoped_key(
            &scope,
            &[
                "semantic",
                &format!(
                    "{}.yaml",
                    encode_key_component(crate::providers::GLOBAL_SEMANTIC_DATASET_ID)
                ),
            ],
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
            .scoped_prefix(&scope, &["dbt"])
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
                serde_json::json!({"project_notes":[]}).to_string(), // summary
                serde_json::json!({"decision":"proceed","tier":"silver","dataset_ids":[],"final_review_text":"ok"}).to_string(), // unify
            ])),
        });
        let sctx_cleanse = SuiteCtx::new(
            storage.clone(),
            Arc::new(react_core::provider_traits::NullSecretsProvider::default()),
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
                serde_json::json!({"project_notes":[]}).to_string(), // summary
                serde_json::json!({"decision":"proceed","tier":"gold","dataset_ids":[],"final_review_text":"ok"}).to_string(), // unify
            ])),
        });
        let sctx_model = SuiteCtx::new(
            storage.clone(),
            Arc::new(react_core::provider_traits::NullSecretsProvider::default()),
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

    #[test]
    fn deterministic_unify_defaults_derives_tier_and_dataset_ids() {
        let notes = vec![
            serde_json::json!({"batch_items":["a.b.c","x.y.z"],"findings":[]}),
            serde_json::json!({"batch_items":["x.y.z"],"findings":[]}),
        ];
        let (tier, dataset_ids) = deterministic_unify_defaults(Phase::CleanseReview, &notes);
        assert_eq!(tier, ReviewTier::Silver);
        assert_eq!(dataset_ids, vec!["a.b.c".to_string(), "x.y.z".to_string()]);
    }

    #[tokio::test]
    async fn unify_respects_llm_plan_change_decision() {
        let storage: Arc<dyn react_core::storage::StorageAdapter> =
            Arc::new(InMemoryStorageAdapter::default());
        let llm = Arc::new(ScriptedModel {
            replies: Arc::new(Mutex::new(vec![
                serde_json::json!({"project_notes":[]}).to_string(),
                serde_json::json!({"findings":["REQUIRES PLAN CHANGE: hallucinated fields"]}).to_string(),
                serde_json::json!({
                    "decision":"plan_change",
                    "tier":"silver",
                    "dataset_ids":["a.b.c"],
                    "final_review_text":"REQUIRES PLAN CHANGE: the approved plan references fields that do not exist."
                })
                .to_string(),
            ])),
        });
        let sctx = make_suite_ctx(storage.clone(), llm);

        let actx = react_core::agent::AgentCtxBuilder::new(
            sctx.llm().clone(),
            sctx.storage().clone(),
            sctx.scope().clone(),
            sctx.keyspace().clone(),
            Arc::new(react_core::agent::DefaultPolicy),
        )
        .top_k(1)
        .per_step_timeout_secs(1)
        .max_steps(1)
        .thread_id("tid_plan_change".to_string())
        .agent_name("test".to_string())
        .vector(sctx.vector().clone())
        .build();

        let base = actx
            .keyspace()
            .scoped_prefix(actx.scope(), &["dbt"])
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
        storage
            .put_bytes(
                &format!("{}/models/staging/stg_a.sql", base),
                "select 1 as a\n".as_bytes(),
                "text/plain",
            )
            .await
            .unwrap();

        let plan = de_plan::CleansePlan {
            plan_key: de_plan::new_cleanse_plan_key(&actx),
            status: de_plan::PlanStatus::Approved,
            project_snapshot: Default::default(),
            tasks: vec![de_plan::CleanseTask {
                dataset_id: "a.b.c".to_string(),
                expected_model_path: Some("models/staging/stg_a.sql".to_string()),
                invariants: vec![],
                implementation_spec: None,
                source_schema: vec![],
                status: de_plan::TaskStatus::Done,
                checklist: vec![],
            }],
            batches: vec![vec!["a.b.c".to_string()]],
            work_groups: vec![],
            mutations: vec![],
            progress: de_plan::PlanProgress::default(),
        };
        de_plan::save_cleanse_plan(&actx, &plan).await.unwrap();

        let out = run_batched_review("tid_plan_change", "goal", Phase::CleanseReview, &sctx)
            .await
            .expect("ok");
        let FlowFrame::Complete { payload, .. } = &out[0] else {
            panic!("expected complete frame");
        };
        let meta: crate::domain_types::ReviewDecisionMeta =
            serde_json::from_value(payload.get("meta").cloned().expect("meta"))
                .expect("typed meta");
        assert_eq!(
            meta.decision,
            crate::domain_types::ReviewDecision::PlanChange
        );
    }
}
