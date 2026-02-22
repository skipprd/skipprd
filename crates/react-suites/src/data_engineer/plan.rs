use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::data_engineer::naming;
use react_core::agent::AgentCtx;
use react_core::session::{ThreadLog, ThreadStep};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Draft,
    Approved,
    Completed,
    Cancelled,
}

impl Default for PlanStatus {
    fn default() -> Self {
        PlanStatus::Draft
    }
}

impl PlanStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, PlanStatus::Completed | PlanStatus::Cancelled)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Done,
    Blocked,
    /// Task was completed but requires correction / updates before the plan can proceed.
    NeedsUpdate,
}

impl Default for TaskStatus {
    fn default() -> Self {
        TaskStatus::Pending
    }
}

fn mark_task_in_progress(status: &mut TaskStatus) {
    // When we observe a tool call that targets a task, that indicates active work/rework.
    *status = TaskStatus::InProgress;
}

fn mark_task_needs_update(status: &mut TaskStatus) {
    // Any note/error attached to a task should surface as "needs_update" for UI remediation.
    *status = TaskStatus::NeedsUpdate;
}

fn file_stem(s: &str) -> Option<String> {
    std::path::Path::new(s)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.trim().is_empty())
}

fn extract_dbt_files_paths(op: &str, args: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    match op {
        "patch" => {
            // Single-file target (required by the patch contract).
            if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
                let p = p.trim();
                if !p.is_empty() {
                    out.push(p.to_string());
                }
            }
        }
        "mv" => {
            // Prefer destination path for progress tracking.
            if let Some(p) = args.get("to").and_then(|v| v.as_str()) {
                let p = p.trim();
                if !p.is_empty() {
                    out.push(p.to_string());
                }
            }
        }
        "rm" => {
            // Removals generally shouldn't advance authoring checklists, but we still capture the target.
            if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
                let p = p.trim();
                if !p.is_empty() {
                    out.push(p.to_string());
                }
            }
        }
        _ => {}
    }

    out.sort();
    out.dedup();
    out
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlanProgress {
    /// Thread step index cursor: when updating from the thread log, only consider steps after this
    /// index (exclusive). This keeps progress updates idempotent and cheap.
    #[serde(default)]
    pub last_applied_step_idx: usize,

    /// Consecutive failed batch attempts (resets on a fully successful attempt).
    /// Used to avoid infinite remediation loops in plan-batched authoring.
    #[serde(default)]
    pub consecutive_batch_failures: usize,

    /// Total failed batch attempts across the plan lifetime (diagnostics only).
    #[serde(default)]
    pub total_batch_failures: usize,
}

impl Default for PlanProgress {
    fn default() -> Self {
        Self {
            last_applied_step_idx: 0,
            consecutive_batch_failures: 0,
            total_batch_failures: 0,
        }
    }
}

/// Audit trail for plan mutations (repairs, pruning, canonicalization).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanMutation {
    pub ts: String,
    /// Machine-readable reason code, e.g. "plan_repair_semantic".
    pub reason_code: String,
    /// Additional structured detail (best-effort; keep small).
    #[serde(default)]
    pub detail: Value,
}

fn push_plan_mutation(muts: &mut Vec<PlanMutation>, reason_code: &str, detail: Value) {
    muts.push(PlanMutation {
        ts: chrono::Utc::now().to_rfc3339(),
        reason_code: reason_code.to_string(),
        detail,
    });
    // Keep bounded.
    if muts.len() > 50 {
        let keep = muts.split_off(muts.len().saturating_sub(50));
        *muts = keep;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecklistOrigin {
    Initial,
    ReviewActionable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecklistItemStatus {
    Pending,
    InProgress,
    Done,
    Blocked,
    NeedsUpdate,
}

impl Default for ChecklistItemStatus {
    fn default() -> Self {
        ChecklistItemStatus::Pending
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChecklistEvidence {
    pub kind: String,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_id: Option<String>,
    pub step_idx: usize,
    #[serde(default)]
    pub ts: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanChecklistItem {
    pub checklist_item_id: String,
    pub label: String,
    #[serde(default)]
    pub details: Option<String>,
    #[serde(default)]
    pub status: ChecklistItemStatus,
    pub origin: ChecklistOrigin,
    #[serde(default)]
    pub origin_step_idx: Option<usize>,
    #[serde(default)]
    pub evidence: Vec<ChecklistEvidence>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkGroupKind {
    AuthorSql,
    AuthorSchema,
    Validate,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkGroupItemRef {
    pub task_id: String,
    pub checklist_item_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanWorkGroup {
    pub group_id: String,
    pub label: String,
    pub kind: WorkGroupKind,
    pub items: Vec<WorkGroupItemRef>,
    #[serde(default)]
    pub depends_on_group_ids: Option<Vec<String>>,
}

// -----------------------
// Design-first plan spec
// -----------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    Raw,
    Clean,
    Derived,
    QualityFlag,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputFieldSpec {
    /// Output column name.
    pub name: String,
    /// The role of the field in the model interface.
    pub kind: FieldKind,
    /// Upstream source columns (or prior-stage columns) this field depends on.
    #[serde(default)]
    pub source_columns: Vec<String>,
    /// A concise, imperative expression or transformation contract.
    /// This is not required to be dialect-perfect SQL; it is the design contract that authoring
    /// should implement faithfully.
    pub expression: String,
    /// Optional intended type (best-effort). Prefer empty over guessing.
    #[serde(default)]
    pub data_type: Option<String>,
    /// Whether the field is allowed to be NULL in the output.
    #[serde(default)]
    pub nullable: bool,
    /// Optional one-line meaning / usage guidance.
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleanseImplementationSpec {
    /// Schema version for the spec itself (not the overall plan). Enables future evolution.
    pub spec_version: i64,
    /// Row-preserving is mandatory for SILVER (models/staging/).
    pub row_preserving: bool,
    /// Explicit output field design for this staging model.
    pub output_fields: Vec<OutputFieldSpec>,
    /// Optional explicit guardrails to prevent accidental grain enforcement (e.g. "no filtering", "no dedup").
    #[serde(default)]
    pub prohibited_ops: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JoinSpec {
    pub right_model: String,
    /// join type: inner|left|right|full (design intent)
    pub join_type: String,
    /// Join keys contract. Example: ["customer_id = customer_id"] or ["order_id = order_id"].
    pub on: Vec<String>,
    /// Optional cardinality expectation (e.g. "many_to_one", "one_to_many").
    #[serde(default)]
    pub cardinality: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricSpec {
    pub name: String,
    /// One-line definition (formula + inclusion/exclusion rules).
    pub definition: String,
    /// Optional caveats/assumptions (bounded).
    #[serde(default)]
    pub caveats: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelImplementationSpec {
    pub spec_version: i64,
    /// Grain contract, e.g. "1 row per order_id".
    pub grain: String,
    /// Inputs should align with task.inputs (stg_* only).
    #[serde(default)]
    pub inputs: Vec<String>,
    /// Join contract for composing the model.
    #[serde(default)]
    pub joins: Vec<JoinSpec>,
    /// Metric definitions for business use.
    #[serde(default)]
    pub metrics: Vec<MetricSpec>,
    /// Expected output schema contract (columns + semantics).
    #[serde(default)]
    pub output_fields: Vec<OutputFieldSpec>,
    /// Assumptions that require validation probes before downstream reliance.
    #[serde(default)]
    pub assumptions: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleanseTask {
    pub dataset_id: String,
    /// Expected DBT model file path for this dataset (project-relative).
    /// Typically models/staging/stg_<schema>_<table>.sql
    #[serde(default)]
    pub expected_model_path: Option<String>,
    /// High-signal planning output: inferred grain, keys, time fields, and any known hazards.
    /// This should be grounded in schema + stats + sample probes.
    #[serde(default)]
    pub invariants: Vec<String>,
    /// Design-first implementation contract for this staging model.
    pub implementation_spec: CleanseImplementationSpec,
    #[serde(default)]
    pub status: TaskStatus,
    #[serde(default)]
    pub checklist: Vec<PlanChecklistItem>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleansePlan {
    /// Storage key where this plan is persisted.
    #[serde(default)]
    pub plan_key: String,
    pub status: PlanStatus,
    /// Optional snapshot of the DBT project state when this plan was created.
    #[serde(default)]
    pub project_snapshot: Value,
    pub tasks: Vec<CleanseTask>,
    /// Ordered batches of dataset_ids; each batch MUST have at most 5 items.
    pub batches: Vec<Vec<String>>,
    /// Ordered work groups (checklist-driven). This is the canonical execution plan for the UI.
    #[serde(default)]
    pub work_groups: Vec<PlanWorkGroup>,
    /// Audit trail for deterministic + LLM-backed repairs.
    #[serde(default)]
    pub mutations: Vec<PlanMutation>,
    #[serde(default)]
    pub progress: PlanProgress,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelTask {
    pub name: String,
    #[serde(default)]
    pub folder: String, // "marts" | "core"
    #[serde(default)]
    pub goal: String,
    #[serde(default)]
    pub inputs: Vec<String>,
    /// Expected DBT model file path for this model (project-relative).
    #[serde(default)]
    pub expected_model_path: Option<String>,
    /// High-signal planning output: grain, join keys, time semantics, uniqueness expectations.
    #[serde(default)]
    pub invariants: Vec<String>,
    /// Design-first implementation contract for this model.
    pub implementation_spec: ModelImplementationSpec,
    #[serde(default)]
    pub status: TaskStatus,
    #[serde(default)]
    pub checklist: Vec<PlanChecklistItem>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPlan {
    /// Storage key where this plan is persisted.
    #[serde(default)]
    pub plan_key: String,
    pub status: PlanStatus,
    /// Optional snapshot of the DBT project state when this plan was created.
    #[serde(default)]
    pub project_snapshot: Value,
    pub tasks: Vec<ModelTask>,
    /// Ordered batches of model names; each batch MUST have at most 5 items.
    pub batches: Vec<Vec<String>>,
    /// Ordered work groups (checklist-driven). This is the canonical execution plan for the UI.
    #[serde(default)]
    pub work_groups: Vec<PlanWorkGroup>,
    /// Audit trail for deterministic + LLM-backed repairs.
    #[serde(default)]
    pub mutations: Vec<PlanMutation>,
    #[serde(default)]
    pub progress: PlanProgress,
}

fn parse_dataset_id_3(s: &str) -> Option<(String, String, String)> {
    let parts: Vec<&str> = s.trim().split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let cat = parts[0].trim();
    let schema = parts[1].trim();
    let table = parts[2].trim();
    if cat.is_empty() || schema.is_empty() || table.is_empty() {
        return None;
    }
    Some((cat.to_string(), schema.to_string(), table.to_string()))
}

fn ensure_expected_model_paths_cleanse(ctx: Option<&AgentCtx>, plan: &mut CleansePlan) -> bool {
    let mut changed = false;
    for t in plan.tasks.iter_mut() {
        let Some((_cat, schema, table)) = parse_dataset_id_3(&t.dataset_id) else {
            continue;
        };
        let canonical = naming::canonical_staging_rel_path(&schema, &table);
        let cur = t
            .expected_model_path
            .as_deref()
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if cur != canonical {
            changed = true;
            let from = if cur.is_empty() { "(missing)".to_string() } else { cur };
            let msg = format!(
                "canonicalized cleanse expected_model_path for {}: {} -> {}",
                t.dataset_id, from, canonical
            );
            tracing::warn!("{}", msg);
            if let Some(tx) = ctx.and_then(|c| c.trace_tx.as_ref()) {
                let _ = tx.send(msg);
            }
            t.expected_model_path = Some(canonical);
        }
    }
    changed
}

fn ensure_expected_model_paths_model(plan: &mut ModelPlan) {
    for t in plan.tasks.iter_mut() {
        let missing = t
            .expected_model_path
            .as_deref()
            .map(|s| s.trim().is_empty())
            .unwrap_or(true);
        if !missing {
            continue;
        }
        let folder = if t.folder.trim().is_empty() {
            "marts"
        } else {
            t.folder.trim()
        };
        t.expected_model_path = Some(format!("models/{}/{}.sql", folder, t.name.trim()));
    }
}

pub fn prune_cleanse_plan_to_grounded_raw_datasets(
    plan: &mut CleansePlan,
    allowed_raw: &std::collections::BTreeSet<String>,
) {
    // Drop tasks that reference unproven datasets.
    let mut removed: Vec<String> = Vec::new();
    plan.tasks.retain(|t| {
        let keep = allowed_raw.contains(t.dataset_id.trim());
        if !keep {
            removed.push(t.dataset_id.clone());
        }
        keep
    });

    // Drop pruned dataset_ids from batches.
    for b in plan.batches.iter_mut() {
        b.retain(|ds| allowed_raw.contains(ds.trim()));
    }
    plan.batches.retain(|b| !b.is_empty());

    if !removed.is_empty() {
        removed.sort();
        removed.dedup();
        // Record provenance in project_snapshot (best-effort) without growing unbounded.
        if plan.project_snapshot.is_null() {
            plan.project_snapshot = serde_json::json!({});
        }
        if let Some(obj) = plan.project_snapshot.as_object_mut() {
            obj.insert(
                "pruned_dataset_ids".to_string(),
                serde_json::json!({
                    "count": removed.len(),
                    "items": removed.into_iter().take(50).collect::<Vec<_>>()
                }),
            );
        }
    }
}

pub fn prune_model_plan_to_grounded_staging_models(
    plan: &mut ModelPlan,
    allowed_stg_models: &std::collections::BTreeSet<String>,
) {
    // Drop tasks that are not grounded in existing staging models and/or violate inputs constraints.
    let mut removed: Vec<String> = Vec::new();
    plan.tasks.retain(|t| {
        let name = t.name.trim();
        if name.is_empty() {
            removed.push(t.name.clone());
            return false;
        }
        // Require that the task itself is a valid model name. (We don't require stg_ prefix for gold outputs.)
        // But we do require its inputs to be staging models only.
        let mut ok_inputs = true;
        for inp in t.inputs.iter() {
            let it = inp.trim();
            if it.is_empty() {
                continue;
            }
            if !it.to_ascii_lowercase().starts_with("stg_") {
                ok_inputs = false;
                break;
            }
            if !allowed_stg_models.contains(it) {
                ok_inputs = false;
                break;
            }
        }
        if !ok_inputs {
            removed.push(t.name.clone());
            return false;
        }
        true
    });

    for b in plan.batches.iter_mut() {
        b.retain(|name| !name.trim().is_empty());
        b.retain(|name| plan.tasks.iter().any(|t| t.name == *name));
    }
    plan.batches.retain(|b| !b.is_empty());

    if !removed.is_empty() {
        removed.sort();
        removed.dedup();
        if plan.project_snapshot.is_null() {
            plan.project_snapshot = serde_json::json!({});
        }
        if let Some(obj) = plan.project_snapshot.as_object_mut() {
            obj.insert(
                "pruned_model_tasks".to_string(),
                serde_json::json!({
                    "count": removed.len(),
                    "items": removed.into_iter().take(50).collect::<Vec<_>>()
                }),
            );
        }
    }
}

fn thread_dir(ctx: &AgentCtx) -> String {
    ctx.thread_id
        .as_deref()
        .unwrap_or("no_thread")
        .trim()
        .to_string()
}

fn utc_timestamp_compact() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

fn plans_thread_prefix(ctx: &AgentCtx) -> String {
    // Plans are stored alongside other top-level resources (threads/, dbt/, etc),
    // NOT under dbt/.
    let root = ctx
        .keyspace
        .threads_prefix(&ctx.scope)
        .trim_end_matches("/threads")
        .trim_end_matches('/')
        .to_string();
    let tid = thread_dir(ctx);
    format!("{}/plans/{}/", root, tid)
}

pub fn new_cleanse_plan_key(ctx: &AgentCtx) -> String {
    let pref = plans_thread_prefix(ctx);
    format!("{}{}_cleanse.json", pref, utc_timestamp_compact())
}

pub fn new_model_plan_key(ctx: &AgentCtx) -> String {
    let pref = plans_thread_prefix(ctx);
    format!("{}{}_model.json", pref, utc_timestamp_compact())
}

async fn list_plan_keys(ctx: &AgentCtx, suffix: &str) -> Vec<String> {
    let pref = plans_thread_prefix(ctx);
    let mut keys = ctx.storage.list_prefix(&pref).await.unwrap_or_default();
    keys.retain(|k| k.ends_with(suffix));
    keys.sort();
    keys
}

/// Return the newest (lexicographically largest) plan key for this thread, regardless of terminal status.
///
/// Notes:
/// - Plan keys are timestamp-prefixed, so lexicographic ordering matches recency.
/// - This is intentionally different from `load_cleanse_plan`/`load_model_plan`, which prefer the
///   oldest non-terminal plan to match the deterministic pipeline behavior.
pub async fn newest_plan_key_any(ctx: &AgentCtx, suffix: &str) -> Option<String> {
    let keys = list_plan_keys(ctx, suffix).await;
    keys.last().cloned()
}

async fn oldest_active_cleanse_plan_key(ctx: &AgentCtx) -> Option<String> {
    let keys = list_plan_keys(ctx, "_cleanse.json").await;
    for k in keys {
        if let Ok(bytes) = ctx.storage.get_bytes(&k).await {
            if let Ok(p) = serde_json::from_slice::<CleansePlan>(&bytes) {
                if !p.status.is_terminal() {
                    return Some(k);
                }
            }
        }
    }
    None
}

async fn oldest_active_model_plan_key(ctx: &AgentCtx) -> Option<String> {
    let keys = list_plan_keys(ctx, "_model.json").await;
    for k in keys {
        if let Ok(bytes) = ctx.storage.get_bytes(&k).await {
            if let Ok(p) = serde_json::from_slice::<ModelPlan>(&bytes) {
                if !p.status.is_terminal() {
                    return Some(k);
                }
            }
        }
    }
    None
}

pub async fn load_cleanse_plan(ctx: &AgentCtx) -> Option<CleansePlan> {
    let key = oldest_active_cleanse_plan_key(ctx).await?;
    load_cleanse_plan_by_key(ctx, &key).await
}

/// Load the cleanse plan for this thread, preferring the oldest non-terminal plan.
///
/// If there is no active plan (e.g. a restart after we marked it Completed), fall back to the
/// newest plan key so "continue" can rehydrate context and progress deterministically.
pub async fn load_cleanse_plan_any(ctx: &AgentCtx) -> Option<CleansePlan> {
    if let Some(p) = load_cleanse_plan(ctx).await {
        return Some(p);
    }
    let key = newest_plan_key_any(ctx, "_cleanse.json").await?;
    load_cleanse_plan_by_key(ctx, &key).await
}

pub async fn load_cleanse_plan_by_key(ctx: &AgentCtx, key: &str) -> Option<CleansePlan> {
    let bytes = ctx.storage.get_bytes(key).await.ok()?;
    let mut p = serde_json::from_slice::<CleansePlan>(&bytes).ok()?;
    if p.plan_key.trim().is_empty() {
        p.plan_key = key.to_string();
    }
    let changed = ensure_expected_model_paths_cleanse(Some(ctx), &mut p);
    if changed {
        // Best-effort persist so subsequent loads (and humans) see the canonical path.
        let _ = save_cleanse_plan(ctx, &p).await;
    }
    Some(p)
}

pub async fn save_cleanse_plan(ctx: &AgentCtx, plan: &CleansePlan) -> Result<(), String> {
    if plan.plan_key.trim().is_empty() {
        return Err("cleanse plan missing plan_key".to_string());
    }
    let bytes = serde_json::to_vec_pretty(plan).map_err(|e| e.to_string())?;
    ctx.storage
        .put_bytes(&plan.plan_key, &bytes, "application/json")
        .await
        .map_err(|e| e.to_string())
}

pub async fn load_model_plan(ctx: &AgentCtx) -> Option<ModelPlan> {
    let key = oldest_active_model_plan_key(ctx).await?;
    load_model_plan_by_key(ctx, &key).await
}

/// Load the model plan for this thread, preferring the oldest non-terminal plan.
///
/// If there is no active plan, fall back to the newest plan key so "continue" can rehydrate.
pub async fn load_model_plan_any(ctx: &AgentCtx) -> Option<ModelPlan> {
    if let Some(p) = load_model_plan(ctx).await {
        return Some(p);
    }
    let key = newest_plan_key_any(ctx, "_model.json").await?;
    load_model_plan_by_key(ctx, &key).await
}

pub async fn load_model_plan_by_key(ctx: &AgentCtx, key: &str) -> Option<ModelPlan> {
    let bytes = ctx.storage.get_bytes(key).await.ok()?;
    let mut p = serde_json::from_slice::<ModelPlan>(&bytes).ok()?;
    if p.plan_key.trim().is_empty() {
        p.plan_key = key.to_string();
    }
    ensure_expected_model_paths_model(&mut p);
    Some(p)
}

pub async fn save_model_plan(ctx: &AgentCtx, plan: &ModelPlan) -> Result<(), String> {
    if plan.plan_key.trim().is_empty() {
        return Err("model plan missing plan_key".to_string());
    }
    let bytes = serde_json::to_vec_pretty(plan).map_err(|e| e.to_string())?;
    ctx.storage
        .put_bytes(&plan.plan_key, &bytes, "application/json")
        .await
        .map_err(|e| e.to_string())
}

#[derive(Clone, Debug)]
pub struct PlanSemanticValidation {
    pub ok: bool,
    pub errors: Vec<String>,
}

fn is_runnable_checklist_status(s: ChecklistItemStatus) -> bool {
    matches!(
        s,
        ChecklistItemStatus::Pending | ChecklistItemStatus::InProgress | ChecklistItemStatus::NeedsUpdate
    )
}

fn excerpt_for_prompt(s: &str, max_chars: usize) -> String {
    let t = s.trim();
    if max_chars == 0 || t.is_empty() {
        return String::new();
    }
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

fn parse_json_object_lenient(text: &str) -> Result<Value, String> {
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        return Ok(v);
    }
    let s = text.trim();
    let st = s.find('{').ok_or_else(|| "no '{' found".to_string())?;
    let en = s.rfind('}').ok_or_else(|| "no '}' found".to_string())?;
    if en <= st {
        return Err("invalid brace span".to_string());
    }
    serde_json::from_str::<Value>(&s[st..=en]).map_err(|e| e.to_string())
}

pub fn validate_cleanse_plan_semantics(plan: &CleansePlan) -> PlanSemanticValidation {
    let mut errors: Vec<String> = Vec::new();
    if plan.plan_key.trim().is_empty() {
        errors.push("plan_key is missing".to_string());
    }
    if plan.tasks.is_empty() {
        errors.push("tasks is empty".to_string());
    }
    for (bi, b) in plan.batches.iter().enumerate() {
        if b.len() > 5 {
            errors.push(format!("batches[{bi}] has >5 items (len={})", b.len()));
        }
    }
    for t in plan.tasks.iter() {
        if t.dataset_id.trim().is_empty() {
            errors.push("task.dataset_id is empty".to_string());
            continue;
        }
        // Design-first requirement: runnable tasks must have explicit implementation spec.
        if t.implementation_spec.spec_version <= 0 {
            errors.push(format!(
                "{}: implementation_spec.spec_version must be >0",
                t.dataset_id
            ));
        }
        if !t.implementation_spec.row_preserving {
            errors.push(format!(
                "{}: implementation_spec.row_preserving must be true for cleanse/silver",
                t.dataset_id
            ));
        }
        if t.implementation_spec.output_fields.is_empty() {
            errors.push(format!(
                "{}: implementation_spec.output_fields is empty (design detail required)",
                t.dataset_id
            ));
        }
        let sql_status = checklist_status(&t.checklist, CHECKLIST_SQL_MODEL);
        if is_runnable_checklist_status(sql_status) {
            let missing_path = t
                .expected_model_path
                .as_deref()
                .map(|s| s.trim().is_empty())
                .unwrap_or(true);
            if missing_path {
                errors.push(format!(
                    "{}: expected_model_path missing for runnable task",
                    t.dataset_id
                ));
            }
        }
    }
    // Batch references should exist in tasks.
    for (bi, b) in plan.batches.iter().enumerate() {
        for ds in b.iter() {
            if plan.tasks.iter().find(|t| t.dataset_id == *ds).is_none() {
                errors.push(format!(
                    "batches[{bi}] references dataset_id not present in tasks: {ds}"
                ));
            }
        }
    }
    // Work-group refs should exist in tasks.
    for g in plan.work_groups.iter() {
        if g.items.len() > 5 {
            errors.push(format!(
                "work_group {} has >5 items (len={})",
                g.group_id,
                g.items.len()
            ));
        }
        for it in g.items.iter() {
            if plan.tasks.iter().find(|t| t.dataset_id == it.task_id).is_none() {
                errors.push(format!(
                    "work_group {} references unknown dataset_id task_id={}",
                    g.group_id, it.task_id
                ));
            }
            let cid = it.checklist_item_id.trim();
            if cid.is_empty() {
                errors.push(format!(
                    "work_group {} item for task_id={} is missing checklist_item_id",
                    g.group_id, it.task_id
                ));
                continue;
            }
            if let Some(t) = plan.tasks.iter().find(|t| t.dataset_id == it.task_id) {
                let exists = t.checklist.iter().any(|x| x.checklist_item_id == cid);
                if !exists {
                    errors.push(format!(
                        "work_group {} references checklist_item_id '{}' not present in task checklist: {}",
                        g.group_id, cid, it.task_id
                    ));
                }
            }
        }
    }
    PlanSemanticValidation {
        ok: errors.is_empty(),
        errors,
    }
}

fn default_silver_passthrough_field_spec() -> OutputFieldSpec {
    OutputFieldSpec {
        name: "__all_source_columns__".to_string(),
        kind: FieldKind::Raw,
        source_columns: vec!["*".to_string()],
        expression: "Pass through all raw/bronze source columns unchanged (do not drop columns); add cleaned/cast columns alongside raw as needed.".to_string(),
        data_type: None,
        nullable: true,
        description: Some(
            "Default silver contract: preserve all raw columns; do not filter/dedup in silver."
                .to_string(),
        ),
    }
}

fn ensure_silver_passthrough_present(output_fields: &mut Vec<OutputFieldSpec>) {
    let has_star = output_fields.iter().any(|f| {
        f.name.trim() == "__all_source_columns__"
            || f.source_columns
                .iter()
                .any(|c| c.trim() == "*" || c.trim().eq_ignore_ascii_case("__all__"))
    });
    if !has_star {
        output_fields.insert(0, default_silver_passthrough_field_spec());
    }
}

fn normalize_cleanse_plan_defaults(plan: &mut CleansePlan) {
    // Hard cutover: silver plans should be runnable by default.
    // Insert a conservative default contract: preserve ALL raw columns + add cleaned columns
    // (row-preserving). This avoids repeated plan gate loops.
    for t in plan.tasks.iter_mut() {
        t.implementation_spec.row_preserving = true;
        if t.implementation_spec.spec_version <= 0 {
            t.implementation_spec.spec_version = 1;
        }
        if t.implementation_spec.prohibited_ops.is_empty() {
            t.implementation_spec.prohibited_ops = vec![
                "no filtering".to_string(),
                "no dedup".to_string(),
                "no grain enforcement".to_string(),
            ];
        }
        if t.implementation_spec.output_fields.is_empty() {
            t.implementation_spec.output_fields = vec![default_silver_passthrough_field_spec()];
        } else {
            ensure_silver_passthrough_present(&mut t.implementation_spec.output_fields);
        }

        // Ensure expected_model_path exists when the task is runnable.
        let sql_status = checklist_status(&t.checklist, CHECKLIST_SQL_MODEL);
        if is_runnable_checklist_status(sql_status) {
            let missing_path = t
                .expected_model_path
                .as_deref()
                .map(|s| s.trim().is_empty())
                .unwrap_or(true);
            if missing_path {
                let parts: Vec<&str> = t.dataset_id.split('.').collect();
                if parts.len() == 3 {
                    t.expected_model_path = Some(crate::data_engineer::naming::canonical_staging_rel_path(
                        parts[1],
                        parts[2],
                    ));
                } else {
                    let safe = t
                        .dataset_id
                        .replace('.', "_")
                        .replace('/', "_")
                        .replace('\\', "_");
                    t.expected_model_path = Some(format!("models/staging/stg_{}.sql", safe));
                }
            }
        }
    }
}

pub fn validate_model_plan_semantics(
    plan: &ModelPlan,
    allowed_staging_models: Option<&std::collections::BTreeSet<String>>,
) -> PlanSemanticValidation {
    let mut errors: Vec<String> = Vec::new();
    if plan.plan_key.trim().is_empty() {
        errors.push("plan_key is missing".to_string());
    }
    if plan.tasks.is_empty() {
        errors.push("tasks is empty".to_string());
    }
    for (bi, b) in plan.batches.iter().enumerate() {
        if b.len() > 5 {
            errors.push(format!("batches[{bi}] has >5 items (len={})", b.len()));
        }
    }
    for t in plan.tasks.iter() {
        if t.name.trim().is_empty() {
            errors.push("task.name is empty".to_string());
            continue;
        }
        // Design-first requirement: runnable tasks must have explicit implementation spec.
        if t.implementation_spec.spec_version <= 0 {
            errors.push(format!(
                "{}: implementation_spec.spec_version must be >0",
                t.name
            ));
        }
        if t.implementation_spec.grain.trim().is_empty() {
            errors.push(format!(
                "{}: implementation_spec.grain is empty (design detail required)",
                t.name
            ));
        }
        if t.implementation_spec.output_fields.is_empty() && t.implementation_spec.metrics.is_empty() {
            errors.push(format!(
                "{}: implementation_spec must include output_fields and/or metrics (design detail required)",
                t.name
            ));
        }
        let sql_status = checklist_status(&t.checklist, CHECKLIST_SQL_MODEL);
        if is_runnable_checklist_status(sql_status) {
            if t.goal.trim().is_empty() {
                errors.push(format!("{}: goal is empty for runnable task", t.name));
            }
            // Prevent the "missing inputs" executor failure.
            let nonempty_inputs: Vec<String> = t
                .inputs
                .iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if nonempty_inputs.is_empty() {
                errors.push(format!("{}: inputs is empty for runnable task", t.name));
            }
            if let Some(allowed) = allowed_staging_models {
                for inp in nonempty_inputs.iter() {
                    if !allowed.contains(inp) {
                        // Soft validation: executor will gate this; but we surface early so repair can fix.
                        errors.push(format!(
                            "{}: input '{}' not grounded in models/staging/",
                            t.name, inp
                        ));
                    }
                }
            }
        }
    }
    // Batch references should exist in tasks.
    for (bi, b) in plan.batches.iter().enumerate() {
        for name in b.iter() {
            if plan.tasks.iter().find(|t| t.name == *name).is_none() {
                errors.push(format!(
                    "batches[{bi}] references model name not present in tasks: {name}"
                ));
            }
        }
    }
    // Work-group refs should exist in tasks.
    for g in plan.work_groups.iter() {
        if g.items.len() > 5 {
            errors.push(format!(
                "work_group {} has >5 items (len={})",
                g.group_id,
                g.items.len()
            ));
        }
        for it in g.items.iter() {
            if plan.tasks.iter().find(|t| t.name == it.task_id).is_none() {
                errors.push(format!(
                    "work_group {} references unknown model task_id={}",
                    g.group_id, it.task_id
                ));
            }
            let cid = it.checklist_item_id.trim();
            if cid.is_empty() {
                errors.push(format!(
                    "work_group {} item for task_id={} is missing checklist_item_id",
                    g.group_id, it.task_id
                ));
                continue;
            }
            if let Some(t) = plan.tasks.iter().find(|t| t.name == it.task_id) {
                let exists = t.checklist.iter().any(|x| x.checklist_item_id == cid);
                if !exists {
                    errors.push(format!(
                        "work_group {} references checklist_item_id '{}' not present in task checklist: {}",
                        g.group_id, cid, it.task_id
                    ));
                }
            }
        }
    }
    PlanSemanticValidation {
        ok: errors.is_empty(),
        errors,
    }
}

fn plan_repair_system_prompt(kind: &str) -> String {
    // Keep this short and hard-constraint focused (JSON-only, no tools).
    format!(
        "You are a plan repair agent.\n\
Your job is to repair a {kind} plan JSON so it satisfies server validation.\n\
\n\
Hard constraints:\n\
- Your entire response MUST be a JSON object only (no markdown, no commentary).\n\
- Output ONE JSON object that matches the plan schema.\n\
- Preserve plan_key and status; preserve tasks/batches/work_groups unless required to fix validation.\n\
- DO NOT add tool calls, do not ask questions.\n\
- If a task cannot be repaired without guessing, mark it blocked by setting its sql_model checklist item to status=\"blocked\" and add a short details message.\n\
\n\
Goal: fix only what's needed so the plan can execute deterministically."
    )
}

pub async fn repair_cleanse_plan_semantics_via_llm(
    ctx: &AgentCtx,
    plan: &CleansePlan,
    validation_errors: &[String],
) -> Result<CleansePlan, String> {
    use react_core::llm::ChatMessage;
    use react_core::llm::LlmCallOptions;
    let sys = plan_repair_system_prompt("cleanse");
    let plan_json = serde_json::to_string_pretty(plan).map_err(|e| e.to_string())?;
    let errs = validation_errors.join("\n");
    let user = format!(
        "Validation errors:\n{errs}\n\nCurrent plan JSON (FULL):\n{}\n\nRe-emit the corrected plan JSON only.",
        excerpt_for_prompt(&plan_json, 200_000)
    );
    let messages = vec![
        ChatMessage {
            role: "system".to_string(),
            content: sys,
        },
        ChatMessage {
            role: "user".to_string(),
            content: user,
        },
    ];
    let call_opts = LlmCallOptions {
        prompt_id: "data_engineer.cleanse_plan_semantic_repair",
        thread_id: ctx.thread_id.clone(),
        expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
        temperature: Some(0.0),
        top_p: Some(1.0),
        max_output_tokens: Some(3600),
        reasoning_effort: None,
    };
    let raw = ctx
        .llm
        .chat(&messages, &call_opts)
        .map_err(|e| e.to_string())?;
    let v = parse_json_object_lenient(&raw)?;
    serde_json::from_value::<CleansePlan>(v).map_err(|e| e.to_string())
}

pub async fn repair_model_plan_semantics_via_llm(
    ctx: &AgentCtx,
    plan: &ModelPlan,
    validation_errors: &[String],
    allowed_staging_models: &[String],
) -> Result<ModelPlan, String> {
    use react_core::llm::ChatMessage;
    use react_core::llm::LlmCallOptions;
    let sys = plan_repair_system_prompt("model");
    let plan_json = serde_json::to_string_pretty(plan).map_err(|e| e.to_string())?;
    let errs = validation_errors.join("\n");
    let mut allowed = allowed_staging_models.to_vec();
    allowed.sort();
    allowed.dedup();
    let user = format!(
        "Validation errors:\n{errs}\n\nAllowed staging model inputs (values for task.inputs):\n{}\n\nCurrent plan JSON (FULL):\n{}\n\nRe-emit the corrected plan JSON only.",
        serde_json::to_string_pretty(&allowed).unwrap_or_else(|_| "[]".to_string()),
        excerpt_for_prompt(&plan_json, 200_000)
    );
    let messages = vec![
        ChatMessage {
            role: "system".to_string(),
            content: sys,
        },
        ChatMessage {
            role: "user".to_string(),
            content: user,
        },
    ];
    let call_opts = LlmCallOptions {
        prompt_id: "data_engineer.model_plan_semantic_repair",
        thread_id: ctx.thread_id.clone(),
        expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
        temperature: Some(0.0),
        top_p: Some(1.0),
        max_output_tokens: Some(3600),
        reasoning_effort: None,
    };
    let raw = ctx
        .llm
        .chat(&messages, &call_opts)
        .map_err(|e| e.to_string())?;
    let v = parse_json_object_lenient(&raw)?;
    serde_json::from_value::<ModelPlan>(v).map_err(|e| e.to_string())
}

pub async fn ensure_cleanse_plan_semantically_valid_or_repaired(
    ctx: &AgentCtx,
    plan: &mut CleansePlan,
) -> Result<PlanSemanticValidation, String> {
    normalize_cleanse_plan_defaults(plan);
    let v0 = validate_cleanse_plan_semantics(plan);
    if v0.ok {
        return Ok(v0);
    }
    // Hard cutover: no LLM semantic repair in the hot path.
    let _ = ctx;
    Ok(v0)
}

pub async fn ensure_model_plan_semantically_valid_or_repaired(
    ctx: &AgentCtx,
    plan: &mut ModelPlan,
    allowed_staging_models: &std::collections::BTreeSet<String>,
) -> Result<PlanSemanticValidation, String> {
    let v0 = validate_model_plan_semantics(plan, Some(allowed_staging_models));
    if v0.ok {
        return Ok(v0);
    }
    // Hard cutover: no LLM semantic repair in the hot path.
    let _ = ctx;
    let _ = allowed_staging_models;
    Ok(v0)
}

fn checklist_status(items: &[PlanChecklistItem], id: &str) -> ChecklistItemStatus {
    items
        .iter()
        .find(|it| it.checklist_item_id == id)
        .map(|it| it.status)
        .unwrap_or(ChecklistItemStatus::Pending)
}

pub fn cleanse_next_batch(plan: &CleansePlan) -> Vec<String> {
    for batch in plan.batches.iter() {
        let mut out: Vec<String> = Vec::new();
        for ds in batch.iter() {
            if let Some(t) = plan.tasks.iter().find(|t| t.dataset_id == *ds) {
                // "Next batch" is defined as "next SQL authoring work", not "overall task not done".
                // This avoids repeatedly scheduling a dataset when only schema/validate checklist
                // items remain.
                let st = checklist_status(&t.checklist, CHECKLIST_SQL_MODEL);
                if st != ChecklistItemStatus::Done && is_runnable_checklist_status(st) {
                    out.push(ds.clone());
                }
            } else {
                // If the plan batches reference a dataset not present in tasks, still allow it.
                out.push(ds.clone());
            }
            if out.len() >= 5 {
                break;
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    vec![]
}

pub fn model_next_batch(plan: &ModelPlan) -> Vec<String> {
    for batch in plan.batches.iter() {
        let mut out: Vec<String> = Vec::new();
        for name in batch.iter() {
            if let Some(t) = plan.tasks.iter().find(|t| t.name == *name) {
                let st = checklist_status(&t.checklist, CHECKLIST_SQL_MODEL);
                if st != ChecklistItemStatus::Done && is_runnable_checklist_status(st) {
                    out.push(name.clone());
                }
            } else {
                out.push(name.clone());
            }
            if out.len() >= 5 {
                break;
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    vec![]
}

pub const CHECKLIST_SQL_MODEL: &str = "sql_model";
pub const CHECKLIST_SCHEMA_CONTRACT: &str = "schema_contract";
pub const CHECKLIST_VALIDATE: &str = "validate";

fn group_is_complete_cleanse(plan: &CleansePlan, g: &PlanWorkGroup) -> bool {
    if g.items.is_empty() {
        return true;
    }
    for it in g.items.iter() {
        let Some(t) = plan.tasks.iter().find(|t| t.dataset_id == it.task_id) else {
            return false;
        };
        if checklist_status(&t.checklist, it.checklist_item_id.as_str()) != ChecklistItemStatus::Done
        {
            return false;
        }
    }
    true
}

fn group_is_complete_model(plan: &ModelPlan, g: &PlanWorkGroup) -> bool {
    if g.items.is_empty() {
        return true;
    }
    for it in g.items.iter() {
        let Some(t) = plan.tasks.iter().find(|t| t.name == it.task_id) else {
            return false;
        };
        if checklist_status(&t.checklist, it.checklist_item_id.as_str()) != ChecklistItemStatus::Done
        {
            return false;
        }
    }
    true
}

fn group_deps_satisfied(
    completed: &std::collections::HashSet<String>,
    depends_on: Option<&Vec<String>>,
) -> bool {
    let Some(deps) = depends_on else {
        return true;
    };
    deps.iter().all(|d| completed.contains(d))
}

/// Returns the next work-group driven action for the cleanse plan.
/// - If `work_groups` is empty, returns None (caller should fall back to `batches` heuristics).
/// - For `AuthorSql` / `AuthorSchema`, returns up to 5 task_ids that still need that checklist item.
/// - For `Validate`, returns the kind and an empty vec (caller should transition phases).
pub fn cleanse_next_action(plan: &CleansePlan) -> Option<(WorkGroupKind, Vec<String>)> {
    if plan.work_groups.is_empty() {
        return None;
    }

    let mut completed: std::collections::HashSet<String> = std::collections::HashSet::new();
    for g in plan.work_groups.iter() {
        if group_is_complete_cleanse(plan, g) {
            completed.insert(g.group_id.clone());
        }
    }

    for g in plan.work_groups.iter() {
        if completed.contains(&g.group_id) {
            continue;
        }
        if !group_deps_satisfied(&completed, g.depends_on_group_ids.as_ref()) {
            continue;
        }

        if g.kind == WorkGroupKind::Validate {
            return Some((WorkGroupKind::Validate, vec![]));
        }

        let mut out: Vec<String> = Vec::new();
        for it in g.items.iter() {
            let need = match plan.tasks.iter().find(|t| t.dataset_id == it.task_id) {
                Some(t) => {
                    is_runnable_checklist_status(checklist_status(
                        &t.checklist,
                        it.checklist_item_id.as_str(),
                    ))
                }
                None => true,
            };
            if need {
                if !out.contains(&it.task_id) {
                    out.push(it.task_id.clone());
                }
                if out.len() >= 5 {
                    break;
                }
            }
        }
        return Some((g.kind, out));
    }

    Some((WorkGroupKind::Validate, vec![]))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NextWorkItemCtx {
    pub workgroup_id: String,
    #[serde(default)]
    pub workgroup_label: Option<String>,
    pub task_id: String,
    pub checklist_item_id: String,
}

/// Returns the next concrete work item (group + single checklist ref) for the cleanse plan.
///
/// This is used to populate explicit execution context for tool/LLM spans so UIs can render a
/// stable hierarchy without heuristics.
pub fn cleanse_next_work_item_ctx(plan: &CleansePlan) -> Option<NextWorkItemCtx> {
    if plan.work_groups.is_empty() {
        return None;
    }

    let mut completed: std::collections::HashSet<String> = std::collections::HashSet::new();
    for g in plan.work_groups.iter() {
        if group_is_complete_cleanse(plan, g) {
            completed.insert(g.group_id.clone());
        }
    }

    for g in plan.work_groups.iter() {
        if completed.contains(&g.group_id) {
            continue;
        }
        if !group_deps_satisfied(&completed, g.depends_on_group_ids.as_ref()) {
            continue;
        }
        if g.kind == WorkGroupKind::Validate {
            return None;
        }
        for it in g.items.iter() {
            let need = match plan.tasks.iter().find(|t| t.dataset_id == it.task_id) {
                Some(t) => {
                    is_runnable_checklist_status(checklist_status(
                        &t.checklist,
                        it.checklist_item_id.as_str(),
                    ))
                }
                None => true,
            };
            if need {
                return Some(NextWorkItemCtx {
                    workgroup_id: g.group_id.clone(),
                    workgroup_label: Some(g.label.clone()).filter(|s| !s.trim().is_empty()),
                    task_id: it.task_id.clone(),
                    checklist_item_id: it.checklist_item_id.clone(),
                });
            }
        }
    }
    None
}

/// Returns the next work-group driven action for the model plan.
/// - If `work_groups` is empty, returns None (caller should fall back to `batches` heuristics).
/// - For `AuthorSql` / `AuthorSchema`, returns up to 5 task_ids that still need that checklist item.
/// - For `Validate`, returns the kind and an empty vec (caller should transition phases).
pub fn model_next_action(plan: &ModelPlan) -> Option<(WorkGroupKind, Vec<String>)> {
    if plan.work_groups.is_empty() {
        return None;
    }

    let mut completed: std::collections::HashSet<String> = std::collections::HashSet::new();
    for g in plan.work_groups.iter() {
        if group_is_complete_model(plan, g) {
            completed.insert(g.group_id.clone());
        }
    }

    for g in plan.work_groups.iter() {
        if completed.contains(&g.group_id) {
            continue;
        }
        if !group_deps_satisfied(&completed, g.depends_on_group_ids.as_ref()) {
            continue;
        }

        if g.kind == WorkGroupKind::Validate {
            return Some((WorkGroupKind::Validate, vec![]));
        }

        let mut out: Vec<String> = Vec::new();
        for it in g.items.iter() {
            let need = match plan.tasks.iter().find(|t| t.name == it.task_id) {
                Some(t) => {
                    is_runnable_checklist_status(checklist_status(
                        &t.checklist,
                        it.checklist_item_id.as_str(),
                    ))
                }
                None => true,
            };
            if need {
                if !out.contains(&it.task_id) {
                    out.push(it.task_id.clone());
                }
                if out.len() >= 5 {
                    break;
                }
            }
        }
        return Some((g.kind, out));
    }

    Some((WorkGroupKind::Validate, vec![]))
}

/// Returns the next concrete work item (group + single checklist ref) for the model plan.
pub fn model_next_work_item_ctx(plan: &ModelPlan) -> Option<NextWorkItemCtx> {
    if plan.work_groups.is_empty() {
        return None;
    }

    let mut completed: std::collections::HashSet<String> = std::collections::HashSet::new();
    for g in plan.work_groups.iter() {
        if group_is_complete_model(plan, g) {
            completed.insert(g.group_id.clone());
        }
    }

    for g in plan.work_groups.iter() {
        if completed.contains(&g.group_id) {
            continue;
        }
        if !group_deps_satisfied(&completed, g.depends_on_group_ids.as_ref()) {
            continue;
        }
        if g.kind == WorkGroupKind::Validate {
            return None;
        }
        for it in g.items.iter() {
            let need = match plan.tasks.iter().find(|t| t.name == it.task_id) {
                Some(t) => {
                    is_runnable_checklist_status(checklist_status(
                        &t.checklist,
                        it.checklist_item_id.as_str(),
                    ))
                }
                None => true,
            };
            if need {
                return Some(NextWorkItemCtx {
                    workgroup_id: g.group_id.clone(),
                    workgroup_label: Some(g.label.clone()).filter(|s| !s.trim().is_empty()),
                    task_id: it.task_id.clone(),
                    checklist_item_id: it.checklist_item_id.clone(),
                });
            }
        }
    }
    None
}

pub fn cleanse_pending_schema_contracts(plan: &CleansePlan) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for batch in plan.batches.iter() {
        for ds in batch.iter() {
            let Some(t) = plan.tasks.iter().find(|t| t.dataset_id == *ds) else {
                continue;
            };
            if checklist_status(&t.checklist, CHECKLIST_SQL_MODEL) == ChecklistItemStatus::Done
                && checklist_status(&t.checklist, CHECKLIST_SCHEMA_CONTRACT)
                    != ChecklistItemStatus::Done
            {
                out.push(ds.clone());
                if out.len() >= 5 {
                    return out;
                }
            }
        }
    }
    out
}

/// Pending work for an arbitrary checklist item (cleanse plan).
///
/// This is used by checklist-driven execution where `ExecutionContext.checklist_item_id`
/// can be something other than the stable defaults.
pub fn cleanse_pending_for_checklist(
    plan: &CleansePlan,
    prereq_checklist_item_id: &str,
    target_checklist_item_id: &str,
) -> Vec<String> {
    let prereq = prereq_checklist_item_id.trim();
    let target = target_checklist_item_id.trim();
    if prereq.is_empty() || target.is_empty() {
        return vec![];
    }
    let mut out: Vec<String> = Vec::new();
    for batch in plan.batches.iter() {
        for ds in batch.iter() {
            let Some(t) = plan.tasks.iter().find(|t| t.dataset_id == *ds) else {
                continue;
            };
            if checklist_status(&t.checklist, prereq) != ChecklistItemStatus::Done {
                continue;
            }
            let st = checklist_status(&t.checklist, target);
            if st != ChecklistItemStatus::Done && is_runnable_checklist_status(st) {
                out.push(ds.clone());
                if out.len() >= 5 {
                    return out;
                }
            }
        }
    }
    out
}

pub fn model_pending_schema_contracts(plan: &ModelPlan) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for batch in plan.batches.iter() {
        for name in batch.iter() {
            let Some(t) = plan.tasks.iter().find(|t| t.name == *name) else {
                continue;
            };
            if checklist_status(&t.checklist, CHECKLIST_SQL_MODEL) == ChecklistItemStatus::Done
                && checklist_status(&t.checklist, CHECKLIST_SCHEMA_CONTRACT)
                    != ChecklistItemStatus::Done
            {
                out.push(name.clone());
                if out.len() >= 5 {
                    return out;
                }
            }
        }
    }
    out
}

/// Pending work for an arbitrary checklist item (model plan).
pub fn model_pending_for_checklist(
    plan: &ModelPlan,
    prereq_checklist_item_id: &str,
    target_checklist_item_id: &str,
) -> Vec<String> {
    let prereq = prereq_checklist_item_id.trim();
    let target = target_checklist_item_id.trim();
    if prereq.is_empty() || target.is_empty() {
        return vec![];
    }
    let mut out: Vec<String> = Vec::new();
    for batch in plan.batches.iter() {
        for name in batch.iter() {
            let Some(t) = plan.tasks.iter().find(|t| t.name == *name) else {
                continue;
            };
            if checklist_status(&t.checklist, prereq) != ChecklistItemStatus::Done {
                continue;
            }
            let st = checklist_status(&t.checklist, target);
            if st != ChecklistItemStatus::Done && is_runnable_checklist_status(st) {
                out.push(name.clone());
                if out.len() >= 5 {
                    return out;
                }
            }
        }
    }
    out
}

fn task_status_from_checklist(items: &[PlanChecklistItem]) -> TaskStatus {
    // If checklist is missing (shouldn't happen in normal operation), be conservative.
    if items.is_empty() {
        return TaskStatus::Pending;
    }
    if items
        .iter()
        .any(|it| it.status == ChecklistItemStatus::NeedsUpdate)
    {
        return TaskStatus::NeedsUpdate;
    }
    if items.iter().any(|it| it.status == ChecklistItemStatus::Blocked) {
        return TaskStatus::Blocked;
    }
    if items.iter().all(|it| it.status == ChecklistItemStatus::Done) {
        return TaskStatus::Done;
    }
    if items
        .iter()
        .any(|it| it.status == ChecklistItemStatus::InProgress)
    {
        return TaskStatus::InProgress;
    }
    // If some items are complete but others remain pending, surface as in_progress.
    if items.iter().any(|it| it.status == ChecklistItemStatus::Done) {
        return TaskStatus::InProgress;
    }
    TaskStatus::Pending
}

fn recompute_cleanse_task_status(t: &mut CleanseTask) {
    t.status = task_status_from_checklist(&t.checklist);
}

fn recompute_model_task_status(t: &mut ModelTask) {
    t.status = task_status_from_checklist(&t.checklist);
}

fn evidence_from_tool_end(
    step_idx: usize,
    kind: &str,
    tool_name: &str,
    tool_id: &str,
    ts: &str,
) -> ChecklistEvidence {
    ChecklistEvidence {
        kind: kind.to_string(),
        tool_name: Some(tool_name.to_string()),
        tool_id: Some(tool_id.to_string()),
        step_idx,
        ts: Some(ts.to_string()),
    }
}

fn push_evidence_unique(item: &mut PlanChecklistItem, ev: ChecklistEvidence) {
    if item
        .evidence
        .iter()
        .any(|e| e.kind == ev.kind && e.step_idx == ev.step_idx && e.tool_id == ev.tool_id)
    {
        return;
    }
    item.evidence.push(ev);
}

fn ensure_checklist_item<'a>(
    items: &'a mut Vec<PlanChecklistItem>,
    checklist_item_id: &str,
    label: &str,
) -> &'a mut PlanChecklistItem {
    if let Some(i) = items
        .iter()
        .position(|it| it.checklist_item_id.trim() == checklist_item_id)
    {
        return &mut items[i];
    }
    items.push(PlanChecklistItem {
        checklist_item_id: checklist_item_id.to_string(),
        label: label.to_string(),
        details: None,
        status: ChecklistItemStatus::Pending,
        origin: ChecklistOrigin::Initial,
        origin_step_idx: None,
        evidence: vec![],
    });
    let last = items.len().saturating_sub(1);
    &mut items[last]
}

fn set_checklist_status(
    item: &mut PlanChecklistItem,
    status: ChecklistItemStatus,
    ev: Option<ChecklistEvidence>,
) {
    item.status = status;
    if let Some(e) = ev {
        push_evidence_unique(item, e);
    }
}

pub fn cleanse_mark_done(plan: &mut CleansePlan, dataset_id: &str) {
    if let Some(t) = plan.tasks.iter_mut().find(|t| t.dataset_id == dataset_id) {
        let it = ensure_checklist_item(&mut t.checklist, CHECKLIST_SQL_MODEL, "Author staging SQL");
        set_checklist_status(it, ChecklistItemStatus::Done, None);
        recompute_cleanse_task_status(t);
    }
}

fn ensure_checklist_item_any<'a>(
    items: &'a mut Vec<PlanChecklistItem>,
    checklist_item_id: &str,
) -> &'a mut PlanChecklistItem {
    if let Some(i) = items
        .iter()
        .position(|it| it.checklist_item_id.trim() == checklist_item_id)
    {
        return &mut items[i];
    }
    items.push(PlanChecklistItem {
        checklist_item_id: checklist_item_id.to_string(),
        label: checklist_item_id.to_string(),
        details: None,
        status: ChecklistItemStatus::Pending,
        origin: ChecklistOrigin::Initial,
        origin_step_idx: None,
        evidence: vec![],
    });
    let last = items.len().saturating_sub(1);
    &mut items[last]
}

/// Mark an arbitrary cleanse checklist item status for a dataset.
///
/// This is used by deterministic tools that operate under a work-group/checklist execution
/// context (e.g. `time_derivatives`, `style_and_contract_cleanup`) so those items can actually
/// complete and the plan can advance.
pub fn cleanse_checklist_mark_status(
    plan: &mut CleansePlan,
    dataset_id: &str,
    checklist_item_id: &str,
    status: ChecklistItemStatus,
) {
    if checklist_item_id.trim().is_empty() {
        return;
    }
    if let Some(t) = plan.tasks.iter_mut().find(|t| t.dataset_id == dataset_id) {
        let it = ensure_checklist_item_any(&mut t.checklist, checklist_item_id);
        set_checklist_status(it, status, None);
        recompute_cleanse_task_status(t);
    }
}

pub fn model_mark_done(plan: &mut ModelPlan, name: &str) {
    if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == name) {
        let it = ensure_checklist_item(&mut t.checklist, CHECKLIST_SQL_MODEL, "Author gold SQL");
        set_checklist_status(it, ChecklistItemStatus::Done, None);
        recompute_model_task_status(t);
    }
}

/// Mark an arbitrary model checklist item status for a gold model item.
///
/// This is used by deterministic tools that operate under a work-group/checklist execution
/// context so secondary checklist items can complete and the plan can advance.
pub fn model_checklist_mark_status(
    plan: &mut ModelPlan,
    name: &str,
    checklist_item_id: &str,
    status: ChecklistItemStatus,
) {
    if checklist_item_id.trim().is_empty() {
        return;
    }
    if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == name) {
        let it = ensure_checklist_item_any(&mut t.checklist, checklist_item_id);
        set_checklist_status(it, status, None);
        recompute_model_task_status(t);
    }
}

pub fn model_schema_contract_mark_done(plan: &mut ModelPlan, name: &str) {
    if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == name) {
        let it = ensure_checklist_item(
            &mut t.checklist,
            CHECKLIST_SCHEMA_CONTRACT,
            "Author schema contract",
        );
        set_checklist_status(it, ChecklistItemStatus::Done, None);
        recompute_model_task_status(t);
    }
}

pub fn model_schema_contract_mark_needs_update(plan: &mut ModelPlan, name: &str) {
    if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == name) {
        let it = ensure_checklist_item(
            &mut t.checklist,
            CHECKLIST_SCHEMA_CONTRACT,
            "Author schema contract",
        );
        set_checklist_status(it, ChecklistItemStatus::NeedsUpdate, None);
        recompute_model_task_status(t);
    }
}

pub fn model_schema_contract_mark_in_progress(plan: &mut ModelPlan, name: &str) {
    if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == name) {
        let it = ensure_checklist_item(
            &mut t.checklist,
            CHECKLIST_SCHEMA_CONTRACT,
            "Author schema contract",
        );
        set_checklist_status(it, ChecklistItemStatus::InProgress, None);
        recompute_model_task_status(t);
    }
}

pub fn cleanse_schema_contract_mark_done(plan: &mut CleansePlan, dataset_id: &str) {
    if let Some(t) = plan.tasks.iter_mut().find(|t| t.dataset_id == dataset_id) {
        let it = ensure_checklist_item(
            &mut t.checklist,
            CHECKLIST_SCHEMA_CONTRACT,
            "Author schema contract",
        );
        set_checklist_status(it, ChecklistItemStatus::Done, None);
        recompute_cleanse_task_status(t);
    }
}

pub fn cleanse_schema_contract_mark_needs_update(plan: &mut CleansePlan, dataset_id: &str) {
    if let Some(t) = plan.tasks.iter_mut().find(|t| t.dataset_id == dataset_id) {
        let it = ensure_checklist_item(
            &mut t.checklist,
            CHECKLIST_SCHEMA_CONTRACT,
            "Author schema contract",
        );
        set_checklist_status(it, ChecklistItemStatus::NeedsUpdate, None);
        recompute_cleanse_task_status(t);
    }
}

pub fn cleanse_schema_contract_mark_in_progress(plan: &mut CleansePlan, dataset_id: &str) {
    if let Some(t) = plan.tasks.iter_mut().find(|t| t.dataset_id == dataset_id) {
        let it = ensure_checklist_item(
            &mut t.checklist,
            CHECKLIST_SCHEMA_CONTRACT,
            "Author schema contract",
        );
        set_checklist_status(it, ChecklistItemStatus::InProgress, None);
        recompute_cleanse_task_status(t);
    }
}

pub fn cleanse_mark_needs_update(plan: &mut CleansePlan, dataset_id: &str) {
    if let Some(t) = plan.tasks.iter_mut().find(|t| t.dataset_id == dataset_id) {
        let it =
            ensure_checklist_item(&mut t.checklist, CHECKLIST_SQL_MODEL, "Author staging SQL");
        set_checklist_status(it, ChecklistItemStatus::NeedsUpdate, None);
        recompute_cleanse_task_status(t);
    }
}

pub fn model_mark_needs_update(plan: &mut ModelPlan, name: &str) {
    if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == name) {
        let it = ensure_checklist_item(&mut t.checklist, CHECKLIST_SQL_MODEL, "Author gold SQL");
        set_checklist_status(it, ChecklistItemStatus::NeedsUpdate, None);
        recompute_model_task_status(t);
    }
}

pub fn cleanse_mark_in_progress(plan: &mut CleansePlan, dataset_id: &str) {
    if let Some(t) = plan.tasks.iter_mut().find(|t| t.dataset_id == dataset_id) {
        let it =
            ensure_checklist_item(&mut t.checklist, CHECKLIST_SQL_MODEL, "Author staging SQL");
        set_checklist_status(it, ChecklistItemStatus::InProgress, None);
        recompute_cleanse_task_status(t);
    }
}

pub fn model_mark_in_progress(plan: &mut ModelPlan, name: &str) {
    if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == name) {
        let it = ensure_checklist_item(&mut t.checklist, CHECKLIST_SQL_MODEL, "Author gold SQL");
        set_checklist_status(it, ChecklistItemStatus::InProgress, None);
        recompute_model_task_status(t);
    }
}

pub fn cleanse_all_done(plan: &CleansePlan) -> bool {
    plan.tasks.iter().all(|t| t.status == TaskStatus::Done)
}

pub fn model_all_done(plan: &ModelPlan) -> bool {
    plan.tasks.iter().all(|t| t.status == TaskStatus::Done)
}

pub fn update_cleanse_progress_from_log(plan: &mut CleansePlan, log: &ThreadLog) {
    let start = plan.progress.last_applied_step_idx.min(log.steps.len());
    for (idx, step) in log.steps.iter().enumerate().skip(start) {
        let ThreadStep::ToolEnd {
            tool_id,
            name,
            args,
            observation,
            ts,
            ctx: step_ctx,
            ..
        } = step
        else {
            continue;
        };

        // Preferred: derive from structured batch tool output.
        if name == "apply_next_cleanse_batch" {
            let checklist_item_id = step_ctx
                .as_ref()
                .and_then(|c| c.checklist_item_id.as_deref())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| CHECKLIST_SQL_MODEL.to_string());
            let ok = observation.ok;
            let attempted: Vec<String> = observation
                .extra
                .get("attempted_dataset_ids")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let succeeded: Vec<String> = observation
                .extra
                .get("succeeded_dataset_ids")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let succ_set: std::collections::HashSet<String> = succeeded.iter().cloned().collect();
            let failed: Vec<String> = attempted
                .iter()
                .filter(|ds| !succ_set.contains(*ds))
                .cloned()
                .collect();

            for ds in attempted.iter() {
                if let Some(t) = plan.tasks.iter_mut().find(|t| t.dataset_id == *ds) {
                    let it = if checklist_item_id == CHECKLIST_SQL_MODEL {
                        ensure_checklist_item(
                            &mut t.checklist,
                            CHECKLIST_SQL_MODEL,
                            "Author staging SQL",
                        )
                    } else {
                        ensure_checklist_item_any(&mut t.checklist, &checklist_item_id)
                    };
                    set_checklist_status(
                        it,
                        ChecklistItemStatus::InProgress,
                        Some(evidence_from_tool_end(idx, "tool_end", name, tool_id, ts)),
                    );
                    recompute_cleanse_task_status(t);
                }
            }
            for ds in succeeded.iter() {
                if let Some(t) = plan.tasks.iter_mut().find(|t| t.dataset_id == *ds) {
                    let it = if checklist_item_id == CHECKLIST_SQL_MODEL {
                        ensure_checklist_item(
                            &mut t.checklist,
                            CHECKLIST_SQL_MODEL,
                            "Author staging SQL",
                        )
                    } else {
                        ensure_checklist_item_any(&mut t.checklist, &checklist_item_id)
                    };
                    set_checklist_status(
                        it,
                        ChecklistItemStatus::Done,
                        Some(evidence_from_tool_end(idx, "tool_end_ok", name, tool_id, ts)),
                    );
                    recompute_cleanse_task_status(t);
                }
            }
            if !ok || !failed.is_empty() {
                for ds in failed.iter() {
                    if let Some(t) = plan.tasks.iter_mut().find(|t| t.dataset_id == *ds) {
                        let it = if checklist_item_id == CHECKLIST_SQL_MODEL {
                            ensure_checklist_item(
                                &mut t.checklist,
                                CHECKLIST_SQL_MODEL,
                                "Author staging SQL",
                            )
                        } else {
                            ensure_checklist_item_any(&mut t.checklist, &checklist_item_id)
                        };
                        set_checklist_status(
                            it,
                            ChecklistItemStatus::NeedsUpdate,
                            Some(evidence_from_tool_end(idx, "tool_end_failed", name, tool_id, ts)),
                        );
                        recompute_cleanse_task_status(t);
                    }
                }
            }
            plan.progress.last_applied_step_idx = idx + 1;
            continue;
        }

        if name == "apply_next_cleanse_schema_batch" {
            let checklist_item_id = step_ctx
                .as_ref()
                .and_then(|c| c.checklist_item_id.as_deref())
                .or_else(|| observation.extra.get("checklist_item_id").and_then(|v| v.as_str()))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| CHECKLIST_SCHEMA_CONTRACT.to_string());
            let ok = observation.ok;
            let attempted: Vec<String> = observation
                .extra
                .get("attempted_dataset_ids")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let succeeded: Vec<String> = observation
                .extra
                .get("succeeded_dataset_ids")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let succ_set: std::collections::HashSet<String> = succeeded.iter().cloned().collect();
            let failed: Vec<String> = attempted
                .iter()
                .filter(|ds| !succ_set.contains(*ds))
                .cloned()
                .collect();

            for ds in attempted.iter() {
                if let Some(t) = plan.tasks.iter_mut().find(|t| t.dataset_id == *ds) {
                    let it = if checklist_item_id == CHECKLIST_SCHEMA_CONTRACT {
                        ensure_checklist_item(
                            &mut t.checklist,
                            CHECKLIST_SCHEMA_CONTRACT,
                            "Author schema contract",
                        )
                    } else {
                        ensure_checklist_item_any(&mut t.checklist, &checklist_item_id)
                    };
                    set_checklist_status(
                        it,
                        ChecklistItemStatus::InProgress,
                        Some(evidence_from_tool_end(idx, "tool_end", name, tool_id, ts)),
                    );
                    recompute_cleanse_task_status(t);
                }
            }
            for ds in succeeded.iter() {
                if let Some(t) = plan.tasks.iter_mut().find(|t| t.dataset_id == *ds) {
                    let it = if checklist_item_id == CHECKLIST_SCHEMA_CONTRACT {
                        ensure_checklist_item(
                            &mut t.checklist,
                            CHECKLIST_SCHEMA_CONTRACT,
                            "Author schema contract",
                        )
                    } else {
                        ensure_checklist_item_any(&mut t.checklist, &checklist_item_id)
                    };
                    set_checklist_status(
                        it,
                        ChecklistItemStatus::Done,
                        Some(evidence_from_tool_end(idx, "tool_end_ok", name, tool_id, ts)),
                    );
                    recompute_cleanse_task_status(t);
                }
            }
            if !ok || !failed.is_empty() {
                for ds in failed.iter() {
                    if let Some(t) = plan.tasks.iter_mut().find(|t| t.dataset_id == *ds) {
                        let it = if checklist_item_id == CHECKLIST_SCHEMA_CONTRACT {
                            ensure_checklist_item(
                                &mut t.checklist,
                                CHECKLIST_SCHEMA_CONTRACT,
                                "Author schema contract",
                            )
                        } else {
                            ensure_checklist_item_any(&mut t.checklist, &checklist_item_id)
                        };
                        set_checklist_status(
                            it,
                            ChecklistItemStatus::NeedsUpdate,
                            Some(evidence_from_tool_end(
                                idx,
                                "tool_end_failed",
                                name,
                                tool_id,
                                ts,
                            )),
                        );
                        recompute_cleanse_task_status(t);
                    }
                }
            }
            plan.progress.last_applied_step_idx = idx + 1;
            continue;
        }

        // Direct staging tool output (non-batched).
        if name == "staging_model" {
            let ok = observation.ok;
            let args_dataset_ids: Vec<String> = args
                .get("dataset_ids")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let succeeded: Vec<String> = observation
                .extra
                .get("succeeded_dataset_ids")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            for ds in args_dataset_ids.iter() {
                if let Some(t) = plan.tasks.iter_mut().find(|t| t.dataset_id == *ds) {
                    let it = ensure_checklist_item(
                        &mut t.checklist,
                        CHECKLIST_SQL_MODEL,
                        "Author staging SQL",
                    );
                    set_checklist_status(
                        it,
                        ChecklistItemStatus::InProgress,
                        Some(evidence_from_tool_end(idx, "tool_end", name, tool_id, ts)),
                    );
                    recompute_cleanse_task_status(t);
                }
            }
            for ds in succeeded.iter() {
                cleanse_mark_done(plan, ds);
            }
            if !ok {
                for ds in args_dataset_ids.iter() {
                    if succeeded.contains(ds) {
                        continue;
                    }
                    if let Some(t) = plan.tasks.iter_mut().find(|t| t.dataset_id == *ds) {
                        let it = ensure_checklist_item(
                            &mut t.checklist,
                            CHECKLIST_SQL_MODEL,
                            "Author staging SQL",
                        );
                        set_checklist_status(
                            it,
                            ChecklistItemStatus::NeedsUpdate,
                            Some(evidence_from_tool_end(idx, "tool_end_failed", name, tool_id, ts)),
                        );
                        recompute_cleanse_task_status(t);
                    }
                }
            }
            plan.progress.last_applied_step_idx = idx + 1;
            continue;
        }

        // Direct authoring tool (legacy / non-batched): track staging_model success/failure as sql_model progress.
        if name == "staging_model" {
            let ok = observation.ok;
            let args_dataset_ids: Vec<String> = args
                .get("dataset_ids")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let succeeded: Vec<String> = observation
                .extra
                .get("succeeded_dataset_ids")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();

            for ds in args_dataset_ids.iter() {
                if let Some(t) = plan.tasks.iter_mut().find(|t| t.dataset_id == *ds) {
                    let it =
                        ensure_checklist_item(&mut t.checklist, CHECKLIST_SQL_MODEL, "Author staging SQL");
                    set_checklist_status(
                        it,
                        ChecklistItemStatus::InProgress,
                        Some(evidence_from_tool_end(idx, "tool_end", name, tool_id, ts)),
                    );
                    recompute_cleanse_task_status(t);
                }
            }
            for ds in succeeded.iter() {
                if let Some(t) = plan.tasks.iter_mut().find(|t| t.dataset_id == *ds) {
                    let it =
                        ensure_checklist_item(&mut t.checklist, CHECKLIST_SQL_MODEL, "Author staging SQL");
                    set_checklist_status(
                        it,
                        ChecklistItemStatus::Done,
                        Some(evidence_from_tool_end(idx, "tool_end_ok", name, tool_id, ts)),
                    );
                    recompute_cleanse_task_status(t);
                }
            }
            if !ok {
                for ds in args_dataset_ids.iter() {
                    if succeeded.contains(ds) {
                        continue;
                    }
                    if let Some(t) = plan.tasks.iter_mut().find(|t| t.dataset_id == *ds) {
                        let it =
                            ensure_checklist_item(&mut t.checklist, CHECKLIST_SQL_MODEL, "Author staging SQL");
                        set_checklist_status(
                            it,
                            ChecklistItemStatus::NeedsUpdate,
                            Some(evidence_from_tool_end(
                                idx,
                                "tool_end_failed",
                                name,
                                tool_id,
                                ts,
                            )),
                        );
                        recompute_cleanse_task_status(t);
                    }
                }
            }
            plan.progress.last_applied_step_idx = idx + 1;
            continue;
        }

        if name == "dbt_files" {
            let op = args.get("op").and_then(|v| v.as_str()).unwrap_or("");
            if op == "patch" || op == "mv" {
                let ok = observation.ok;

                let paths = extract_dbt_files_paths(op, args);
                // SQL patching can be used for targeted remediation; treat successful patches as progress.
                let mut stems: Vec<String> = paths.iter().filter_map(|p| file_stem(p)).collect();
                stems.sort();
                stems.dedup();
                let sql_checklist_item_id = step_ctx
                    .as_ref()
                    .and_then(|c| c.checklist_item_id.as_deref())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| CHECKLIST_SQL_MODEL.to_string());
                if !stems.is_empty() {
                    for t in plan.tasks.iter_mut() {
                        let Some(p) = t.expected_model_path.as_deref() else {
                            continue;
                        };
                        let Some(st) = file_stem(p) else {
                            continue;
                        };
                        if stems.iter().any(|s| s == &st) {
                            let it = if sql_checklist_item_id == CHECKLIST_SQL_MODEL {
                                ensure_checklist_item(
                                    &mut t.checklist,
                                    CHECKLIST_SQL_MODEL,
                                    "Author staging SQL",
                                )
                            } else {
                                ensure_checklist_item_any(&mut t.checklist, &sql_checklist_item_id)
                            };
                            let new_status = if ok {
                                ChecklistItemStatus::InProgress
                            } else {
                                ChecklistItemStatus::NeedsUpdate
                            };
                            let ev_kind = if ok { "tool_end_ok" } else { "tool_end_failed" };
                            set_checklist_status(
                                it,
                                new_status,
                                Some(evidence_from_tool_end(idx, ev_kind, name, tool_id, ts)),
                            );
                            recompute_cleanse_task_status(t);
                        }
                    }
                    if ok {
                        plan.progress.consecutive_batch_failures = 0;
                    }
                }
                let touched_schema_yml = paths.iter().any(|p| {
                    p.trim() == "models/schema.yml"
                        || (p.trim().starts_with("models/")
                            && p.trim().ends_with(".yml")
                            && !p.trim().contains("/_versions/"))
                });
                if touched_schema_yml {
                    let Some(schema_checklist_item_id) = step_ctx
                        .as_ref()
                        .and_then(|c| c.checklist_item_id.as_deref())
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                    else {
                        plan.progress.last_applied_step_idx = idx + 1;
                        continue;
                    };
                    // Best-effort: match by expected_model_path stem.
                    for t in plan.tasks.iter_mut() {
                        let Some(p) = t.expected_model_path.as_deref() else {
                            continue;
                        };
                        let Some(st) = file_stem(p) else {
                            continue;
                        };
                        if stems.is_empty() || stems.iter().any(|s| s == &st) || paths.iter().any(|pp| pp.trim() == "models/schema.yml") {
                            let it = if schema_checklist_item_id == CHECKLIST_SCHEMA_CONTRACT {
                                ensure_checklist_item(
                                    &mut t.checklist,
                                    CHECKLIST_SCHEMA_CONTRACT,
                                    "Author schema contract",
                                )
                            } else {
                                ensure_checklist_item_any(&mut t.checklist, &schema_checklist_item_id)
                            };
                            let ev_kind = if ok { "tool_end_ok" } else { "tool_end_failed" };
                            let new_status = if ok {
                                ChecklistItemStatus::Done
                            } else {
                                ChecklistItemStatus::NeedsUpdate
                            };
                            set_checklist_status(
                                it,
                                new_status,
                                Some(evidence_from_tool_end(idx, ev_kind, name, tool_id, ts)),
                            );
                            recompute_cleanse_task_status(t);
                        }
                    }
                    if ok {
                        // Treat a successful schema patch as forward progress for batching.
                        plan.progress.consecutive_batch_failures = 0;
                    }
                }
                plan.progress.last_applied_step_idx = idx + 1;
            }
            continue;
        }

        if name == "dbt_validate" {
            let ok = observation.ok;
            if ok {
                for t in plan.tasks.iter_mut() {
                    // Mark validate done for tasks that have authored their SQL.
                    if checklist_status(&t.checklist, CHECKLIST_SQL_MODEL) != ChecklistItemStatus::Done
                    {
                        continue;
                    }
                    let v = ensure_checklist_item(&mut t.checklist, CHECKLIST_VALIDATE, "Validate");
                    if v.status == ChecklistItemStatus::Done {
                        continue;
                    }
                    set_checklist_status(
                        v,
                        ChecklistItemStatus::Done,
                        Some(evidence_from_tool_end(idx, "tool_end_ok", name, tool_id, ts)),
                    );
                    recompute_cleanse_task_status(t);
                }
            } else {
                let logs = observation
                    .extra
                    .get("logs")
                    .cloned()
                    .unwrap_or(Value::Null);
                let failed = crate::data_engineer::dbt_error::extract_failed_models_from_logs(&logs);
                let runtime =
                    crate::data_engineer::dbt_error::extract_runtime_failures_from_logs(&logs);
                let contract_data_type_missing =
                    crate::data_engineer::dbt_error::logs_indicate_contract_data_type_missing(&logs);

                let mut names: Vec<String> = Vec::new();
                for f in failed.iter() {
                    if let Some(file) = f.get("file").and_then(|v| v.as_str()) {
                        if let Some(st) = file_stem(file) {
                            names.push(st);
                        }
                    }
                    if let Some(nm) = f.get("name").and_then(|v| v.as_str()) {
                        if !nm.trim().is_empty() {
                            names.push(nm.trim().to_string());
                        }
                    }
                }
                for r in runtime.iter() {
                    if let Some(mh) = r.get("model_hint").and_then(|v| v.as_str()) {
                        if !mh.trim().is_empty() {
                            names.push(mh.trim().to_string());
                        }
                    }
                }
                names.sort();
                names.dedup();

                for t in plan.tasks.iter_mut() {
                    let implicated = if names.is_empty() {
                        true
                    } else if let Some(p) = t.expected_model_path.as_deref() {
                        if let Some(st) = file_stem(p) {
                            names.iter().any(|n| n == &st)
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    if !implicated {
                        continue;
                    }
                    let v = ensure_checklist_item(&mut t.checklist, CHECKLIST_VALIDATE, "Validate");
                    set_checklist_status(
                        v,
                        ChecklistItemStatus::NeedsUpdate,
                        Some(evidence_from_tool_end(idx, "tool_end_failed", name, tool_id, ts)),
                    );
                    if contract_data_type_missing {
                        let sc = ensure_checklist_item(
                            &mut t.checklist,
                            CHECKLIST_SCHEMA_CONTRACT,
                            "Author schema contract",
                        );
                        set_checklist_status(
                            sc,
                            ChecklistItemStatus::NeedsUpdate,
                            Some(evidence_from_tool_end(
                                idx,
                                "tool_end_failed",
                                name,
                                tool_id,
                                ts,
                            )),
                        );
                    }
                    recompute_cleanse_task_status(t);
                }
            }
            plan.progress.last_applied_step_idx = idx + 1;
            continue;
        }
    }
}

pub fn update_model_progress_from_log(plan: &mut ModelPlan, log: &ThreadLog) {
    let start = plan.progress.last_applied_step_idx.min(log.steps.len());
    for (idx, step) in log.steps.iter().enumerate().skip(start) {
        let ThreadStep::ToolEnd {
            tool_id,
            name,
            args,
            observation,
            ts,
            ctx: step_ctx,
            ..
        } = step
        else {
            continue;
        };

        // Preferred: derive from structured batch tool output.
        if name == "apply_next_model_batch" {
            let checklist_item_id = step_ctx
                .as_ref()
                .and_then(|c| c.checklist_item_id.as_deref())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| CHECKLIST_SQL_MODEL.to_string());
            let ok = observation.ok;
            let attempted: Vec<String> = observation
                .extra
                .get("attempted_item_names")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let succeeded: Vec<String> = observation
                .extra
                .get("succeeded_item_names")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let succ_set: std::collections::HashSet<String> = succeeded.iter().cloned().collect();
            let failed: Vec<String> = attempted
                .iter()
                .filter(|n| !succ_set.contains(*n))
                .cloned()
                .collect();

            for n in attempted.iter() {
                if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == *n) {
                    let it = if checklist_item_id == CHECKLIST_SQL_MODEL {
                        ensure_checklist_item(&mut t.checklist, CHECKLIST_SQL_MODEL, "Author gold SQL")
                    } else {
                        ensure_checklist_item_any(&mut t.checklist, &checklist_item_id)
                    };
                    set_checklist_status(
                        it,
                        ChecklistItemStatus::InProgress,
                        Some(evidence_from_tool_end(idx, "tool_end", name, tool_id, ts)),
                    );
                    recompute_model_task_status(t);
                }
            }
            for n in succeeded.iter() {
                if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == *n) {
                    let it = if checklist_item_id == CHECKLIST_SQL_MODEL {
                        ensure_checklist_item(&mut t.checklist, CHECKLIST_SQL_MODEL, "Author gold SQL")
                    } else {
                        ensure_checklist_item_any(&mut t.checklist, &checklist_item_id)
                    };
                    set_checklist_status(
                        it,
                        ChecklistItemStatus::Done,
                        Some(evidence_from_tool_end(idx, "tool_end_ok", name, tool_id, ts)),
                    );
                    recompute_model_task_status(t);
                }
            }
            if !ok || !failed.is_empty() {
                for n in failed.iter() {
                    if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == *n) {
                        let it = if checklist_item_id == CHECKLIST_SQL_MODEL {
                            ensure_checklist_item(&mut t.checklist, CHECKLIST_SQL_MODEL, "Author gold SQL")
                        } else {
                            ensure_checklist_item_any(&mut t.checklist, &checklist_item_id)
                        };
                        set_checklist_status(
                            it,
                            ChecklistItemStatus::NeedsUpdate,
                            Some(evidence_from_tool_end(
                                idx,
                                "tool_end_failed",
                                name,
                                tool_id,
                                ts,
                            )),
                        );
                        recompute_model_task_status(t);
                    }
                }
            }
            plan.progress.last_applied_step_idx = idx + 1;
            continue;
        }

        if name == "apply_next_model_schema_batch" {
            let checklist_item_id = step_ctx
                .as_ref()
                .and_then(|c| c.checklist_item_id.as_deref())
                .or_else(|| observation.extra.get("checklist_item_id").and_then(|v| v.as_str()))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| CHECKLIST_SCHEMA_CONTRACT.to_string());
            let ok = observation.ok;
            let attempted: Vec<String> = observation
                .extra
                .get("attempted_item_names")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let succeeded: Vec<String> = observation
                .extra
                .get("succeeded_item_names")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let succ_set: std::collections::HashSet<String> = succeeded.iter().cloned().collect();
            let failed: Vec<String> = attempted
                .iter()
                .filter(|n| !succ_set.contains(*n))
                .cloned()
                .collect();

            for n in attempted.iter() {
                if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == *n) {
                    let it = if checklist_item_id == CHECKLIST_SCHEMA_CONTRACT {
                        ensure_checklist_item(
                            &mut t.checklist,
                            CHECKLIST_SCHEMA_CONTRACT,
                            "Author schema contract",
                        )
                    } else {
                        ensure_checklist_item_any(&mut t.checklist, &checklist_item_id)
                    };
                    set_checklist_status(
                        it,
                        ChecklistItemStatus::InProgress,
                        Some(evidence_from_tool_end(idx, "tool_end", name, tool_id, ts)),
                    );
                    recompute_model_task_status(t);
                }
            }
            for n in succeeded.iter() {
                if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == *n) {
                    let it = if checklist_item_id == CHECKLIST_SCHEMA_CONTRACT {
                        ensure_checklist_item(
                            &mut t.checklist,
                            CHECKLIST_SCHEMA_CONTRACT,
                            "Author schema contract",
                        )
                    } else {
                        ensure_checklist_item_any(&mut t.checklist, &checklist_item_id)
                    };
                    set_checklist_status(
                        it,
                        ChecklistItemStatus::Done,
                        Some(evidence_from_tool_end(idx, "tool_end_ok", name, tool_id, ts)),
                    );
                    recompute_model_task_status(t);
                }
            }
            if !ok || !failed.is_empty() {
                for n in failed.iter() {
                    if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == *n) {
                        let it = if checklist_item_id == CHECKLIST_SCHEMA_CONTRACT {
                            ensure_checklist_item(
                                &mut t.checklist,
                                CHECKLIST_SCHEMA_CONTRACT,
                                "Author schema contract",
                            )
                        } else {
                            ensure_checklist_item_any(&mut t.checklist, &checklist_item_id)
                        };
                        set_checklist_status(
                            it,
                            ChecklistItemStatus::NeedsUpdate,
                            Some(evidence_from_tool_end(
                                idx,
                                "tool_end_failed",
                                name,
                                tool_id,
                                ts,
                            )),
                        );
                        recompute_model_task_status(t);
                    }
                }
            }
            plan.progress.last_applied_step_idx = idx + 1;
            continue;
        }

        if name == "gold_model" {
            let ok = observation.ok;
            let succeeded: Vec<String> = observation
                .extra
                .get("succeeded_item_names")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();

            let arg_names: Vec<String> = args
                .get("items")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|it| {
                            it.get("name")
                                .and_then(|n| n.as_str())
                                .map(|s| s.to_string())
                        })
                        .collect()
                })
                .unwrap_or_default();
            for nm in arg_names.iter() {
                if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == *nm) {
                    let it = ensure_checklist_item(
                        &mut t.checklist,
                        CHECKLIST_SQL_MODEL,
                        "Author gold SQL",
                    );
                    set_checklist_status(
                        it,
                        ChecklistItemStatus::InProgress,
                        Some(evidence_from_tool_end(idx, "tool_end", name, tool_id, ts)),
                    );
                    recompute_model_task_status(t);
                }
            }

            for n in succeeded.iter() {
                if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == *n) {
                    let it = ensure_checklist_item(
                        &mut t.checklist,
                        CHECKLIST_SQL_MODEL,
                        "Author gold SQL",
                    );
                    set_checklist_status(
                        it,
                        ChecklistItemStatus::Done,
                        Some(evidence_from_tool_end(idx, "tool_end_ok", name, tool_id, ts)),
                    );
                    recompute_model_task_status(t);
                }
            }

            if !ok {
                for nm in arg_names.iter() {
                    if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == *nm) {
                        let it = ensure_checklist_item(
                            &mut t.checklist,
                            CHECKLIST_SQL_MODEL,
                            "Author gold SQL",
                        );
                        if !succeeded.contains(nm) {
                            set_checklist_status(
                                it,
                                ChecklistItemStatus::NeedsUpdate,
                                Some(evidence_from_tool_end(
                                    idx,
                                    "tool_end_failed",
                                    name,
                                    tool_id,
                                    ts,
                                )),
                            );
                        }
                        recompute_model_task_status(t);
                    }
                }
            }

            plan.progress.last_applied_step_idx = idx + 1;
            continue;
        }

        if name == "dbt_validate" {
            let ok = observation.ok;
            if ok {
                // Treat a successful validate as satisfying `validate` for the whole plan.
                for t in plan.tasks.iter_mut() {
                    let it = ensure_checklist_item(
                        &mut t.checklist,
                        CHECKLIST_VALIDATE,
                        "Validate DBT",
                    );
                    set_checklist_status(
                        it,
                        ChecklistItemStatus::Done,
                        Some(evidence_from_tool_end(idx, "tool_end_ok", name, tool_id, ts)),
                    );
                    recompute_model_task_status(t);
                }
            } else {
                let logs = observation
                    .extra
                    .get("logs")
                    .cloned()
                    .unwrap_or(Value::Null);
                let failed =
                    crate::data_engineer::dbt_error::extract_failed_models_from_logs(&logs);
                let runtime =
                    crate::data_engineer::dbt_error::extract_runtime_failures_from_logs(&logs);

                let mut names: Vec<String> = Vec::new();
                for f in failed.iter() {
                    if let Some(file) = f.get("file").and_then(|v| v.as_str()) {
                        if let Some(st) = file_stem(file) {
                            names.push(st);
                        }
                    }
                    if let Some(nm) = f.get("name").and_then(|v| v.as_str()) {
                        if !nm.trim().is_empty() {
                            names.push(nm.trim().to_string());
                        }
                    }
                }
                for r in runtime.iter() {
                    if let Some(mh) = r.get("model_hint").and_then(|v| v.as_str()) {
                        if !mh.trim().is_empty() {
                            names.push(mh.trim().to_string());
                        }
                    }
                }
                names.sort();
                names.dedup();

                for t in plan.tasks.iter_mut() {
                    if names.iter().any(|n| n == &t.name) {
                        let it = ensure_checklist_item(
                            &mut t.checklist,
                            CHECKLIST_VALIDATE,
                            "Validate DBT",
                        );
                        set_checklist_status(
                            it,
                            ChecklistItemStatus::NeedsUpdate,
                            Some(evidence_from_tool_end(
                                idx,
                                "tool_end_failed",
                                name,
                                tool_id,
                                ts,
                            )),
                        );
                        recompute_model_task_status(t);
                    }
                }
            }
            plan.progress.last_applied_step_idx = idx + 1;
            continue;
        }

        // Capture dbt_files patch activity and map to schema-contract checklist items.
        if name == "dbt_files" {
            let op = args.get("op").and_then(|v| v.as_str()).unwrap_or("");
            if op == "patch" {
                let ok = observation.ok;
                let paths = extract_dbt_files_paths(op, args);
                let mut stems: Vec<String> = paths.iter().filter_map(|p| file_stem(p)).collect();
                stems.sort();
                stems.dedup();

                // Special-case: patching models/schema.yml (canonical schema file) should satisfy
                // schema_contract for any models present in the YAML content, not by file stem.
                let touched_models_schema_yml = paths
                    .iter()
                    .any(|p| p.trim().replace('\\', "/") == "models/schema.yml");
                if ok && touched_models_schema_yml {
                    let Some(schema_checklist_item_id) = step_ctx
                        .as_ref()
                        .and_then(|c| c.checklist_item_id.as_deref())
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                    else {
                        plan.progress.last_applied_step_idx = idx + 1;
                        continue;
                    };
                    // Best-effort: recover updated models/schema.yml content from patch_text and
                    // mark checklist done for model names present in that YAML.
                    let mut schema_text: Option<String> = None;
                    if let Some(pt) = args.get("patch_text").and_then(|v| v.as_str()) {
                        let guarded_schema = args
                            .get("path")
                            .and_then(|v| v.as_str())
                            .map(|p| p.trim().replace('\\', "/") == "models/schema.yml")
                            .unwrap_or(false);
                        let has_diff_git = pt.lines().any(|l| l.trim_start().starts_with("diff --git "));
                        let mut in_target = guarded_schema && !has_diff_git;
                        let mut added: Vec<String> = Vec::new();
                        for line in pt.lines() {
                            let t = line.trim_end_matches('\r');
                            if t.trim_start().starts_with("diff --git ") {
                                let low = t.to_ascii_lowercase();
                                in_target = low.contains(" b/models/schema.yml");
                                continue;
                            }
                            if !in_target {
                                continue;
                            }
                            if t.starts_with("--- ") || t.starts_with("+++ ") || t.starts_with("@@ ") || t == "@@" {
                                continue;
                            }
                            if let Some(rest) = t.strip_prefix('+') {
                                if !t.starts_with("+++") {
                                    added.push(rest.to_string());
                                }
                            }
                        }
                        if !added.is_empty() {
                            schema_text = Some(added.join("\n"));
                        }
                    }
                    if let Some(text) = schema_text {
                        if let Ok(vy) = serde_yaml::from_str::<serde_yaml::Value>(&text) {
                            if let Some(models) = vy.get("models").and_then(|m| m.as_sequence()) {
                                for m in models.iter() {
                                    let model_name = m
                                        .get("name")
                                        .and_then(|n| n.as_str())
                                        .map(|s| s.trim().to_string())
                                        .filter(|s| !s.is_empty());
                                    if let Some(nm) = model_name {
                                        if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == nm) {
                                            let it = if schema_checklist_item_id == CHECKLIST_SCHEMA_CONTRACT {
                                                ensure_checklist_item(
                                                    &mut t.checklist,
                                                    CHECKLIST_SCHEMA_CONTRACT,
                                                    "Author schema contract",
                                                )
                                            } else {
                                                ensure_checklist_item_any(
                                                    &mut t.checklist,
                                                    &schema_checklist_item_id,
                                                )
                                            };
                                            set_checklist_status(
                                                it,
                                                ChecklistItemStatus::Done,
                                                Some(evidence_from_tool_end(
                                                    idx,
                                                    "tool_end_ok",
                                                    name,
                                                    tool_id,
                                                    ts,
                                                )),
                                            );
                                            recompute_model_task_status(t);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                let Some(schema_checklist_item_id) = step_ctx
                    .as_ref()
                    .and_then(|c| c.checklist_item_id.as_deref())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                else {
                    plan.progress.last_applied_step_idx = idx + 1;
                    continue;
                };
                for t in plan.tasks.iter_mut() {
                    if stems.iter().any(|s| s == &t.name) {
                        let it = if schema_checklist_item_id == CHECKLIST_SCHEMA_CONTRACT {
                            ensure_checklist_item(
                                &mut t.checklist,
                                CHECKLIST_SCHEMA_CONTRACT,
                                "Author schema contract",
                            )
                        } else {
                            ensure_checklist_item_any(&mut t.checklist, &schema_checklist_item_id)
                        };
                        if ok {
                            set_checklist_status(
                                it,
                                ChecklistItemStatus::Done,
                                Some(evidence_from_tool_end(
                                    idx,
                                    "tool_end_ok",
                                    name,
                                    tool_id,
                                    ts,
                                )),
                            );
                        } else {
                            set_checklist_status(
                                it,
                                ChecklistItemStatus::NeedsUpdate,
                                Some(evidence_from_tool_end(
                                    idx,
                                    "tool_end_failed",
                                    name,
                                    tool_id,
                                    ts,
                                )),
                            );
                        }
                        recompute_model_task_status(t);
                    }
                }

                plan.progress.last_applied_step_idx = idx + 1;
                continue;
            }
        }
    }
}

pub fn summarize_cleanse_plan(plan: &CleansePlan, max_lines: usize) -> String {
    let total = plan.tasks.len();
    let done = plan
        .tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Done)
        .count();
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "Cleanse plan: status={:?} ({done}/{total} done)",
        plan.status
    ));
    lines.push("Next batches (dataset_ids):".to_string());
    for (i, b) in plan.batches.iter().take(6).enumerate() {
        lines.push(format!("- batch {}: {}", i + 1, b.join(", ")));
        if lines.len() >= max_lines {
            break;
        }
    }
    lines.join("\n")
}

pub fn summarize_model_plan(plan: &ModelPlan, max_lines: usize) -> String {
    let total = plan.tasks.len();
    let done = plan
        .tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Done)
        .count();
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "Model plan: status={:?} ({done}/{total} done)",
        plan.status
    ));
    lines.push("Next batches (model names):".to_string());
    for (i, b) in plan.batches.iter().take(6).enumerate() {
        lines.push(format!("- batch {}: {}", i + 1, b.join(", ")));
        if lines.len() >= max_lines {
            break;
        }
    }
    lines.join("\n")
}

pub fn parse_plan_json(answer: &str) -> Option<Value> {
    // Hard cutover: the entire answer must be JSON.
    serde_json::from_str::<Value>(answer.trim()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use react_core::session::{ExecutionContext, ThreadLog, ThreadStep, ToolObservation};

    fn step(action: &str, args: Value, observation: Value) -> ThreadStep {
        let obs = ToolObservation::normalize(observation);
        ThreadStep::ToolEnd {
            tool_id: "t".to_string(),
            name: action.to_string(),
            clean_name: action.to_string(),
            args,
            status: if obs.ok {
                "ok".to_string()
            } else {
                "failed".to_string()
            },
            payload: None,
            ctx: None,
            observation: obs,
            ts: "t".to_string(),
            agent: "test".to_string(),
        }
    }

    fn step_with_ctx(
        action: &str,
        args: Value,
        observation: Value,
        ctx: Option<ExecutionContext>,
    ) -> ThreadStep {
        let obs = ToolObservation::normalize(observation);
        ThreadStep::ToolEnd {
            tool_id: "t".to_string(),
            name: action.to_string(),
            clean_name: action.to_string(),
            args,
            status: if obs.ok {
                "ok".to_string()
            } else {
                "failed".to_string()
            },
            payload: None,
            ctx,
            observation: obs,
            ts: "t".to_string(),
            agent: "test".to_string(),
        }
    }

    fn std_checklist(sql_label: &str) -> Vec<PlanChecklistItem> {
        vec![
            PlanChecklistItem {
                checklist_item_id: CHECKLIST_SQL_MODEL.to_string(),
                label: sql_label.to_string(),
                details: None,
                status: ChecklistItemStatus::Pending,
                origin: ChecklistOrigin::Initial,
                origin_step_idx: None,
                evidence: vec![],
            },
            PlanChecklistItem {
                checklist_item_id: CHECKLIST_SCHEMA_CONTRACT.to_string(),
                label: "Author schema contract".to_string(),
                details: None,
                status: ChecklistItemStatus::Pending,
                origin: ChecklistOrigin::Initial,
                origin_step_idx: None,
                evidence: vec![],
            },
            PlanChecklistItem {
                checklist_item_id: CHECKLIST_VALIDATE.to_string(),
                label: "Validate".to_string(),
                details: None,
                status: ChecklistItemStatus::Pending,
                origin: ChecklistOrigin::Initial,
                origin_step_idx: None,
                evidence: vec![],
            },
        ]
    }

    fn dummy_cleanse_spec() -> CleanseImplementationSpec {
        CleanseImplementationSpec {
            spec_version: 1,
            row_preserving: true,
            output_fields: vec![OutputFieldSpec {
                name: "id_raw".to_string(),
                kind: FieldKind::Raw,
                source_columns: vec!["id".to_string()],
                expression: "id as id_raw (raw)".to_string(),
                data_type: None,
                nullable: true,
                description: None,
            }],
            prohibited_ops: vec![],
        }
    }

    fn dummy_model_spec() -> ModelImplementationSpec {
        ModelImplementationSpec {
            spec_version: 1,
            grain: "1 row per id".to_string(),
            inputs: vec!["stg_x".to_string()],
            joins: vec![],
            metrics: vec![],
            output_fields: vec![OutputFieldSpec {
                name: "id".to_string(),
                kind: FieldKind::Clean,
                source_columns: vec!["id".to_string()],
                expression: "id passthrough".to_string(),
                data_type: None,
                nullable: true,
                description: None,
            }],
            assumptions: vec![],
        }
    }

    #[test]
    fn validate_cleanse_plan_semantics_rejects_missing_design_spec_details() {
        let mut bad_spec = dummy_cleanse_spec();
        bad_spec.output_fields = vec![];
        let plan = CleansePlan {
            plan_key: "k".to_string(),
            status: PlanStatus::Draft,
            project_snapshot: serde_json::json!({}),
            tasks: vec![CleanseTask {
                dataset_id: "a.b.c".to_string(),
                expected_model_path: Some("models/staging/stg_b_c.sql".to_string()),
                invariants: vec![],
                implementation_spec: bad_spec,
                status: TaskStatus::Pending,
                checklist: std_checklist("Author staging SQL"),
            }],
            batches: vec![vec!["a.b.c".to_string()]],
            work_groups: vec![],
            mutations: vec![],
            progress: PlanProgress::default(),
        };
        let v = validate_cleanse_plan_semantics(&plan);
        assert!(!v.ok);
        assert!(v.errors.iter().any(|e| e.contains("implementation_spec.output_fields is empty")));
    }

    #[test]
    fn validate_model_plan_semantics_rejects_missing_grain_in_design_spec() {
        let mut bad_spec = dummy_model_spec();
        bad_spec.grain = "".to_string();
        let plan = ModelPlan {
            plan_key: "k".to_string(),
            status: PlanStatus::Draft,
            project_snapshot: serde_json::json!({}),
            tasks: vec![ModelTask {
                name: "m".to_string(),
                folder: "marts".to_string(),
                goal: "g".to_string(),
                inputs: vec!["stg_x".to_string()],
                expected_model_path: Some("models/marts/m.sql".to_string()),
                invariants: vec![],
                implementation_spec: bad_spec,
                status: TaskStatus::Pending,
                checklist: std_checklist("Author gold SQL"),
            }],
            batches: vec![vec!["m".to_string()]],
            work_groups: vec![],
            mutations: vec![],
            progress: PlanProgress::default(),
        };
        let allowed = std::collections::BTreeSet::from(["stg_x".to_string()]);
        let v = validate_model_plan_semantics(&plan, Some(&allowed));
        assert!(!v.ok);
        assert!(v.errors.iter().any(|e| e.contains("implementation_spec.grain is empty")));
    }

    fn status_of(items: &[PlanChecklistItem], id: &str) -> ChecklistItemStatus {
        items
            .iter()
            .find(|it| it.checklist_item_id == id)
            .map(|it| it.status)
            .unwrap_or(ChecklistItemStatus::Pending)
    }

    #[test]
    fn cleanse_progress_marks_sql_done_but_task_not_done_until_schema_and_validate() {
        let mut plan = CleansePlan {
            plan_key: "k".to_string(),
            status: PlanStatus::Approved,
            project_snapshot: serde_json::json!({}),
            tasks: vec![
                CleanseTask {
                    dataset_id: "a.b.c".to_string(),
                    expected_model_path: None,
                    invariants: vec![],
                    implementation_spec: dummy_cleanse_spec(),
                    status: TaskStatus::Pending,
                    checklist: std_checklist("Author staging SQL"),
                },
                CleanseTask {
                    dataset_id: "d.e.f".to_string(),
                    expected_model_path: None,
                    invariants: vec![],
                    implementation_spec: dummy_cleanse_spec(),
                    status: TaskStatus::Pending,
                    checklist: std_checklist("Author staging SQL"),
                },
            ],
            batches: vec![vec!["a.b.c".to_string()], vec!["d.e.f".to_string()]],
            work_groups: vec![],
            mutations: vec![],
            progress: PlanProgress::default(),
        };

        let log = ThreadLog {
            steps: vec![
                step(
                    "staging_model",
                    serde_json::json!({"dataset_ids":["a.b.c"]}),
                    serde_json::json!({"ok": true, "succeeded_dataset_ids":["a.b.c"]}),
                ),
                step(
                    "staging_model",
                    serde_json::json!({"dataset_ids":["d.e.f"]}),
                    serde_json::json!({"ok": false, "succeeded_dataset_ids":["d.e.f"]}),
                ),
            ],
            ..Default::default()
        };

        update_cleanse_progress_from_log(&mut plan, &log);
        assert_eq!(
            status_of(&plan.tasks[0].checklist, CHECKLIST_SQL_MODEL),
            ChecklistItemStatus::Done
        );
        assert_eq!(
            status_of(&plan.tasks[1].checklist, CHECKLIST_SQL_MODEL),
            ChecklistItemStatus::Done
        );
        // Other checklist items remain pending, so tasks are not done.
        assert_eq!(plan.tasks[0].status, TaskStatus::InProgress);
        assert_eq!(plan.tasks[1].status, TaskStatus::InProgress);
        assert!(!cleanse_all_done(&plan));
        // Note: being "done" is tracked at the task level. PlanStatus::Completed is reserved for
        // "validate passed", and is set by the suite after dbt_validate succeeds.
        assert_eq!(plan.status, PlanStatus::Approved);
    }

    #[test]
    fn model_progress_marks_sql_done_but_task_not_done_until_schema_and_validate() {
        let mut plan = ModelPlan {
            plan_key: "k".to_string(),
            status: PlanStatus::Approved,
            project_snapshot: serde_json::json!({}),
            tasks: vec![
                ModelTask {
                    name: "fct_a".to_string(),
                    folder: "marts".to_string(),
                    goal: "a".to_string(),
                    inputs: vec!["stg_x".to_string()],
                    expected_model_path: None,
                    invariants: vec![],
                    implementation_spec: dummy_model_spec(),
                    status: TaskStatus::Pending,
                    checklist: std_checklist("Author gold SQL"),
                },
                ModelTask {
                    name: "dim_b".to_string(),
                    folder: "core".to_string(),
                    goal: "b".to_string(),
                    inputs: vec!["stg_y".to_string()],
                    expected_model_path: None,
                    invariants: vec![],
                    implementation_spec: dummy_model_spec(),
                    status: TaskStatus::Pending,
                    checklist: std_checklist("Author gold SQL"),
                },
            ],
            batches: vec![vec!["fct_a".to_string(), "dim_b".to_string()]],
            work_groups: vec![],
            mutations: vec![],
            progress: PlanProgress::default(),
        };

        let log = ThreadLog {
            steps: vec![step(
                "gold_model",
                serde_json::json!({"items":[{"name":"fct_a"},{"name":"dim_b"}]}),
                serde_json::json!({"ok": true, "succeeded_item_names":["fct_a","dim_b"]}),
            )],
            ..Default::default()
        };

        update_model_progress_from_log(&mut plan, &log);
        assert_eq!(
            status_of(&plan.tasks[0].checklist, CHECKLIST_SQL_MODEL),
            ChecklistItemStatus::Done
        );
        assert_eq!(
            status_of(&plan.tasks[1].checklist, CHECKLIST_SQL_MODEL),
            ChecklistItemStatus::Done
        );
        assert_eq!(plan.tasks[0].status, TaskStatus::InProgress);
        assert_eq!(plan.tasks[1].status, TaskStatus::InProgress);
        assert!(!model_all_done(&plan));
        // Note: PlanStatus::Completed is set only after dbt_validate passes.
        assert_eq!(plan.status, PlanStatus::Approved);
    }

    #[test]
    fn cleanse_progress_marks_custom_checklist_from_apply_next_cleanse_batch_ctx() {
        let mut plan = CleansePlan {
            plan_key: "k".to_string(),
            status: PlanStatus::Approved,
            project_snapshot: serde_json::json!({}),
            tasks: vec![CleanseTask {
                dataset_id: "a.b.c".to_string(),
                expected_model_path: None,
                invariants: vec![],
                implementation_spec: dummy_cleanse_spec(),
                status: TaskStatus::Pending,
                checklist: vec![
                    PlanChecklistItem {
                        checklist_item_id: CHECKLIST_SQL_MODEL.to_string(),
                        label: "Author staging SQL".to_string(),
                        details: None,
                        status: ChecklistItemStatus::Pending,
                        origin: ChecklistOrigin::Initial,
                        origin_step_idx: None,
                        evidence: vec![],
                    },
                    PlanChecklistItem {
                        checklist_item_id: "time_derivatives".to_string(),
                        label: "Time derivatives".to_string(),
                        details: None,
                        status: ChecklistItemStatus::Pending,
                        origin: ChecklistOrigin::Initial,
                        origin_step_idx: None,
                        evidence: vec![],
                    },
                ],
            }],
            batches: vec![vec!["a.b.c".to_string()]],
            work_groups: vec![],
            mutations: vec![],
            progress: PlanProgress::default(),
        };
        let log = ThreadLog {
            steps: vec![step_with_ctx(
                "apply_next_cleanse_batch",
                serde_json::json!({}),
                serde_json::json!({
                    "ok": true,
                    "attempted_dataset_ids":["a.b.c"],
                    "succeeded_dataset_ids":["a.b.c"]
                }),
                Some(ExecutionContext {
                    plan_kind: Some("cleanse".to_string()),
                    plan_key: Some("k".to_string()),
                    workgroup_id: Some("wg".to_string()),
                    task_id: Some("a.b.c".to_string()),
                    checklist_item_id: Some("time_derivatives".to_string()),
                }),
            )],
            ..Default::default()
        };
        update_cleanse_progress_from_log(&mut plan, &log);
        assert_eq!(
            status_of(&plan.tasks[0].checklist, "time_derivatives"),
            ChecklistItemStatus::Done
        );
        assert_eq!(
            status_of(&plan.tasks[0].checklist, CHECKLIST_SQL_MODEL),
            ChecklistItemStatus::Pending
        );
    }

    #[test]
    fn cleanse_progress_marks_custom_schema_checklist_from_apply_next_cleanse_schema_batch_ctx() {
        let mut plan = CleansePlan {
            plan_key: "k".to_string(),
            status: PlanStatus::Approved,
            project_snapshot: serde_json::json!({}),
            tasks: vec![CleanseTask {
                dataset_id: "a.b.c".to_string(),
                expected_model_path: None,
                invariants: vec![],
                implementation_spec: dummy_cleanse_spec(),
                status: TaskStatus::Pending,
                checklist: vec![
                    PlanChecklistItem {
                        checklist_item_id: CHECKLIST_SQL_MODEL.to_string(),
                        label: "Author staging SQL".to_string(),
                        details: None,
                        status: ChecklistItemStatus::Done,
                        origin: ChecklistOrigin::Initial,
                        origin_step_idx: None,
                        evidence: vec![],
                    },
                    PlanChecklistItem {
                        checklist_item_id: "collision_id_contract".to_string(),
                        label: "Collision id contract".to_string(),
                        details: None,
                        status: ChecklistItemStatus::Pending,
                        origin: ChecklistOrigin::Initial,
                        origin_step_idx: None,
                        evidence: vec![],
                    },
                ],
            }],
            batches: vec![vec!["a.b.c".to_string()]],
            work_groups: vec![],
            mutations: vec![],
            progress: PlanProgress::default(),
        };
        let log = ThreadLog {
            steps: vec![step_with_ctx(
                "apply_next_cleanse_schema_batch",
                serde_json::json!({}),
                serde_json::json!({
                    "ok": true,
                    "attempted_dataset_ids":["a.b.c"],
                    "succeeded_dataset_ids":["a.b.c"]
                }),
                Some(ExecutionContext {
                    plan_kind: Some("cleanse".to_string()),
                    plan_key: Some("k".to_string()),
                    workgroup_id: Some("wg".to_string()),
                    task_id: Some("a.b.c".to_string()),
                    checklist_item_id: Some("collision_id_contract".to_string()),
                }),
            )],
            ..Default::default()
        };
        update_cleanse_progress_from_log(&mut plan, &log);
        assert_eq!(
            status_of(&plan.tasks[0].checklist, "collision_id_contract"),
            ChecklistItemStatus::Done
        );
    }

    #[test]
    fn model_progress_marks_schema_contract_done_from_models_schema_yml_patch() {
        let mut plan = ModelPlan {
            plan_key: "k".to_string(),
            status: PlanStatus::Approved,
            project_snapshot: serde_json::json!({}),
            tasks: vec![
                ModelTask {
                    name: "dim_customers".to_string(),
                    folder: "marts".to_string(),
                    goal: "a".to_string(),
                    inputs: vec!["stg_test_raw_raw_customers".to_string()],
                    expected_model_path: None,
                    invariants: vec![],
                    implementation_spec: dummy_model_spec(),
                    status: TaskStatus::Pending,
                    checklist: std_checklist("Author gold SQL"),
                },
                ModelTask {
                    name: "dim_orders".to_string(),
                    folder: "marts".to_string(),
                    goal: "b".to_string(),
                    inputs: vec!["stg_test_raw_raw_orders".to_string()],
                    expected_model_path: None,
                    invariants: vec![],
                    implementation_spec: dummy_model_spec(),
                    status: TaskStatus::Pending,
                    checklist: std_checklist("Author gold SQL"),
                },
            ],
            batches: vec![vec!["dim_customers".to_string(), "dim_orders".to_string()]],
            work_groups: vec![],
            mutations: vec![],
            progress: PlanProgress::default(),
        };

        let log = ThreadLog {
            steps: vec![step_with_ctx(
                "dbt_files",
                serde_json::json!({
                    "op":"patch",
                    "path": "models/schema.yml",
                    "patch_text": "@@ ... @@\n+version: 2\n+\n+models:\n+  - name: dim_customers\n+    columns:\n+      - name: customer_id\n+sources:\n+  - name: test_raw\n"
                }),
                serde_json::json!({"ok": true}),
                Some(ExecutionContext {
                    plan_kind: Some("model".to_string()),
                    plan_key: Some("k".to_string()),
                    workgroup_id: Some("wg".to_string()),
                    task_id: Some("dim_customers".to_string()),
                    checklist_item_id: Some(CHECKLIST_SCHEMA_CONTRACT.to_string()),
                }),
            )],
            ..Default::default()
        };

        update_model_progress_from_log(&mut plan, &log);
        assert_eq!(
            status_of(&plan.tasks[0].checklist, CHECKLIST_SCHEMA_CONTRACT),
            ChecklistItemStatus::Done
        );
        assert_eq!(
            status_of(&plan.tasks[1].checklist, CHECKLIST_SCHEMA_CONTRACT),
            ChecklistItemStatus::Pending
        );
    }

    #[test]
    fn prune_cleanse_plan_to_grounded_raw_datasets_prunes_tasks_and_batches() {
        let mut plan = CleansePlan {
            plan_key: "k".to_string(),
            status: PlanStatus::Draft,
            project_snapshot: serde_json::json!({}),
            tasks: vec![
                CleanseTask {
                    dataset_id: "AwsDataCatalog.test_raw.raw_customers".to_string(),
                    expected_model_path: None,
                    invariants: vec![],
                    implementation_spec: dummy_cleanse_spec(),
                    status: TaskStatus::Pending,
                    checklist: vec![],
                },
                CleanseTask {
                    dataset_id: "AwsDataCatalog.test_raw.raw_products".to_string(),
                    expected_model_path: None,
                    invariants: vec![],
                    implementation_spec: dummy_cleanse_spec(),
                    status: TaskStatus::Pending,
                    checklist: vec![],
                },
            ],
            batches: vec![vec![
                "AwsDataCatalog.test_raw.raw_customers".to_string(),
                "AwsDataCatalog.test_raw.raw_products".to_string(),
            ]],
            work_groups: vec![],
            mutations: vec![],
            progress: PlanProgress::default(),
        };
        let mut allowed = std::collections::BTreeSet::new();
        allowed.insert("AwsDataCatalog.test_raw.raw_customers".to_string());

        prune_cleanse_plan_to_grounded_raw_datasets(&mut plan, &allowed);
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(
            plan.tasks[0].dataset_id,
            "AwsDataCatalog.test_raw.raw_customers"
        );
        assert_eq!(plan.batches.len(), 1);
        assert_eq!(
            plan.batches[0],
            vec!["AwsDataCatalog.test_raw.raw_customers".to_string()]
        );
    }

    #[test]
    fn prune_model_plan_to_grounded_staging_models_prunes_tasks_with_unproven_inputs() {
        let mut plan = ModelPlan {
            plan_key: "k".to_string(),
            status: PlanStatus::Draft,
            project_snapshot: serde_json::json!({}),
            tasks: vec![
                ModelTask {
                    name: "fct_ok".to_string(),
                    folder: "marts".to_string(),
                    goal: "ok".to_string(),
                    inputs: vec!["stg_customers".to_string()],
                    expected_model_path: None,
                    invariants: vec![],
                    implementation_spec: dummy_model_spec(),
                    status: TaskStatus::Pending,
                    checklist: vec![],
                },
                ModelTask {
                    name: "fct_bad".to_string(),
                    folder: "marts".to_string(),
                    goal: "bad".to_string(),
                    inputs: vec!["raw_orders".to_string()],
                    expected_model_path: None,
                    invariants: vec![],
                    implementation_spec: dummy_model_spec(),
                    status: TaskStatus::Pending,
                    checklist: vec![],
                },
            ],
            batches: vec![vec!["fct_ok".to_string(), "fct_bad".to_string()]],
            work_groups: vec![],
            mutations: vec![],
            progress: PlanProgress::default(),
        };
        let mut allowed = std::collections::BTreeSet::new();
        allowed.insert("stg_customers".to_string());

        prune_model_plan_to_grounded_staging_models(&mut plan, &allowed);
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.tasks[0].name, "fct_ok");
        assert_eq!(plan.batches.len(), 1);
        assert_eq!(plan.batches[0], vec!["fct_ok".to_string()]);
    }

    #[test]
    fn model_progress_scoping_prevents_replaying_old_tool_steps() {
        // Prior-cycle authoring produced a success for dim_customers.
        let log = ThreadLog {
            steps: vec![step(
                "gold_model",
                serde_json::json!({"items":[{"name":"dim_customers"}]}),
                serde_json::json!({"ok": true, "succeeded_item_names":["dim_customers"]}),
            )],
            ..Default::default()
        };

        // New plan instance reuses the same task name, but MUST NOT be auto-completed by old log history.
        let mut plan = ModelPlan {
            plan_key: "k2".to_string(),
            status: PlanStatus::Approved,
            project_snapshot: serde_json::json!({}),
            tasks: vec![ModelTask {
                name: "dim_customers".to_string(),
                folder: "marts".to_string(),
                goal: "x".to_string(),
                inputs: vec![],
                expected_model_path: None,
                invariants: vec![],
                implementation_spec: dummy_model_spec(),
                status: TaskStatus::Pending,
                checklist: std_checklist("Author gold SQL"),
            }],
            batches: vec![vec!["dim_customers".to_string()]],
            work_groups: vec![],
            mutations: vec![],
            progress: PlanProgress {
                // Critical: scope progress cursor beyond the historical log we already have.
                last_applied_step_idx: log.steps.len(),
                ..Default::default()
            },
        };

        update_model_progress_from_log(&mut plan, &log);
        assert_eq!(plan.tasks[0].status, TaskStatus::Pending);
        assert_eq!(model_next_batch(&plan), vec!["dim_customers".to_string()]);
    }

    #[test]
    fn ensure_expected_model_paths_cleanse_sets_canonical_path() {
        let mut plan = CleansePlan {
            plan_key: "k".to_string(),
            status: PlanStatus::Approved,
            project_snapshot: serde_json::json!({}),
            tasks: vec![CleanseTask {
                dataset_id: "AwsDataCatalog.test_raw.raw_customers".to_string(),
                expected_model_path: None,
                invariants: vec![],
                implementation_spec: dummy_cleanse_spec(),
                status: TaskStatus::Pending,
                checklist: vec![],
            }],
            batches: vec![vec!["AwsDataCatalog.test_raw.raw_customers".to_string()]],
            work_groups: vec![],
            mutations: vec![],
            progress: PlanProgress::default(),
        };
        ensure_expected_model_paths_cleanse(None, &mut plan);
        assert_eq!(
            plan.tasks[0].expected_model_path.as_deref(),
            Some("models/staging/stg_test_raw_raw_customers.sql")
        );
    }

    #[test]
    fn ensure_expected_model_paths_cleanse_overwrites_noncanonical_path() {
        let mut plan = CleansePlan {
            plan_key: "k".to_string(),
            status: PlanStatus::Approved,
            project_snapshot: serde_json::json!({}),
            tasks: vec![CleanseTask {
                dataset_id: "AwsDataCatalog.test_raw.raw_customers".to_string(),
                expected_model_path: Some("models/staging/customers.sql".to_string()),
                invariants: vec![],
                implementation_spec: dummy_cleanse_spec(),
                status: TaskStatus::Pending,
                checklist: vec![],
            }],
            batches: vec![vec!["AwsDataCatalog.test_raw.raw_customers".to_string()]],
            work_groups: vec![],
            mutations: vec![],
            progress: PlanProgress::default(),
        };
        let changed = ensure_expected_model_paths_cleanse(None, &mut plan);
        assert!(changed);
        assert_eq!(
            plan.tasks[0].expected_model_path.as_deref(),
            Some("models/staging/stg_test_raw_raw_customers.sql")
        );
    }

    #[test]
    fn successful_mutating_patch_resets_consecutive_batch_failures() {
        let mut plan = CleansePlan {
            plan_key: "k".to_string(),
            status: PlanStatus::Approved,
            project_snapshot: serde_json::json!({}),
            tasks: vec![CleanseTask {
                dataset_id: "AwsDataCatalog.test_raw.raw_customers".to_string(),
                expected_model_path: Some(
                    "models/staging/stg_test_raw_raw_customers.sql".to_string(),
                ),
                invariants: vec![],
                implementation_spec: dummy_cleanse_spec(),
                status: TaskStatus::NeedsUpdate,
                checklist: vec![],
            }],
            batches: vec![vec!["AwsDataCatalog.test_raw.raw_customers".to_string()]],
            work_groups: vec![],
            mutations: vec![],
            progress: PlanProgress {
                consecutive_batch_failures: 3,
                total_batch_failures: 3,
                last_applied_step_idx: 0,
            },
        };
        let log = ThreadLog {
            steps: vec![step(
                "dbt_files",
                serde_json::json!({
                    "op":"patch",
                    "path": "models/staging/stg_test_raw_raw_customers.sql",
                    "patch_text":"@@ ... @@\n+select 1\n"
                }),
                serde_json::json!({"ok": true, "mutated": true}),
            )],
            ..Default::default()
        };
        update_cleanse_progress_from_log(&mut plan, &log);
        assert_eq!(plan.progress.consecutive_batch_failures, 0);
    }

    #[test]
    fn cleanse_validate_ok_marks_validate_done_for_sql_done_tasks() {
        let mut plan = CleansePlan {
            plan_key: "k".to_string(),
            status: PlanStatus::Approved,
            project_snapshot: serde_json::json!({}),
            tasks: vec![
                CleanseTask {
                    dataset_id: "AwsDataCatalog.test_raw.raw_orders".to_string(),
                    expected_model_path: Some("models/staging/stg_test_raw_raw_orders.sql".to_string()),
                    invariants: vec![],
                    implementation_spec: dummy_cleanse_spec(),
                    status: TaskStatus::Pending,
                    checklist: std_checklist("Author staging SQL"),
                },
                CleanseTask {
                    dataset_id: "AwsDataCatalog.test_raw.raw_customers".to_string(),
                    expected_model_path: Some(
                        "models/staging/stg_test_raw_raw_customers.sql".to_string(),
                    ),
                    invariants: vec![],
                    implementation_spec: dummy_cleanse_spec(),
                    status: TaskStatus::Pending,
                    checklist: std_checklist("Author staging SQL"),
                },
            ],
            batches: vec![vec![
                "AwsDataCatalog.test_raw.raw_orders".to_string(),
                "AwsDataCatalog.test_raw.raw_customers".to_string(),
            ]],
            work_groups: vec![],
            mutations: vec![],
            progress: PlanProgress::default(),
        };

        // Pretend SQL authoring is done so validate can be marked complete.
        for t in plan.tasks.iter_mut() {
            let it = ensure_checklist_item(&mut t.checklist, CHECKLIST_SQL_MODEL, "Author staging SQL");
            it.status = ChecklistItemStatus::Done;
            recompute_cleanse_task_status(t);
        }

        let log = ThreadLog {
            steps: vec![step(
                "dbt_validate",
                serde_json::json!({}),
                serde_json::json!({"ok": true}),
            )],
            ..Default::default()
        };
        update_cleanse_progress_from_log(&mut plan, &log);

        for t in plan.tasks.iter() {
            assert_eq!(
                status_of(&t.checklist, CHECKLIST_VALIDATE),
                ChecklistItemStatus::Done
            );
        }
    }

    #[test]
    fn cleanse_validate_failure_marks_needs_update_for_implicated_tasks_only() {
        let mut plan = CleansePlan {
            plan_key: "k".to_string(),
            status: PlanStatus::Approved,
            project_snapshot: serde_json::json!({}),
            tasks: vec![
                CleanseTask {
                    dataset_id: "AwsDataCatalog.test_raw.raw_orders".to_string(),
                    expected_model_path: Some("models/staging/stg_test_raw_raw_orders.sql".to_string()),
                    invariants: vec![],
                    implementation_spec: dummy_cleanse_spec(),
                    status: TaskStatus::Pending,
                    checklist: std_checklist("Author staging SQL"),
                },
                CleanseTask {
                    dataset_id: "AwsDataCatalog.test_raw.raw_customers".to_string(),
                    expected_model_path: Some(
                        "models/staging/stg_test_raw_raw_customers.sql".to_string(),
                    ),
                    invariants: vec![],
                    implementation_spec: dummy_cleanse_spec(),
                    status: TaskStatus::Pending,
                    checklist: std_checklist("Author staging SQL"),
                },
            ],
            batches: vec![vec![
                "AwsDataCatalog.test_raw.raw_orders".to_string(),
                "AwsDataCatalog.test_raw.raw_customers".to_string(),
            ]],
            work_groups: vec![],
            mutations: vec![],
            progress: PlanProgress::default(),
        };

        // Mark both as SQL-done so validate failures are meaningful.
        for t in plan.tasks.iter_mut() {
            let it = ensure_checklist_item(&mut t.checklist, CHECKLIST_SQL_MODEL, "Author staging SQL");
            it.status = ChecklistItemStatus::Done;
            recompute_cleanse_task_status(t);
        }

        let stdout = "Failure in model stg_test_raw_raw_orders (models/staging/stg_test_raw_raw_orders.sql)\n";
        let log = ThreadLog {
            steps: vec![step(
                "dbt_validate",
                serde_json::json!({}),
                serde_json::json!({
                    "ok": false,
                    "logs": { "run_or_build": { "stdout": stdout } }
                }),
            )],
            ..Default::default()
        };

        update_cleanse_progress_from_log(&mut plan, &log);

        assert_eq!(
            status_of(&plan.tasks[0].checklist, CHECKLIST_VALIDATE),
            ChecklistItemStatus::NeedsUpdate
        );
        assert_eq!(
            status_of(&plan.tasks[1].checklist, CHECKLIST_VALIDATE),
            ChecklistItemStatus::Pending
        );
    }
}
