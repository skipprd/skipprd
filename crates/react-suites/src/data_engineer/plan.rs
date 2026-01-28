use serde::{Deserialize, Serialize};
use serde_json::Value;

use react_core::agent::AgentCtx;
use react_core::session::{ThreadLog, ThreadStep};
use crate::data_engineer::naming;

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

fn push_note_unique(notes: &mut Vec<String>, note: String) {
    let nt = note.trim();
    if nt.is_empty() {
        return;
    }
    if !notes.iter().any(|n| n.trim() == nt) {
        notes.push(note);
    }
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
    pub notes: Vec<String>,
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
    pub notes: Vec<String>,
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
        let Some((_cat, schema, table)) = parse_dataset_id_3(&t.dataset_id) else { continue };
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
        let folder = if t.folder.trim().is_empty() { "marts" } else { t.folder.trim() };
        t.expected_model_path = Some(format!("models/{}/{}.sql", folder, t.name.trim()));
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

pub fn cleanse_next_batch(plan: &CleansePlan) -> Vec<String> {
    for batch in plan.batches.iter() {
        let mut out: Vec<String> = Vec::new();
        for ds in batch.iter() {
            if let Some(t) = plan.tasks.iter().find(|t| t.dataset_id == *ds) {
                if t.status != TaskStatus::Done {
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
                if t.status != TaskStatus::Done {
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

pub fn cleanse_mark_done(plan: &mut CleansePlan, dataset_id: &str) {
    if let Some(t) = plan.tasks.iter_mut().find(|t| t.dataset_id == dataset_id) {
        t.status = TaskStatus::Done;
    }
}

pub fn model_mark_done(plan: &mut ModelPlan, name: &str) {
    if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == name) {
        t.status = TaskStatus::Done;
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
        // Deterministic batch executor (preferred): derive progress from its structured output.
        if let ThreadStep::Tool { name, args: _args, observation, .. } = step {
            if name == "apply_next_cleanse_batch" {
                let ok = observation.ok;
                let attempted: Vec<String> = observation
                    .extra
                    .get("attempted_dataset_ids")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
                    .unwrap_or_default();
                let succeeded: Vec<String> = observation
                    .extra
                    .get("succeeded_dataset_ids")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
                    .unwrap_or_default();
                let succ_set: std::collections::HashSet<String> = succeeded.iter().cloned().collect();
                let failed: Vec<String> = attempted
                    .iter()
                    .filter(|ds| !succ_set.contains(*ds))
                    .cloned()
                    .collect();

                for ds in attempted.iter() {
                    if let Some(t) = plan.tasks.iter_mut().find(|t| t.dataset_id == *ds) {
                        mark_task_in_progress(&mut t.status);
                    }
                }
                for ds in succeeded.iter() {
                    cleanse_mark_done(plan, ds);
                }
                if !ok || !failed.is_empty() {
                    let err = observation
                        .errors
                        .first()
                        .map(|s| s.as_str())
                        .unwrap_or("apply_next_cleanse_batch failed");
                    let note = format!("apply_next_cleanse_batch failed: {}", err.trim());
                    for ds in failed.iter() {
                        if let Some(t) = plan.tasks.iter_mut().find(|t| t.dataset_id == *ds) {
                            mark_task_needs_update(&mut t.status);
                            push_note_unique(&mut t.notes, note.clone());
                        }
                    }
                }
                plan.progress.last_applied_step_idx = idx + 1;
                continue;
            }
        }

        // Track BOTH success and failure so plans remain truthful and can drive remediation.
        // IMPORTANT: do NOT `continue` for non-staging tools here; later handlers (dbt_validate, dbt_files)
        // need to observe those tool steps too.
        if let ThreadStep::Tool { name, args, observation, .. } = step {
            if name != "staging_model" {
                // not handled here
            } else {
            let ok = observation.ok;
            let args_dataset_ids: Vec<String> = args
                .get("dataset_ids")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
                .unwrap_or_default();
            let errs: Vec<String> = observation.errors.clone();

            let succeeded: Vec<String> = observation
                .extra
                .get("succeeded_dataset_ids")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
                .unwrap_or_default();

            // Work signal: the suite is actively (re)working these tasks.
            for ds in args_dataset_ids.iter() {
                if let Some(t) = plan.tasks.iter_mut().find(|t| t.dataset_id == *ds) {
                    mark_task_in_progress(&mut t.status);
                }
            }

            // Success path: mark done.
            for ds in succeeded.iter() {
                cleanse_mark_done(plan, ds);
            }

            // Failure path: attach notes and flag tasks as needs_update (idempotent per step index).
            if !ok {
                let mut msg = String::new();
                msg.push_str("staging_model failed");
                if !errs.is_empty() {
                    msg.push_str(": ");
                    msg.push_str(errs[0].trim());
                }
                if !msg.trim().is_empty() {
                    for ds in args_dataset_ids.iter() {
                        if let Some(t) = plan.tasks.iter_mut().find(|t| t.dataset_id == *ds) {
                            // If the tool reported this dataset as succeeded, do not regress it.
                            if !succeeded.contains(ds) {
                                mark_task_needs_update(&mut t.status);
                            }
                            push_note_unique(&mut t.notes, msg.clone());
                        }
                    }
                }
            }

            plan.progress.last_applied_step_idx = idx + 1;
            continue;
            }
        }

        // dbt_validate failures: mark affected staging tasks as needs_update.
        if let ThreadStep::Tool { name, args: _args, observation, .. } = step {
            if name == "dbt_validate" {
                let ok = observation.ok;
                if !ok {
                    let err = observation.errors.first().map(|s| s.as_str()).unwrap_or("dbt_validate failed");
                    let note = format!("dbt_validate failed: {}", err.trim());
                    let logs = observation.extra.get("logs").cloned().unwrap_or(Value::Null);

                    // Extract failing models + runtime test failures from dbt stdout.
                    let failed = crate::data_engineer::dbt_error::extract_failed_models_from_logs(&logs);
                    let runtime = crate::data_engineer::dbt_error::extract_runtime_failures_from_logs(&logs);

                    let mut stems: Vec<String> = Vec::new();
                    for f in failed.iter() {
                        if let Some(file) = f.get("file").and_then(|v| v.as_str()) {
                            if let Some(st) = file_stem(file) {
                                stems.push(st);
                            }
                        }
                        if let Some(name) = f.get("name").and_then(|v| v.as_str()) {
                            if !name.trim().is_empty() {
                                stems.push(name.trim().to_string());
                            }
                        }
                    }
                    for r in runtime.iter() {
                        if let Some(mh) = r.get("model_hint").and_then(|v| v.as_str()) {
                            if !mh.trim().is_empty() {
                                stems.push(mh.trim().to_string());
                            }
                        }
                    }
                    stems.sort();
                    stems.dedup();

                    for t in plan.tasks.iter_mut() {
                        let Some(p) = t.expected_model_path.as_deref() else { continue };
                        let Some(st) = file_stem(p) else { continue };
                        if stems.iter().any(|s| s == &st) {
                            mark_task_needs_update(&mut t.status);
                            push_note_unique(&mut t.notes, note.clone());
                        }
                    }
                }
                plan.progress.last_applied_step_idx = idx + 1;
                continue;
            }
        }

        // Also capture dbt_files patch failures (repair steps) and attach them to the best matching task.
        if let ThreadStep::Tool { name, args, observation, .. } = step {
            if name == "dbt_files" {
                let op = args.get("op").and_then(|v| v.as_str()).unwrap_or("");
                if op == "patch" {
                    let ok = observation.ok;
                    let paths = extract_dbt_files_patch_paths(args);
                    let mut stems: Vec<String> = paths.iter().filter_map(|p| file_stem(p)).collect();
                    stems.sort();
                    stems.dedup();
                    // Work signal: any targeted model is being actively remediated.
                    let mut matched_any = false;
                    for t in plan.tasks.iter_mut() {
                        let Some(p) = t.expected_model_path.as_deref() else { continue };
                        let Some(st) = file_stem(p) else { continue };
                        if stems.iter().any(|s| s == &st) {
                            t.status = TaskStatus::InProgress;
                            matched_any = true;
                        }
                    }
                    if ok && matched_any {
                        // Successful, non-preview mutation indicates forward progress; reset batch-failure budget.
                        let preview = args.get("preview_diff").and_then(|v| v.as_bool()).unwrap_or(false);
                        // NOTE: keep this intentionally simple: any successful non-preview patch
                        // targeting a known task is treated as forward progress (even if it was a no-op write).
                        if !preview {
                            plan.progress.consecutive_batch_failures = 0;
                        }
                    }
                    if !ok {
                        let err = observation.errors.first().map(|s| s.as_str()).unwrap_or("unknown error");
                        let note = format!("dbt_files patch failed: {}", err.trim());
                        for t in plan.tasks.iter_mut() {
                            let Some(p) = t.expected_model_path.as_deref() else { continue };
                            let Some(st) = file_stem(p) else { continue };
                            if stems.iter().any(|s| s == &st) {
                                mark_task_needs_update(&mut t.status);
                                push_note_unique(&mut t.notes, note.clone());
                            }
                        }
                    }
                    plan.progress.last_applied_step_idx = idx + 1;
                }
                continue;
            }
        }
    }
}

pub fn update_model_progress_from_log(plan: &mut ModelPlan, log: &ThreadLog) {
    let start = plan.progress.last_applied_step_idx.min(log.steps.len());
    for (idx, step) in log.steps.iter().enumerate().skip(start) {
        // Deterministic batch executor (preferred): derive progress from its structured output.
        if let ThreadStep::Tool { name, args: _args, observation, .. } = step {
            if name == "apply_next_model_batch" {
                let ok = observation.ok;
                let attempted: Vec<String> = observation
                    .extra
                    .get("attempted_item_names")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
                    .unwrap_or_default();
                let succeeded: Vec<String> = observation
                    .extra
                    .get("succeeded_item_names")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
                    .unwrap_or_default();
                let succ_set: std::collections::HashSet<String> = succeeded.iter().cloned().collect();
                let failed: Vec<String> = attempted
                    .iter()
                    .filter(|n| !succ_set.contains(*n))
                    .cloned()
                    .collect();

                for n in attempted.iter() {
                    if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == *n) {
                        mark_task_in_progress(&mut t.status);
                    }
                }
                for n in succeeded.iter() {
                    model_mark_done(plan, n);
                }
                if !ok || !failed.is_empty() {
                    let err = observation
                        .errors
                        .first()
                        .map(|s| s.as_str())
                        .unwrap_or("apply_next_model_batch failed");
                    let note = format!("apply_next_model_batch failed: {}", err.trim());
                    for n in failed.iter() {
                        if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == *n) {
                            mark_task_needs_update(&mut t.status);
                            push_note_unique(&mut t.notes, note.clone());
                        }
                    }
                }
                plan.progress.last_applied_step_idx = idx + 1;
                continue;
            }
        }

        if let ThreadStep::Tool { name, args, observation, .. } = step {
            if name != "gold_model" {
                // fall through
            } else {
            let ok = observation.ok;
            let succeeded: Vec<String> = observation
                .extra
                .get("succeeded_item_names")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
                .unwrap_or_default();

            // Work signal: any referenced item names are being actively (re)worked.
            let arg_names: Vec<String> = args
                .get("items")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|it| it.get("name").and_then(|n| n.as_str()).map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            for name in arg_names.iter() {
                if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == *name) {
                    mark_task_in_progress(&mut t.status);
                }
            }

            for n in succeeded.iter() {
                model_mark_done(plan, n);
            }
            if !ok {
                let err = observation.errors.first().map(|s| s.as_str()).unwrap_or("unknown error");
                let note = format!("gold_model failed: {}", err.trim());
                for name in arg_names.iter() {
                    if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == *name) {
                        // Don't regress tasks already marked done in this same step's success list.
                        if !succeeded.contains(name) {
                            mark_task_needs_update(&mut t.status);
                        }
                        push_note_unique(&mut t.notes, note.clone());
                    }
                }
            }
            plan.progress.last_applied_step_idx = idx + 1;
            continue;
            }
        }

        // dbt_validate failures: mark affected gold tasks as needs_update (best-effort).
        if let ThreadStep::Tool { name, args: _args, observation, .. } = step {
            if name == "dbt_validate" {
                let ok = observation.ok;
                if !ok {
                    let err = observation.errors.first().map(|s| s.as_str()).unwrap_or("dbt_validate failed");
                    let note = format!("dbt_validate failed: {}", err.trim());
                    let logs = observation.extra.get("logs").cloned().unwrap_or(Value::Null);
                    let failed = crate::data_engineer::dbt_error::extract_failed_models_from_logs(&logs);
                    let runtime = crate::data_engineer::dbt_error::extract_runtime_failures_from_logs(&logs);

                    let mut names: Vec<String> = Vec::new();
                    for f in failed.iter() {
                        if let Some(file) = f.get("file").and_then(|v| v.as_str()) {
                            if let Some(st) = file_stem(file) {
                                names.push(st);
                            }
                        }
                        if let Some(name) = f.get("name").and_then(|v| v.as_str()) {
                            if !name.trim().is_empty() {
                                names.push(name.trim().to_string());
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
                            mark_task_needs_update(&mut t.status);
                            push_note_unique(&mut t.notes, note.clone());
                        }
                    }
                }
                plan.progress.last_applied_step_idx = idx + 1;
                continue;
            }
        }

        // Capture dbt_files patch failures during gold repairs too.
        if let ThreadStep::Tool { name, args, observation, .. } = step {
            if name == "dbt_files" {
            let op = args.get("op").and_then(|v| v.as_str()).unwrap_or("");
            if op == "patch" {
                let ok = observation.ok;
                let paths = extract_dbt_files_patch_paths(args);
                let mut stems: Vec<String> = paths.iter().filter_map(|p| file_stem(p)).collect();
                stems.sort();
                stems.dedup();
                // Work signal: patching a gold model implies active remediation of that model task.
                let mut matched_any = false;
                for t in plan.tasks.iter_mut() {
                    if stems.iter().any(|s| s == &t.name) {
                        t.status = TaskStatus::InProgress;
                        matched_any = true;
                    }
                }
                if ok && matched_any {
                    plan.progress.consecutive_batch_failures = 0;
                }
                if !ok {
                    let err = observation.errors.first().map(|s| s.as_str()).unwrap_or("unknown error");
                    let note = format!("dbt_files patch failed: {}", err.trim());
                    for t in plan.tasks.iter_mut() {
                        if stems.iter().any(|s| s == &t.name) {
                            mark_task_needs_update(&mut t.status);
                            push_note_unique(&mut t.notes, note.clone());
                        }
                    }
                }
                plan.progress.last_applied_step_idx = idx + 1;
            }
            continue;
            }
        }
    }
}

pub fn summarize_cleanse_plan(plan: &CleansePlan, max_lines: usize) -> String {
    let total = plan.tasks.len();
    let done = plan.tasks.iter().filter(|t| t.status == TaskStatus::Done).count();
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("Cleanse plan: status={:?} ({done}/{total} done)", plan.status));
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
    let done = plan.tasks.iter().filter(|t| t.status == TaskStatus::Done).count();
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("Model plan: status={:?} ({done}/{total} done)", plan.status));
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
    // Preferred: the entire answer is JSON.
    if let Ok(v) = serde_json::from_str::<Value>(answer.trim()) {
        return Some(v);
    }
    // Backstop: accept the historical first-line prefix format: PLAN:<json>
    let first = answer.lines().next()?.trim();
    let prefix = "PLAN:";
    if first.starts_with(prefix) {
        let json_text = first[prefix.len()..].trim();
        if let Ok(v) = serde_json::from_str::<Value>(json_text) {
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use react_core::session::{ThreadLog, ThreadStep, ToolObservation};

    fn step(action: &str, args: Value, observation: Value) -> ThreadStep {
        ThreadStep::Tool {
            name: action.to_string(),
            args,
            observation: ToolObservation::normalize(observation),
            ts: "t".to_string(),
            agent: "test".to_string(),
        }
    }

    #[test]
    fn cleanse_progress_marks_done_from_succeeded_ids_and_completes() {
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
                    notes: vec![],
                },
                CleanseTask {
                    dataset_id: "d.e.f".to_string(),
                    expected_model_path: None,
                    invariants: vec![],
                    status: TaskStatus::Pending,
                    notes: vec![],
                },
            ],
            batches: vec![vec!["a.b.c".to_string()], vec!["d.e.f".to_string()]],
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
        assert!(cleanse_all_done(&plan));
        // Note: being "done" is tracked at the task level. PlanStatus::Completed is reserved for
        // "validate passed", and is set by the suite after dbt_validate succeeds.
        assert_eq!(plan.status, PlanStatus::Approved);
    }

    #[test]
    fn model_progress_marks_done_from_succeeded_names_and_completes() {
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
                    notes: vec![],
                },
                ModelTask {
                    name: "dim_b".to_string(),
                    folder: "core".to_string(),
                    goal: "b".to_string(),
                    inputs: vec!["stg_y".to_string()],
                    expected_model_path: None,
                    invariants: vec![],
                    status: TaskStatus::Pending,
                    notes: vec![],
                },
            ],
            batches: vec![vec!["fct_a".to_string(), "dim_b".to_string()]],
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
        assert!(model_all_done(&plan));
        // Note: PlanStatus::Completed is set only after dbt_validate passes.
        assert_eq!(plan.status, PlanStatus::Approved);
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
                notes: vec![],
            }],
            batches: vec![vec!["AwsDataCatalog.test_raw.raw_customers".to_string()]],
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
                expected_model_path: Some("models/staging/stg_test_raw_raw_customers.sql".to_string()),
                invariants: vec![],
                status: TaskStatus::NeedsUpdate,
                notes: vec![],
            }],
            batches: vec![vec!["AwsDataCatalog.test_raw.raw_customers".to_string()]],
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
}

