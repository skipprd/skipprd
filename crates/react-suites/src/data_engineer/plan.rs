use serde::{Deserialize, Serialize};
use serde_json::Value;

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
}

impl Default for TaskStatus {
    fn default() -> Self {
        TaskStatus::Pending
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlanProgress {
    /// Thread step index cursor: when updating from the thread log, only consider steps after this
    /// index (exclusive). This keeps progress updates idempotent and cheap.
    #[serde(default)]
    pub last_applied_step_idx: usize,
}

impl Default for PlanProgress {
    fn default() -> Self {
        Self {
            last_applied_step_idx: 0,
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
        // Track BOTH success and failure so plans remain truthful and can drive remediation.
        if let ThreadStep::Tool { name, args, observation, .. } = step {
            if name != "staging_model" {
                continue;
            }
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

            // Success path: mark done.
            for ds in succeeded.iter() {
                cleanse_mark_done(plan, ds);
            }

            // Failure path: mark tasks in-progress and attach notes (idempotent per step index).
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
                                t.status = TaskStatus::Blocked;
                            }
                            if !t.notes.iter().any(|n| n.trim() == msg.trim()) {
                                t.notes.push(msg.clone());
                            }
                        }
                    }
                }
            }

            plan.progress.last_applied_step_idx = idx + 1;
            continue;
        }

        // Also capture dbt_files patch failures (repair steps) and attach them to the best matching task.
        if let ThreadStep::Tool { name, args, observation, .. } = step {
            if name == "dbt_files" {
                let op = args.get("op").and_then(|v| v.as_str()).unwrap_or("");
                if op == "patch" {
                    let ok = observation.ok;
                    if !ok {
                        let err = observation.errors.first().map(|s| s.as_str()).unwrap_or("unknown error");
                        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                        let stem = std::path::Path::new(path)
                            .file_stem()
                            .map(|s| s.to_string_lossy().to_string());
                        if let Some(stem) = stem {
                            if let Some(t) = plan.tasks.iter_mut().find(|t| {
                                t.expected_model_path
                                    .as_deref()
                                    .and_then(|p| std::path::Path::new(p).file_stem().map(|s| s.to_string_lossy().to_string()))
                                    .map(|s| s == stem)
                                    .unwrap_or(false)
                            }) {
                                t.status = TaskStatus::Blocked;
                                let note = format!("dbt_files patch failed: {}", err.trim());
                                if !t.notes.iter().any(|n| n.trim() == note.trim()) {
                                    t.notes.push(note);
                                }
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
            for n in succeeded.iter() {
                model_mark_done(plan, n);
            }
            if !ok {
                let err = observation.errors.first().map(|s| s.as_str()).unwrap_or("unknown error");
                // Best-effort: mark all referenced item names in args as in-progress.
                let arg_names: Vec<String> = args
                    .get("items")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|it| it.get("name").and_then(|n| n.as_str()).map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                let note = format!("gold_model failed: {}", err.trim());
                for name in arg_names.iter() {
                    if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == *name) {
                        // Don't regress tasks already marked done in this same step's success list.
                        if !succeeded.contains(name) {
                            t.status = TaskStatus::Blocked;
                        }
                        if !t.notes.iter().any(|n| n.trim() == note.trim()) {
                            t.notes.push(note.clone());
                        }
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
                if !ok {
                    let err = observation.errors.first().map(|s| s.as_str()).unwrap_or("unknown error");
                    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                    let stem = std::path::Path::new(path).file_stem().map(|s| s.to_string_lossy().to_string());
                    if let Some(stem) = stem {
                        if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == stem) {
                            t.status = TaskStatus::Blocked;
                            let note = format!("dbt_files patch failed: {}", err.trim());
                            if !t.notes.iter().any(|n| n.trim() == note.trim()) {
                                t.notes.push(note);
                            }
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
}

