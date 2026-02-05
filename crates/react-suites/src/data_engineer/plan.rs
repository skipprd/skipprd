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

fn extract_dbt_files_patch_paths(args: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    // Optional single-file guard.
    if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
        let p = p.trim();
        if !p.is_empty() {
            out.push(p.to_string());
        }
    }

    // Structured patch primitives can embed paths.
    for key in ["replace_file", "replace_range", "replace_list"] {
        let Some(v) = args.get(key) else { continue };
        let mut visit = |obj: &serde_json::Map<String, Value>| {
            if let Some(p) = obj.get("path").and_then(|v| v.as_str()) {
                let p = p.trim();
                if !p.is_empty() {
                    out.push(p.to_string());
                }
            }
        };
        if let Some(arr) = v.as_array() {
            for it in arr.iter() {
                if let Some(obj) = it.as_object() {
                    visit(obj);
                }
            }
        } else if let Some(obj) = v.as_object() {
            visit(obj);
        }
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
pub struct WorkGroupItemRef {
    pub task_id: String,
    pub checklist_item_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlanWorkGroup {
    pub group_id: String,
    pub label: String,
    pub kind: WorkGroupKind,
    pub items: Vec<WorkGroupItemRef>,
    #[serde(default)]
    pub depends_on_group_ids: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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
    #[serde(default)]
    pub status: TaskStatus,
    #[serde(default)]
    pub checklist: Vec<PlanChecklistItem>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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
    #[serde(default)]
    pub progress: PlanProgress,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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
    #[serde(default)]
    pub status: TaskStatus,
    #[serde(default)]
    pub checklist: Vec<PlanChecklistItem>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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

fn ensure_expected_model_paths_cleanse(plan: &mut CleansePlan) {
    for t in plan.tasks.iter_mut() {
        let missing = t
            .expected_model_path
            .as_deref()
            .map(|s| s.trim().is_empty())
            .unwrap_or(true);
        if !missing {
            continue;
        }
        let Some((_cat, schema, table)) = parse_dataset_id_3(&t.dataset_id) else {
            continue;
        };
        t.expected_model_path = Some(naming::canonical_staging_rel_path(&schema, &table));
    }
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

pub async fn load_cleanse_plan_by_key(ctx: &AgentCtx, key: &str) -> Option<CleansePlan> {
    let bytes = ctx.storage.get_bytes(key).await.ok()?;
    let mut p = serde_json::from_slice::<CleansePlan>(&bytes).ok()?;
    if p.plan_key.trim().is_empty() {
        p.plan_key = key.to_string();
    }
    ensure_expected_model_paths_cleanse(&mut p);
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
                if checklist_status(&t.checklist, CHECKLIST_SQL_MODEL) != ChecklistItemStatus::Done
                {
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
                if checklist_status(&t.checklist, CHECKLIST_SQL_MODEL) != ChecklistItemStatus::Done
                {
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

const CHECKLIST_SQL_MODEL: &str = "sql_model";
const CHECKLIST_SCHEMA_CONTRACT: &str = "schema_contract";
const CHECKLIST_VALIDATE: &str = "validate";

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
                    checklist_status(&t.checklist, it.checklist_item_id.as_str())
                        != ChecklistItemStatus::Done
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
                    checklist_status(&t.checklist, it.checklist_item_id.as_str())
                        != ChecklistItemStatus::Done
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

pub fn model_mark_done(plan: &mut ModelPlan, name: &str) {
    if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == name) {
        let it = ensure_checklist_item(&mut t.checklist, CHECKLIST_SQL_MODEL, "Author gold SQL");
        set_checklist_status(it, ChecklistItemStatus::Done, None);
        recompute_model_task_status(t);
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
            ..
        } = step
        else {
            continue;
        };

        // Preferred: derive from structured batch tool output.
        if name == "apply_next_cleanse_batch" {
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
                if let Some(t) = plan.tasks.iter_mut().find(|t| t.dataset_id == *ds) {
                    let it = ensure_checklist_item(
                        &mut t.checklist,
                        CHECKLIST_SQL_MODEL,
                        "Author staging SQL",
                    );
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
            if op == "patch" {
                let ok = observation.ok;
                let preview = args
                    .get("preview_diff")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                let paths = extract_dbt_files_patch_paths(args);
                // SQL patching can be used for targeted remediation; treat successful non-preview patches as progress.
                let mut stems: Vec<String> = paths.iter().filter_map(|p| file_stem(p)).collect();
                stems.sort();
                stems.dedup();
                if !stems.is_empty() {
                    for t in plan.tasks.iter_mut() {
                        let Some(p) = t.expected_model_path.as_deref() else {
                            continue;
                        };
                        let Some(st) = file_stem(p) else {
                            continue;
                        };
                        if stems.iter().any(|s| s == &st) {
                            let it = ensure_checklist_item(
                                &mut t.checklist,
                                CHECKLIST_SQL_MODEL,
                                "Author staging SQL",
                            );
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
                    if ok && !preview {
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
                    // Best-effort: match by expected_model_path stem.
                    for t in plan.tasks.iter_mut() {
                        let Some(p) = t.expected_model_path.as_deref() else {
                            continue;
                        };
                        let Some(st) = file_stem(p) else {
                            continue;
                        };
                        if stems.is_empty() || stems.iter().any(|s| s == &st) || paths.iter().any(|pp| pp.trim() == "models/schema.yml") {
                            let it = ensure_checklist_item(
                                &mut t.checklist,
                                CHECKLIST_SCHEMA_CONTRACT,
                                "Author schema contract",
                            );
                            let ev_kind = if ok { "tool_end_ok" } else { "tool_end_failed" };
                            let new_status = if ok && !preview {
                                ChecklistItemStatus::Done
                            } else if ok {
                                ChecklistItemStatus::InProgress
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
                    if ok && !preview {
                        // Treat a successful non-preview schema patch as forward progress for batching.
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
            ..
        } = step
        else {
            continue;
        };

        // Preferred: derive from structured batch tool output.
        if name == "apply_next_model_batch" {
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
            if !ok || !failed.is_empty() {
                for n in failed.iter() {
                    if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == *n) {
                        let it = ensure_checklist_item(
                            &mut t.checklist,
                            CHECKLIST_SQL_MODEL,
                            "Author gold SQL",
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
                let preview = args
                    .get("preview_diff")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let paths = extract_dbt_files_patch_paths(args);
                let mut stems: Vec<String> = paths.iter().filter_map(|p| file_stem(p)).collect();
                stems.sort();
                stems.dedup();

                for t in plan.tasks.iter_mut() {
                    if stems.iter().any(|s| s == &t.name) {
                        let it = ensure_checklist_item(
                            &mut t.checklist,
                            CHECKLIST_SCHEMA_CONTRACT,
                            "Author schema contract",
                        );
                        if ok {
                            let status = if preview {
                                ChecklistItemStatus::InProgress
                            } else {
                                ChecklistItemStatus::Done
                            };
                            set_checklist_status(
                                it,
                                status,
                                Some(evidence_from_tool_end(
                                    idx,
                                    if preview { "tool_end_preview" } else { "tool_end_ok" },
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
    use react_core::session::{ThreadLog, ThreadStep, ToolObservation};

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
                    status: TaskStatus::Pending,
                    checklist: std_checklist("Author staging SQL"),
                },
                CleanseTask {
                    dataset_id: "d.e.f".to_string(),
                    expected_model_path: None,
                    invariants: vec![],
                    status: TaskStatus::Pending,
                    checklist: std_checklist("Author staging SQL"),
                },
            ],
            batches: vec![vec!["a.b.c".to_string()], vec!["d.e.f".to_string()]],
            work_groups: vec![],
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
                    status: TaskStatus::Pending,
                    checklist: std_checklist("Author gold SQL"),
                },
            ],
            batches: vec![vec!["fct_a".to_string(), "dim_b".to_string()]],
            work_groups: vec![],
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
                    status: TaskStatus::Pending,
                    checklist: vec![],
                },
                CleanseTask {
                    dataset_id: "AwsDataCatalog.test_raw.raw_products".to_string(),
                    expected_model_path: None,
                    invariants: vec![],
                    status: TaskStatus::Pending,
                    checklist: vec![],
                },
            ],
            batches: vec![vec![
                "AwsDataCatalog.test_raw.raw_customers".to_string(),
                "AwsDataCatalog.test_raw.raw_products".to_string(),
            ]],
            work_groups: vec![],
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
                    status: TaskStatus::Pending,
                    checklist: vec![],
                },
            ],
            batches: vec![vec!["fct_ok".to_string(), "fct_bad".to_string()]],
            work_groups: vec![],
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
                status: TaskStatus::Pending,
                checklist: std_checklist("Author gold SQL"),
            }],
            batches: vec![vec!["dim_customers".to_string()]],
            work_groups: vec![],
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
                status: TaskStatus::Pending,
                checklist: vec![],
            }],
            batches: vec![vec!["AwsDataCatalog.test_raw.raw_customers".to_string()]],
            work_groups: vec![],
            progress: PlanProgress::default(),
        };
        ensure_expected_model_paths_cleanse(&mut plan);
        assert_eq!(
            plan.tasks[0].expected_model_path.as_deref(),
            Some("models/staging/stg_test_raw_raw_customers.sql")
        );
    }

    #[test]
    fn successful_non_preview_mutating_patch_resets_consecutive_batch_failures() {
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
                status: TaskStatus::NeedsUpdate,
                checklist: vec![],
            }],
            batches: vec![vec!["AwsDataCatalog.test_raw.raw_customers".to_string()]],
            work_groups: vec![],
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
                    "replace_file": {"path":"models/staging/stg_test_raw_raw_customers.sql","new_text":"select 1\n"}
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
                    status: TaskStatus::Pending,
                    checklist: std_checklist("Author staging SQL"),
                },
                CleanseTask {
                    dataset_id: "AwsDataCatalog.test_raw.raw_customers".to_string(),
                    expected_model_path: Some(
                        "models/staging/stg_test_raw_raw_customers.sql".to_string(),
                    ),
                    invariants: vec![],
                    status: TaskStatus::Pending,
                    checklist: std_checklist("Author staging SQL"),
                },
            ],
            batches: vec![vec![
                "AwsDataCatalog.test_raw.raw_orders".to_string(),
                "AwsDataCatalog.test_raw.raw_customers".to_string(),
            ]],
            work_groups: vec![],
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
                    status: TaskStatus::Pending,
                    checklist: std_checklist("Author staging SQL"),
                },
                CleanseTask {
                    dataset_id: "AwsDataCatalog.test_raw.raw_customers".to_string(),
                    expected_model_path: Some(
                        "models/staging/stg_test_raw_raw_customers.sql".to_string(),
                    ),
                    invariants: vec![],
                    status: TaskStatus::Pending,
                    checklist: std_checklist("Author staging SQL"),
                },
            ],
            batches: vec![vec![
                "AwsDataCatalog.test_raw.raw_orders".to_string(),
                "AwsDataCatalog.test_raw.raw_customers".to_string(),
            ]],
            work_groups: vec![],
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
