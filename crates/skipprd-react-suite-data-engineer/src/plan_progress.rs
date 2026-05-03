#[cfg(test)]
use crate::plan_grounding::{
    ensure_expected_model_paths_cleanse, prune_cleanse_plan_to_grounded_raw_datasets,
    prune_model_plan_to_grounded_staging_models,
};
use crate::plan_types::*;
#[cfg(test)]
use crate::plan_validation::{validate_cleanse_plan_semantics, validate_model_plan_semantics};
use crate::track_spec::TrackKind;
#[cfg(test)]
use react_core::session::{ThreadLog, ThreadStep};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use serde_json::Value;

pub const MAX_BATCH_SIZE: usize = 5;

#[cfg(test)]
fn parse_batch_contract<T: serde::de::DeserializeOwned>(
    extra: &std::collections::BTreeMap<String, Value>,
    contract_name: &str,
) -> T {
    serde_json::from_value(
        serde_json::to_value(extra).expect("serialize tool observation contract"),
    )
    .unwrap_or_else(|e| panic!("{contract_name}: {e}"))
}

pub const CHECKLIST_SQL_MODEL: &str = "sql_model";
pub const CHECKLIST_SCHEMA_CONTRACT: &str = "schema_contract";
pub const CHECKLIST_VALIDATE: &str = "validate";

pub fn schema_contract_checklist_item_id() -> &'static str {
    CHECKLIST_SCHEMA_CONTRACT
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "plan_kind", rename_all = "snake_case")]
pub enum PlanPendingRef {
    Cleanse {
        dataset_id: String,
        checklist_item_id: String,
    },
    Model {
        item_name: String,
        checklist_item_id: String,
    },
    Generic {
        task_id: String,
        checklist_item_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanCompletionSnapshot {
    pub all_done: bool,
    pub pending_count: usize,
    pub pending_refs: Vec<PlanPendingRef>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanCompletionState {
    Complete,
    Incomplete,
}

impl PlanCompletionSnapshot {
    pub fn completion_state(&self) -> PlanCompletionState {
        if self.all_done {
            PlanCompletionState::Complete
        } else {
            PlanCompletionState::Incomplete
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlanProgressEvent {
    CleanseSqlInProgress { dataset_id: String },
    CleanseSqlDone { dataset_id: String },
    CleanseSqlNeedsUpdate { dataset_id: String },
    CleanseSchemaInProgress { dataset_id: String },
    CleanseSchemaDone { dataset_id: String },
    CleanseSchemaNeedsUpdate { dataset_id: String },
    CleanseValidateDone,
    ModelSqlInProgress { item_name: String },
    ModelSqlDone { item_name: String },
    ModelSqlNeedsUpdate { item_name: String },
    ModelSchemaInProgress { item_name: String },
    ModelSchemaDone { item_name: String },
    ModelSchemaNeedsUpdate { item_name: String },
    ModelValidateDone,
}

pub(crate) fn required_checklist_item_ids() -> [&'static str; 3] {
    [
        CHECKLIST_SQL_MODEL,
        CHECKLIST_SCHEMA_CONTRACT,
        CHECKLIST_VALIDATE,
    ]
}

fn checklist_label_for_kind(track: TrackKind, checklist_item_id: &str) -> &'static str {
    match checklist_item_id {
        CHECKLIST_SQL_MODEL if track.is_cleanse() => "Author staging SQL",
        CHECKLIST_SQL_MODEL => "Author gold SQL",
        CHECKLIST_SCHEMA_CONTRACT => "Author schema contract",
        CHECKLIST_VALIDATE => "Validate",
        _ => "Checklist item",
    }
}

pub fn canonical_task_checklist(track: TrackKind) -> Vec<PlanChecklistItem> {
    required_checklist_item_ids()
        .into_iter()
        .map(|id| PlanChecklistItem {
            checklist_item_id: id.to_string(),
            label: checklist_label_for_kind(track, id).to_string(),
            details: None,
            status: ChecklistItemStatus::Pending,
            origin: ChecklistOrigin::Initial,
            evidence: vec![],
        })
        .collect()
}

pub(crate) fn missing_required_checklist_items(items: &[PlanChecklistItem]) -> Vec<String> {
    required_checklist_item_ids()
        .into_iter()
        .filter(|id| !items.iter().any(|it| it.checklist_item_id.trim() == *id))
        .map(|id| id.to_string())
        .collect()
}

pub(crate) fn work_groups_cover_task_checklist(
    work_groups: &[PlanWorkGroup],
    task_id: &str,
    checklist_item_id: &str,
) -> bool {
    work_groups.iter().any(|g| {
        g.items.iter().any(|it| {
            it.task_id.trim() == task_id.trim()
                && it.checklist_item_id.trim() == checklist_item_id.trim()
        })
    })
}

pub(crate) fn checklist_status(items: &[PlanChecklistItem], id: &str) -> ChecklistItemStatus {
    items
        .iter()
        .find(|it| it.checklist_item_id == id)
        .map(|it| it.status)
        .unwrap_or(ChecklistItemStatus::Pending)
}

pub fn is_runnable_checklist_status(s: ChecklistItemStatus) -> bool {
    matches!(
        s,
        ChecklistItemStatus::Pending
            | ChecklistItemStatus::InProgress
            | ChecklistItemStatus::NeedsUpdate
    )
}

pub fn canonical_work_groups_from_batches(
    batches: &[Vec<String>],
    item_prefix: &str,
) -> Vec<PlanWorkGroup> {
    let mut out: Vec<PlanWorkGroup> = Vec::new();
    let mut schema_group_ids: Vec<String> = Vec::new();
    for (idx, b) in batches.iter().enumerate() {
        let mut item_ids: Vec<String> = Vec::new();
        for item in b.iter() {
            let id = item.trim();
            if id.is_empty() {
                continue;
            }
            if !item_ids.iter().any(|x| x == id) {
                item_ids.push(id.to_string());
            }
        }
        if item_ids.is_empty() {
            continue;
        }
        let ord = idx + 1;
        let sql_group_id = format!("{item_prefix}_author_sql_{ord:03}");
        let schema_group_id = format!("{item_prefix}_author_schema_{ord:03}");
        out.push(PlanWorkGroup {
            group_id: sql_group_id.clone(),
            label: format!("Author SQL batch {}", ord),
            kind: WorkGroupKind::AuthorSql,
            items: item_ids
                .iter()
                .map(|task_id| WorkGroupItemRef {
                    task_id: task_id.clone(),
                    checklist_item_id: CHECKLIST_SQL_MODEL.to_string(),
                })
                .collect(),
            depends_on_group_ids: None,
        });
        out.push(PlanWorkGroup {
            group_id: schema_group_id.clone(),
            label: format!("Author schema batch {}", ord),
            kind: WorkGroupKind::AuthorSchema,
            items: item_ids
                .iter()
                .map(|task_id| WorkGroupItemRef {
                    task_id: task_id.clone(),
                    checklist_item_id: CHECKLIST_SCHEMA_CONTRACT.to_string(),
                })
                .collect(),
            depends_on_group_ids: Some(vec![sql_group_id]),
        });
        schema_group_ids.push(schema_group_id);
    }
    if !schema_group_ids.is_empty() {
        for (idx, b) in batches.iter().enumerate() {
            let mut validate_items: Vec<WorkGroupItemRef> = Vec::new();
            for item in b.iter() {
                let id = item.trim();
                if id.is_empty() {
                    continue;
                }
                if !validate_items.iter().any(|it| it.task_id == id) {
                    validate_items.push(WorkGroupItemRef {
                        task_id: id.to_string(),
                        checklist_item_id: CHECKLIST_VALIDATE.to_string(),
                    });
                }
            }
            if !validate_items.is_empty() {
                let ord = idx + 1;
                out.push(PlanWorkGroup {
                    group_id: format!("{item_prefix}_validate_{ord:03}"),
                    label: format!("Validate batch {}", ord),
                    kind: WorkGroupKind::Validate,
                    items: validate_items,
                    depends_on_group_ids: Some(schema_group_ids.clone()),
                });
            }
        }
    }
    out
}

pub fn canonical_sequential_work_groups_from_batches(
    batches: &[Vec<String>],
    item_prefix: &str,
) -> Vec<PlanWorkGroup> {
    let mut out: Vec<PlanWorkGroup> = Vec::new();
    let mut schema_group_ids: Vec<String> = Vec::new();
    let mut previous_schema_group_id: Option<String> = None;
    for (idx, b) in batches.iter().enumerate() {
        let mut item_ids: Vec<String> = Vec::new();
        for item in b.iter() {
            let id = item.trim();
            if id.is_empty() {
                continue;
            }
            if !item_ids.iter().any(|x| x == id) {
                item_ids.push(id.to_string());
            }
        }
        if item_ids.is_empty() {
            continue;
        }
        let ord = idx + 1;
        let sql_group_id = format!("{item_prefix}_author_sql_{ord:03}");
        let schema_group_id = format!("{item_prefix}_author_schema_{ord:03}");
        out.push(PlanWorkGroup {
            group_id: sql_group_id.clone(),
            label: format!("Author SQL batch {}", ord),
            kind: WorkGroupKind::AuthorSql,
            items: item_ids
                .iter()
                .map(|task_id| WorkGroupItemRef {
                    task_id: task_id.clone(),
                    checklist_item_id: CHECKLIST_SQL_MODEL.to_string(),
                })
                .collect(),
            depends_on_group_ids: previous_schema_group_id.clone().map(|id| vec![id]),
        });
        out.push(PlanWorkGroup {
            group_id: schema_group_id.clone(),
            label: format!("Author schema batch {}", ord),
            kind: WorkGroupKind::AuthorSchema,
            items: item_ids
                .iter()
                .map(|task_id| WorkGroupItemRef {
                    task_id: task_id.clone(),
                    checklist_item_id: CHECKLIST_SCHEMA_CONTRACT.to_string(),
                })
                .collect(),
            depends_on_group_ids: Some(vec![sql_group_id]),
        });
        previous_schema_group_id = Some(schema_group_id.clone());
        schema_group_ids.push(schema_group_id);
    }
    if !schema_group_ids.is_empty() {
        for (idx, b) in batches.iter().enumerate() {
            let mut validate_items: Vec<WorkGroupItemRef> = Vec::new();
            for item in b.iter() {
                let id = item.trim();
                if id.is_empty() {
                    continue;
                }
                if !validate_items.iter().any(|it| it.task_id == id) {
                    validate_items.push(WorkGroupItemRef {
                        task_id: id.to_string(),
                        checklist_item_id: CHECKLIST_VALIDATE.to_string(),
                    });
                }
            }
            if !validate_items.is_empty() {
                let ord = idx + 1;
                out.push(PlanWorkGroup {
                    group_id: format!("{item_prefix}_validate_{ord:03}"),
                    label: format!("Validate batch {}", ord),
                    kind: WorkGroupKind::Validate,
                    items: validate_items,
                    depends_on_group_ids: Some(schema_group_ids.clone()),
                });
            }
        }
    }
    out
}

pub fn canonical_model_batches_from_tasks(tasks: &[ModelTask]) -> Vec<Vec<String>> {
    let mut task_names: Vec<String> = Vec::new();
    for task in tasks.iter() {
        let name = task.name.trim();
        if !name.is_empty() && !task_names.iter().any(|n| n == name) {
            task_names.push(name.to_string());
        }
    }

    let task_name_set: std::collections::BTreeSet<String> = task_names.iter().cloned().collect();
    let mut deps_by_name: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
        std::collections::BTreeMap::new();
    for task in tasks.iter() {
        let name = task.name.trim();
        if name.is_empty() || !task_name_set.contains(name) {
            continue;
        }
        let mut deps = std::collections::BTreeSet::new();
        for input in task.inputs.iter() {
            let dep = input.trim();
            if dep != name && task_name_set.contains(dep) {
                deps.insert(dep.to_string());
            }
        }
        deps_by_name.insert(name.to_string(), deps);
    }

    let mut ordered: Vec<String> = Vec::new();
    let mut done: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    while ordered.len() < task_names.len() {
        let mut progressed = false;
        for name in task_names.iter() {
            if done.contains(name) {
                continue;
            }
            let deps = deps_by_name.get(name).cloned().unwrap_or_default();
            if deps.iter().all(|dep| done.contains(dep)) {
                done.insert(name.clone());
                ordered.push(name.clone());
                progressed = true;
            }
        }
        if !progressed {
            // Cycles are semantically invalid, but keep the persisted plan executable enough
            // for validation/repair to report the real issue instead of dropping tasks.
            for name in task_names.iter() {
                if !done.contains(name) {
                    done.insert(name.clone());
                    ordered.push(name.clone());
                }
            }
        }
    }

    ordered.into_iter().map(|name| vec![name]).collect()
}

#[cfg(test)]
fn file_stem(s: &str) -> Option<String> {
    std::path::Path::new(s)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.trim().is_empty())
}

#[cfg(test)]
fn extract_file_paths(op: &str, args: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    match op {
        "patch" => {
            if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
                let p = p.trim();
                if !p.is_empty() {
                    out.push(p.to_string());
                }
            }
        }
        "mv" => {
            if let Some(p) = args.get("to").and_then(|v| v.as_str()) {
                let p = p.trim();
                if !p.is_empty() {
                    out.push(p.to_string());
                }
            }
        }
        "rm" => {
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

// Plan storage I/O is in plan_storage.rs; re-exported via plan.rs.

#[cfg(test)]
fn next_sql_batch_from_plan<T>(
    batches: &[Vec<String>],
    tasks: &[T],
    task_id: impl Fn(&T) -> &str,
    checklist: impl Fn(&T) -> &[PlanChecklistItem],
) -> Vec<String> {
    for batch in batches.iter() {
        let mut out: Vec<String> = Vec::new();
        for item in batch.iter() {
            if let Some(task) = tasks.iter().find(|t| task_id(t) == item) {
                // "Next batch" is defined as "next SQL authoring work", not "overall task not done".
                // This avoids repeatedly scheduling a task when only schema/validate checklist items remain.
                let st = checklist_status(checklist(task), CHECKLIST_SQL_MODEL);
                if st != ChecklistItemStatus::Done && is_runnable_checklist_status(st) {
                    out.push(item.clone());
                }
            } else {
                // If the plan batches reference a task not present in tasks, still allow it.
                out.push(item.clone());
            }
            if out.len() >= MAX_BATCH_SIZE {
                break;
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    vec![]
}

#[cfg(test)]
pub fn next_batch<T: PlanTask>(plan: &Plan<T>) -> Vec<String> {
    next_sql_batch_from_plan(
        &plan.batches,
        &plan.tasks,
        |t| t.task_id(),
        |t| t.checklist(),
    )
}

#[cfg(test)]
pub fn cleanse_next_batch(plan: &CleansePlan) -> Vec<String> {
    next_batch(plan)
}

#[cfg(test)]
pub fn model_next_batch(plan: &ModelPlan) -> Vec<String> {
    next_batch(plan)
}

fn _executable_plan_issues<T>(
    tasks: &[T],
    batches: &[Vec<String>],
    work_groups: &[PlanWorkGroup],
    task_id: impl Fn(&T) -> &str,
    checklist: impl Fn(&T) -> &[PlanChecklistItem],
    all_done: bool,
    next_action: AuthoringNextAction,
) -> Vec<String> {
    let mut issues: Vec<String> = Vec::new();
    if tasks.is_empty() {
        issues.push("tasks is empty".to_string());
    }
    if batches.is_empty() {
        issues.push("batches is empty".to_string());
    }
    if work_groups.is_empty() {
        issues.push("work_groups is empty".to_string());
    }
    for task in tasks.iter() {
        let task_key = task_id(task).to_string();
        let missing = missing_required_checklist_items(checklist(task));
        if !missing.is_empty() {
            issues.push(format!(
                "task {} missing checklist items: {}",
                task_key,
                missing.join(", ")
            ));
        }
        for checklist_id in required_checklist_item_ids() {
            if !work_groups_cover_task_checklist(work_groups, &task_key, checklist_id) {
                issues.push(format!(
                    "task {} checklist '{}' missing work-group scheduling",
                    task_key, checklist_id
                ));
            }
        }
    }
    if !all_done && matches!(next_action, AuthoringNextAction::None) {
        issues.push("plan has pending checklist work but no actionable work-group".to_string());
    }
    issues
}

pub fn executable_plan_issues<T: PlanTask>(plan: &Plan<T>) -> Vec<String> {
    _executable_plan_issues(
        &plan.tasks,
        &plan.batches,
        &plan.work_groups,
        |t| t.task_id(),
        |t| t.checklist(),
        all_done(plan),
        next_authoring_action(plan),
    )
}

#[cfg(test)]
pub fn cleanse_executable_plan_issues(plan: &CleansePlan) -> Vec<String> {
    executable_plan_issues(plan)
}

#[cfg(test)]
pub fn model_executable_plan_issues(plan: &ModelPlan) -> Vec<String> {
    executable_plan_issues(plan)
}

fn group_is_complete<T: PlanTask>(plan: &Plan<T>, g: &PlanWorkGroup) -> bool {
    if g.items.is_empty() {
        return true;
    }
    for it in g.items.iter() {
        let Some(t) = plan.tasks.iter().find(|t| t.task_id() == it.task_id) else {
            return false;
        };
        if checklist_status(t.checklist(), it.checklist_item_id.as_str())
            != ChecklistItemStatus::Done
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

fn next_action_from_work_groups(
    work_groups: &[PlanWorkGroup],
    completed: &std::collections::HashSet<String>,
    mut item_needs_work: impl FnMut(&WorkGroupItemRef) -> bool,
) -> Option<(WorkGroupKind, Vec<String>)> {
    for g in work_groups.iter() {
        if completed.contains(&g.group_id) {
            continue;
        }
        if !group_deps_satisfied(completed, g.depends_on_group_ids.as_ref()) {
            continue;
        }
        if g.kind == WorkGroupKind::Validate {
            return Some((WorkGroupKind::Validate, vec![]));
        }

        let mut out: Vec<String> = Vec::new();
        for it in g.items.iter() {
            if item_needs_work(it) {
                if !out.contains(&it.task_id) {
                    out.push(it.task_id.clone());
                }
                if out.len() >= MAX_BATCH_SIZE {
                    break;
                }
            }
        }
        return Some((g.kind, out));
    }
    Some((WorkGroupKind::Validate, vec![]))
}

fn next_work_item_ctx_from_work_groups(
    work_groups: &[PlanWorkGroup],
    completed: &std::collections::HashSet<String>,
    mut item_needs_work: impl FnMut(&WorkGroupItemRef) -> bool,
) -> Option<NextWorkItemCtx> {
    for g in work_groups.iter() {
        if completed.contains(&g.group_id) {
            continue;
        }
        if !group_deps_satisfied(completed, g.depends_on_group_ids.as_ref()) {
            continue;
        }
        if g.kind == WorkGroupKind::Validate {
            return None;
        }
        for it in g.items.iter() {
            if item_needs_work(it) {
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

fn next_action_from_plan<T: PlanTask>(plan: &Plan<T>) -> Option<(WorkGroupKind, Vec<String>)> {
    if plan.work_groups.is_empty() {
        return None;
    }

    let mut completed: std::collections::HashSet<String> = std::collections::HashSet::new();
    for g in plan.work_groups.iter() {
        if group_is_complete(plan, g) {
            completed.insert(g.group_id.clone());
        }
    }
    next_action_from_work_groups(&plan.work_groups, &completed, |it| {
        match plan.tasks.iter().find(|t| t.task_id() == it.task_id) {
            Some(t) => is_runnable_checklist_status(checklist_status(
                t.checklist(),
                it.checklist_item_id.as_str(),
            )),
            None => true,
        }
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NextWorkItemCtx {
    pub workgroup_id: String,
    #[serde(default)]
    pub workgroup_label: Option<String>,
    pub task_id: String,
    pub checklist_item_id: String,
}

pub fn next_work_item_ctx<T: PlanTask>(plan: &Plan<T>) -> Option<NextWorkItemCtx> {
    if plan.work_groups.is_empty() {
        return None;
    }

    let mut completed: std::collections::HashSet<String> = std::collections::HashSet::new();
    for g in plan.work_groups.iter() {
        if group_is_complete(plan, g) {
            completed.insert(g.group_id.clone());
        }
    }
    next_work_item_ctx_from_work_groups(&plan.work_groups, &completed, |it| {
        match plan.tasks.iter().find(|t| t.task_id() == it.task_id) {
            Some(t) => is_runnable_checklist_status(checklist_status(
                t.checklist(),
                it.checklist_item_id.as_str(),
            )),
            None => true,
        }
    })
}

pub fn cleanse_next_work_item_ctx(plan: &CleansePlan) -> Option<NextWorkItemCtx> {
    next_work_item_ctx(plan)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthoringNextAction {
    AuthorSql(Vec<String>),
    AuthorSchema(Vec<String>),
    Validate,
    None,
}

impl AuthoringNextAction {
    pub fn author_sql_ids(&self) -> Vec<String> {
        match self {
            Self::AuthorSql(ids) => ids.clone(),
            _ => Vec::new(),
        }
    }
}

pub fn next_authoring_action<T: PlanTask>(plan: &Plan<T>) -> AuthoringNextAction {
    match next_action_from_plan(plan) {
        Some((WorkGroupKind::AuthorSql, ids)) => AuthoringNextAction::AuthorSql(ids),
        Some((WorkGroupKind::AuthorSchema, ids)) => AuthoringNextAction::AuthorSchema(ids),
        Some((WorkGroupKind::Validate, _)) => AuthoringNextAction::Validate,
        None => AuthoringNextAction::None,
    }
}

pub fn cleanse_next_authoring_action(plan: &CleansePlan) -> AuthoringNextAction {
    next_authoring_action(plan)
}

pub fn model_next_authoring_action(plan: &ModelPlan) -> AuthoringNextAction {
    next_authoring_action(plan)
}

pub fn model_next_work_item_ctx(plan: &ModelPlan) -> Option<NextWorkItemCtx> {
    next_work_item_ctx(plan)
}

pub fn pending_for_checklist<T: PlanTask>(
    plan: &Plan<T>,
    prereq_checklist_item_id: &str,
    target_checklist_item_id: &str,
) -> Vec<String> {
    let prereq = prereq_checklist_item_id.trim();
    let target = target_checklist_item_id.trim();
    if prereq.is_empty() || target.is_empty() {
        return vec![];
    }
    pending_for_checklist_from_plan(
        &plan.batches,
        &plan.tasks,
        |t| t.task_id(),
        |t| t.checklist(),
        prereq,
        target,
    )
}

pub fn pending_schema_contracts<T: PlanTask>(plan: &Plan<T>) -> Vec<String> {
    pending_for_checklist(plan, CHECKLIST_SQL_MODEL, CHECKLIST_SCHEMA_CONTRACT)
}

pub fn cleanse_pending_schema_contracts(plan: &CleansePlan) -> Vec<String> {
    pending_schema_contracts(plan)
}

#[cfg(test)]
pub fn cleanse_pending_for_checklist(
    plan: &CleansePlan,
    prereq_checklist_item_id: &str,
    target_checklist_item_id: &str,
) -> Vec<String> {
    pending_for_checklist(plan, prereq_checklist_item_id, target_checklist_item_id)
}

pub fn model_pending_schema_contracts(plan: &ModelPlan) -> Vec<String> {
    pending_schema_contracts(plan)
}

#[cfg(test)]
pub fn model_pending_for_checklist(
    plan: &ModelPlan,
    prereq_checklist_item_id: &str,
    target_checklist_item_id: &str,
) -> Vec<String> {
    pending_for_checklist(plan, prereq_checklist_item_id, target_checklist_item_id)
}

fn pending_for_checklist_from_plan<T>(
    batches: &[Vec<String>],
    tasks: &[T],
    task_id: impl Fn(&T) -> &str,
    checklist: impl Fn(&T) -> &[PlanChecklistItem],
    prereq: &str,
    target: &str,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for batch in batches.iter() {
        for item in batch.iter() {
            let Some(task) = tasks.iter().find(|t| task_id(t) == item) else {
                continue;
            };
            if checklist_status(checklist(task), prereq) != ChecklistItemStatus::Done {
                continue;
            }
            let st = checklist_status(checklist(task), target);
            if st != ChecklistItemStatus::Done && is_runnable_checklist_status(st) {
                out.push(item.clone());
                if out.len() >= MAX_BATCH_SIZE {
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
    if items
        .iter()
        .any(|it| it.status == ChecklistItemStatus::Blocked)
    {
        return TaskStatus::Blocked;
    }
    if items
        .iter()
        .all(|it| it.status == ChecklistItemStatus::Done)
    {
        return TaskStatus::Done;
    }
    if items
        .iter()
        .any(|it| it.status == ChecklistItemStatus::InProgress)
    {
        return TaskStatus::InProgress;
    }
    // If some items are complete but others remain pending, surface as in_progress.
    if items
        .iter()
        .any(|it| it.status == ChecklistItemStatus::Done)
    {
        return TaskStatus::InProgress;
    }
    TaskStatus::Pending
}

fn recompute_task_status<T: PlanTask>(t: &mut T) {
    let new_status = task_status_from_checklist(t.checklist());
    t.set_status(new_status);
}

#[cfg(test)]
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

pub fn mark_done<T: PlanTask>(plan: &mut Plan<T>, task_id: &str, label: &str) {
    if let Some(t) = plan.tasks.iter_mut().find(|t| t.task_id() == task_id) {
        let it = ensure_checklist_item(t.checklist_mut(), CHECKLIST_SQL_MODEL, label);
        set_checklist_status(it, ChecklistItemStatus::Done, None);
        recompute_task_status(t);
    }
}

pub fn cleanse_mark_done(plan: &mut CleansePlan, dataset_id: &str) {
    mark_done(plan, dataset_id, "Author staging SQL");
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
        evidence: vec![],
    });
    let last = items.len().saturating_sub(1);
    &mut items[last]
}

pub fn checklist_mark_status<T: PlanTask>(
    plan: &mut Plan<T>,
    task_id: &str,
    checklist_item_id: &str,
    status: ChecklistItemStatus,
) {
    if checklist_item_id.trim().is_empty() {
        return;
    }
    if let Some(t) = plan.tasks.iter_mut().find(|t| t.task_id() == task_id) {
        let it = ensure_checklist_item_any(t.checklist_mut(), checklist_item_id);
        set_checklist_status(it, status, None);
        recompute_task_status(t);
    }
}

pub fn cleanse_checklist_mark_status(
    plan: &mut CleansePlan,
    dataset_id: &str,
    checklist_item_id: &str,
    status: ChecklistItemStatus,
) {
    checklist_mark_status(plan, dataset_id, checklist_item_id, status);
}

pub fn model_mark_done(plan: &mut ModelPlan, name: &str) {
    mark_done(plan, name, "Author gold SQL");
}

pub fn model_checklist_mark_status(
    plan: &mut ModelPlan,
    name: &str,
    checklist_item_id: &str,
    status: ChecklistItemStatus,
) {
    checklist_mark_status(plan, name, checklist_item_id, status);
}

pub fn schema_contract_mark_done<T: PlanTask>(plan: &mut Plan<T>, task_id: &str) {
    if let Some(t) = plan.tasks.iter_mut().find(|t| t.task_id() == task_id) {
        let it = ensure_checklist_item(
            t.checklist_mut(),
            CHECKLIST_SCHEMA_CONTRACT,
            "Author schema contract",
        );
        set_checklist_status(it, ChecklistItemStatus::Done, None);
        recompute_task_status(t);
    }
}

pub fn schema_contract_mark_needs_update<T: PlanTask>(
    plan: &mut Plan<T>,
    task_id: &str,
    reason: Option<&str>,
) {
    if let Some(t) = plan.tasks.iter_mut().find(|t| t.task_id() == task_id) {
        let it = ensure_checklist_item(
            t.checklist_mut(),
            CHECKLIST_SCHEMA_CONTRACT,
            "Author schema contract",
        );
        set_checklist_status(it, ChecklistItemStatus::NeedsUpdate, None);
        if let Some(r) = reason.map(str::trim).filter(|s| !s.is_empty()) {
            it.details = Some(r.to_string());
        }
        recompute_task_status(t);
    }
}

pub fn schema_contract_mark_in_progress<T: PlanTask>(plan: &mut Plan<T>, task_id: &str) {
    if let Some(t) = plan.tasks.iter_mut().find(|t| t.task_id() == task_id) {
        let it = ensure_checklist_item(
            t.checklist_mut(),
            CHECKLIST_SCHEMA_CONTRACT,
            "Author schema contract",
        );
        set_checklist_status(it, ChecklistItemStatus::InProgress, None);
        recompute_task_status(t);
    }
}

pub fn model_schema_contract_mark_done(plan: &mut ModelPlan, name: &str) {
    schema_contract_mark_done(plan, name);
}

pub fn model_schema_contract_mark_needs_update(
    plan: &mut ModelPlan,
    name: &str,
    reason: Option<&str>,
) {
    schema_contract_mark_needs_update(plan, name, reason);
}

pub fn model_schema_contract_mark_in_progress(plan: &mut ModelPlan, name: &str) {
    schema_contract_mark_in_progress(plan, name);
}

pub fn cleanse_schema_contract_mark_done(plan: &mut CleansePlan, dataset_id: &str) {
    schema_contract_mark_done(plan, dataset_id);
}

pub fn cleanse_schema_contract_mark_needs_update(
    plan: &mut CleansePlan,
    dataset_id: &str,
    reason: Option<&str>,
) {
    schema_contract_mark_needs_update(plan, dataset_id, reason);
}

pub fn cleanse_schema_contract_mark_in_progress(plan: &mut CleansePlan, dataset_id: &str) {
    schema_contract_mark_in_progress(plan, dataset_id);
}

pub fn mark_needs_update<T: PlanTask>(
    plan: &mut Plan<T>,
    task_id: &str,
    label: &str,
    reason: Option<&str>,
) {
    if let Some(t) = plan.tasks.iter_mut().find(|t| t.task_id() == task_id) {
        let it = ensure_checklist_item(t.checklist_mut(), CHECKLIST_SQL_MODEL, label);
        set_checklist_status(it, ChecklistItemStatus::NeedsUpdate, None);
        if let Some(r) = reason.map(str::trim).filter(|s| !s.is_empty()) {
            it.details = Some(r.to_string());
        }
        recompute_task_status(t);
    }
}

pub fn mark_in_progress<T: PlanTask>(plan: &mut Plan<T>, task_id: &str, label: &str) {
    if let Some(t) = plan.tasks.iter_mut().find(|t| t.task_id() == task_id) {
        let it = ensure_checklist_item(t.checklist_mut(), CHECKLIST_SQL_MODEL, label);
        set_checklist_status(it, ChecklistItemStatus::InProgress, None);
        recompute_task_status(t);
    }
}

pub fn cleanse_mark_needs_update(plan: &mut CleansePlan, dataset_id: &str, reason: Option<&str>) {
    mark_needs_update(plan, dataset_id, "Author staging SQL", reason);
}

pub fn model_mark_needs_update(plan: &mut ModelPlan, name: &str, reason: Option<&str>) {
    mark_needs_update(plan, name, "Author gold SQL", reason);
}

pub fn cleanse_mark_in_progress(plan: &mut CleansePlan, dataset_id: &str) {
    mark_in_progress(plan, dataset_id, "Author staging SQL");
}

pub fn model_mark_in_progress(plan: &mut ModelPlan, name: &str) {
    mark_in_progress(plan, name, "Author gold SQL");
}

pub fn mark_validate_done<T: PlanTask>(plan: &mut Plan<T>, label: &str) {
    for t in plan.tasks.iter_mut() {
        let it = ensure_checklist_item(t.checklist_mut(), CHECKLIST_VALIDATE, label);
        set_checklist_status(it, ChecklistItemStatus::Done, None);
        recompute_task_status(t);
    }
}

pub fn cleanse_mark_validate_done(plan: &mut CleansePlan) {
    mark_validate_done(plan, "Validate");
}

pub fn model_mark_validate_done(plan: &mut ModelPlan) {
    mark_validate_done(plan, "Validate DBT");
}

pub fn all_done<T: PlanTask>(plan: &Plan<T>) -> bool {
    snapshot_completion(plan).all_done
}

#[cfg(test)]
pub fn cleanse_all_done(plan: &CleansePlan) -> bool {
    all_done(plan)
}

#[cfg(test)]
pub fn model_all_done(plan: &ModelPlan) -> bool {
    all_done(plan)
}

pub fn apply_cleanse_progress_event(plan: &mut CleansePlan, event: PlanProgressEvent) {
    match event {
        PlanProgressEvent::CleanseSqlInProgress { dataset_id } => {
            cleanse_mark_in_progress(plan, &dataset_id);
        }
        PlanProgressEvent::CleanseSqlDone { dataset_id } => {
            cleanse_mark_done(plan, &dataset_id);
        }
        PlanProgressEvent::CleanseSqlNeedsUpdate { dataset_id } => {
            cleanse_mark_needs_update(plan, &dataset_id, None);
        }
        PlanProgressEvent::CleanseSchemaInProgress { dataset_id } => {
            cleanse_schema_contract_mark_in_progress(plan, &dataset_id);
        }
        PlanProgressEvent::CleanseSchemaDone { dataset_id } => {
            cleanse_schema_contract_mark_done(plan, &dataset_id);
        }
        PlanProgressEvent::CleanseSchemaNeedsUpdate { dataset_id } => {
            cleanse_schema_contract_mark_needs_update(plan, &dataset_id, None);
        }
        PlanProgressEvent::CleanseValidateDone => {
            cleanse_mark_validate_done(plan);
        }
        _ => {}
    }
}

pub fn apply_model_progress_event(plan: &mut ModelPlan, event: PlanProgressEvent) {
    match event {
        PlanProgressEvent::ModelSqlInProgress { item_name } => {
            model_mark_in_progress(plan, &item_name);
        }
        PlanProgressEvent::ModelSqlDone { item_name } => {
            model_mark_done(plan, &item_name);
        }
        PlanProgressEvent::ModelSqlNeedsUpdate { item_name } => {
            model_mark_needs_update(plan, &item_name, None);
        }
        PlanProgressEvent::ModelSchemaInProgress { item_name } => {
            model_schema_contract_mark_in_progress(plan, &item_name);
        }
        PlanProgressEvent::ModelSchemaDone { item_name } => {
            model_schema_contract_mark_done(plan, &item_name);
        }
        PlanProgressEvent::ModelSchemaNeedsUpdate { item_name } => {
            model_schema_contract_mark_needs_update(plan, &item_name, None);
        }
        PlanProgressEvent::ModelValidateDone => {
            model_mark_validate_done(plan);
        }
        _ => {}
    }
}

pub fn snapshot_completion<T: PlanTask>(plan: &Plan<T>) -> PlanCompletionSnapshot {
    let mut pending_count: usize = 0;
    let mut pending_refs: Vec<PlanPendingRef> = Vec::new();
    for t in plan.tasks.iter() {
        for checklist_item_id in required_checklist_item_ids() {
            if checklist_status(t.checklist(), checklist_item_id) != ChecklistItemStatus::Done {
                pending_count += 1;
                pending_refs.push(PlanPendingRef::Generic {
                    task_id: t.task_id().to_string(),
                    checklist_item_id: checklist_item_id.to_string(),
                });
            }
        }
    }
    PlanCompletionSnapshot {
        all_done: !plan.tasks.is_empty() && pending_count == 0,
        pending_count,
        pending_refs,
    }
}

pub fn snapshot_cleanse_completion(plan: &CleansePlan) -> PlanCompletionSnapshot {
    let mut pending_refs: Vec<PlanPendingRef> = Vec::new();
    for t in plan.tasks.iter() {
        for checklist_item_id in required_checklist_item_ids() {
            if checklist_status(&t.checklist, checklist_item_id) != ChecklistItemStatus::Done {
                pending_refs.push(PlanPendingRef::Cleanse {
                    dataset_id: t.dataset_id.clone(),
                    checklist_item_id: checklist_item_id.to_string(),
                });
            }
        }
    }
    PlanCompletionSnapshot {
        all_done: !plan.tasks.is_empty() && pending_refs.is_empty(),
        pending_count: pending_refs.len(),
        pending_refs,
    }
}

pub fn snapshot_model_completion(plan: &ModelPlan) -> PlanCompletionSnapshot {
    let mut pending_refs: Vec<PlanPendingRef> = Vec::new();
    for t in plan.tasks.iter() {
        for checklist_item_id in required_checklist_item_ids() {
            if checklist_status(&t.checklist, checklist_item_id) != ChecklistItemStatus::Done {
                pending_refs.push(PlanPendingRef::Model {
                    item_name: t.name.clone(),
                    checklist_item_id: checklist_item_id.to_string(),
                });
            }
        }
    }
    PlanCompletionSnapshot {
        all_done: !plan.tasks.is_empty() && pending_refs.is_empty(),
        pending_count: pending_refs.len(),
        pending_refs,
    }
}

#[cfg(test)]
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
                .and_then(|c| c.get_str("checklist_item_id"))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| CHECKLIST_SQL_MODEL.to_string());
            let batch = parse_batch_contract::<
                crate::tools::batch_contracts::CleanseSqlBatchContract,
            >(&observation.extra, "apply_next_cleanse_batch contract");
            let ok = batch.ok;
            let attempted = batch.attempted_dataset_ids;
            let succeeded = batch.succeeded_dataset_ids;
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
                    recompute_task_status(t);
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
                        Some(evidence_from_tool_end(
                            idx,
                            "tool_end_ok",
                            name,
                            tool_id,
                            ts,
                        )),
                    );
                    recompute_task_status(t);
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
                            Some(evidence_from_tool_end(
                                idx,
                                "tool_end_failed",
                                name,
                                tool_id,
                                ts,
                            )),
                        );
                        recompute_task_status(t);
                    }
                }
            }
            plan.progress.last_applied_step_idx = idx + 1;
            continue;
        }

        if name == "apply_next_cleanse_schema_batch" {
            let batch =
                parse_batch_contract::<crate::tools::batch_contracts::CleanseSchemaBatchContract>(
                    &observation.extra,
                    "apply_next_cleanse_schema_batch contract",
                );
            let checklist_item_id = batch.checklist_item_id;
            let ok = batch.ok;
            let attempted = batch.attempted_dataset_ids;
            let succeeded = batch.succeeded_dataset_ids;
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
                    recompute_task_status(t);
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
                        Some(evidence_from_tool_end(
                            idx,
                            "tool_end_ok",
                            name,
                            tool_id,
                            ts,
                        )),
                    );
                    recompute_task_status(t);
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
                        recompute_task_status(t);
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
                    recompute_task_status(t);
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
                            Some(evidence_from_tool_end(
                                idx,
                                "tool_end_failed",
                                name,
                                tool_id,
                                ts,
                            )),
                        );
                        recompute_task_status(t);
                    }
                }
            }
            plan.progress.last_applied_step_idx = idx + 1;
            continue;
        }

        if name == "file" {
            let op = args.get("op").and_then(|v| v.as_str()).unwrap_or("");
            if op == "patch" || op == "mv" {
                let ok = observation.ok;

                let paths = extract_file_paths(op, args);
                // SQL patching can be used for targeted remediation; treat successful patches as progress.
                let mut stems: Vec<String> = paths.iter().filter_map(|p| file_stem(p)).collect();
                stems.sort();
                stems.dedup();
                let sql_checklist_item_id = step_ctx
                    .as_ref()
                    .and_then(|c| c.get_str("checklist_item_id"))
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
                            recompute_task_status(t);
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
                        .and_then(|c| c.get_str("checklist_item_id"))
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
                        if stems.is_empty()
                            || stems.iter().any(|s| s == &st)
                            || paths.iter().any(|pp| pp.trim() == "models/schema.yml")
                        {
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
                            recompute_task_status(t);
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
                    if checklist_status(&t.checklist, CHECKLIST_SQL_MODEL)
                        != ChecklistItemStatus::Done
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
                        Some(evidence_from_tool_end(
                            idx,
                            "tool_end_ok",
                            name,
                            tool_id,
                            ts,
                        )),
                    );
                    recompute_task_status(t);
                }
            } else {
                let logs = observation
                    .extra
                    .get("logs")
                    .cloned()
                    .unwrap_or(Value::Null);
                let contract_data_type_missing =
                    crate::dbt_error::logs_indicate_contract_data_type_missing(&logs);

                for t in plan.tasks.iter_mut() {
                    let v = ensure_checklist_item(&mut t.checklist, CHECKLIST_VALIDATE, "Validate");
                    set_checklist_status(
                        v,
                        ChecklistItemStatus::NeedsUpdate,
                        Some(evidence_from_tool_end(
                            idx,
                            "tool_end_failed",
                            name,
                            tool_id,
                            ts,
                        )),
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
                    recompute_task_status(t);
                }
            }
            plan.progress.last_applied_step_idx = idx + 1;
            continue;
        }
    }
}

#[cfg(test)]
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
                .and_then(|c| c.get_str("checklist_item_id"))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| CHECKLIST_SQL_MODEL.to_string());
            let batch = parse_batch_contract::<crate::tools::batch_contracts::ModelSqlBatchContract>(
                &observation.extra,
                "apply_next_model_batch contract",
            );
            let ok = batch.ok;
            let attempted = batch.attempted_item_names;
            let succeeded = batch.succeeded_item_names;
            let succ_set: std::collections::HashSet<String> = succeeded.iter().cloned().collect();
            let failed: Vec<String> = attempted
                .iter()
                .filter(|n| !succ_set.contains(*n))
                .cloned()
                .collect();

            for n in attempted.iter() {
                if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == *n) {
                    let it = if checklist_item_id == CHECKLIST_SQL_MODEL {
                        ensure_checklist_item(
                            &mut t.checklist,
                            CHECKLIST_SQL_MODEL,
                            "Author gold SQL",
                        )
                    } else {
                        ensure_checklist_item_any(&mut t.checklist, &checklist_item_id)
                    };
                    set_checklist_status(
                        it,
                        ChecklistItemStatus::InProgress,
                        Some(evidence_from_tool_end(idx, "tool_end", name, tool_id, ts)),
                    );
                    recompute_task_status(t);
                }
            }
            for n in succeeded.iter() {
                if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == *n) {
                    let it = if checklist_item_id == CHECKLIST_SQL_MODEL {
                        ensure_checklist_item(
                            &mut t.checklist,
                            CHECKLIST_SQL_MODEL,
                            "Author gold SQL",
                        )
                    } else {
                        ensure_checklist_item_any(&mut t.checklist, &checklist_item_id)
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
                    recompute_task_status(t);
                }
            }
            if !ok || !failed.is_empty() {
                for n in failed.iter() {
                    if let Some(t) = plan.tasks.iter_mut().find(|t| t.name == *n) {
                        let it = if checklist_item_id == CHECKLIST_SQL_MODEL {
                            ensure_checklist_item(
                                &mut t.checklist,
                                CHECKLIST_SQL_MODEL,
                                "Author gold SQL",
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
                        recompute_task_status(t);
                    }
                }
            }
            plan.progress.last_applied_step_idx = idx + 1;
            continue;
        }

        if name == "apply_next_model_schema_batch" {
            let batch = parse_batch_contract::<
                crate::tools::batch_contracts::ModelSchemaBatchContract,
            >(&observation.extra, "apply_next_model_schema_batch contract");
            let checklist_item_id = batch.checklist_item_id;
            let ok = batch.ok;
            let attempted = batch.attempted_item_names;
            let succeeded = batch.succeeded_item_names;
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
                    recompute_task_status(t);
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
                        Some(evidence_from_tool_end(
                            idx,
                            "tool_end_ok",
                            name,
                            tool_id,
                            ts,
                        )),
                    );
                    recompute_task_status(t);
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
                        recompute_task_status(t);
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
                    recompute_task_status(t);
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
                        Some(evidence_from_tool_end(
                            idx,
                            "tool_end_ok",
                            name,
                            tool_id,
                            ts,
                        )),
                    );
                    recompute_task_status(t);
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
                        recompute_task_status(t);
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
                    let it =
                        ensure_checklist_item(&mut t.checklist, CHECKLIST_VALIDATE, "Validate DBT");
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
                    recompute_task_status(t);
                }
            } else {
                for t in plan.tasks.iter_mut() {
                    let it =
                        ensure_checklist_item(&mut t.checklist, CHECKLIST_VALIDATE, "Validate DBT");
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
                    recompute_task_status(t);
                }
            }
            plan.progress.last_applied_step_idx = idx + 1;
            continue;
        }

        // Capture file patch activity and map to schema-contract checklist items.
        if name == "file" {
            let op = args.get("op").and_then(|v| v.as_str()).unwrap_or("");
            if op == "patch" {
                let ok = observation.ok;
                let paths = extract_file_paths(op, args);
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
                        .and_then(|c| c.get_str("checklist_item_id"))
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
                        let has_diff_git = pt
                            .lines()
                            .any(|l| l.trim_start().starts_with("diff --git "));
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
                            if t.starts_with("--- ")
                                || t.starts_with("+++ ")
                                || t.starts_with("@@ ")
                                || t == "@@"
                            {
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
                                        if let Some(t) =
                                            plan.tasks.iter_mut().find(|t| t.name == nm)
                                        {
                                            let it = if schema_checklist_item_id
                                                == CHECKLIST_SCHEMA_CONTRACT
                                            {
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
                                            recompute_task_status(t);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                let Some(schema_checklist_item_id) = step_ctx
                    .as_ref()
                    .and_then(|c| c.get_str("checklist_item_id"))
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
                        recompute_task_status(t);
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

#[cfg(test)]
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
                react_core::session::ToolStepStatus::Ok
            } else {
                react_core::session::ToolStepStatus::Failed
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
                react_core::session::ToolStepStatus::Ok
            } else {
                react_core::session::ToolStepStatus::Failed
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
                evidence: vec![],
            },
            PlanChecklistItem {
                checklist_item_id: CHECKLIST_SCHEMA_CONTRACT.to_string(),
                label: "Author schema contract".to_string(),
                details: None,
                status: ChecklistItemStatus::Pending,
                origin: ChecklistOrigin::Initial,
                evidence: vec![],
            },
            PlanChecklistItem {
                checklist_item_id: CHECKLIST_VALIDATE.to_string(),
                label: "Validate".to_string(),
                details: None,
                status: ChecklistItemStatus::Pending,
                origin: ChecklistOrigin::Initial,
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
            evidence_claim_refs: vec![crate::providers::SemanticClaimRef {
                claim_id: "candidate_key:test_raw.x:id".to_string().into(),
                kind: crate::providers::SemanticClaimKind::CandidateKey,
                status: crate::providers::EvidenceStatus::Observed,
            }],
        }
    }

    fn model_task(name: &str, inputs: Vec<&str>) -> ModelTask {
        ModelTask {
            name: name.to_string(),
            folder: ModelFolder::Marts,
            goal: "model goal".to_string(),
            inputs: inputs.into_iter().map(str::to_string).collect(),
            expected_model_path: Some(format!("models/marts/{name}.sql")),
            invariants: vec![],
            implementation_spec: Some(dummy_model_spec()),
            source_schema: vec![],
            grounded_inputs: vec![],
            status: TaskStatus::Pending,
            checklist: std_checklist("Author gold SQL"),
        }
    }

    #[test]
    fn canonical_model_batches_order_intra_plan_dependencies_first() {
        let tasks = vec![
            model_task("fct_orders", vec!["dim_customers", "stg_orders"]),
            model_task("dim_customers", vec!["stg_customers"]),
            model_task("agg_orders", vec!["fct_orders"]),
        ];

        let batches = canonical_model_batches_from_tasks(&tasks);

        assert_eq!(
            batches,
            vec![
                vec!["dim_customers".to_string()],
                vec!["fct_orders".to_string()],
                vec!["agg_orders".to_string()],
            ]
        );
    }

    #[test]
    fn sequential_model_work_groups_gate_next_sql_on_previous_schema() {
        let batches = vec![
            vec!["dim_customers".to_string()],
            vec!["fct_orders".to_string()],
        ];

        let groups = canonical_sequential_work_groups_from_batches(&batches, "model");

        assert_eq!(groups[0].group_id, "model_author_sql_001");
        assert_eq!(groups[0].depends_on_group_ids, None);
        assert_eq!(groups[2].group_id, "model_author_sql_002");
        assert_eq!(
            groups[2].depends_on_group_ids,
            Some(vec!["model_author_schema_001".to_string()])
        );
    }

    #[test]
    fn persistable_cleanse_plan_rejects_non_terminal_ungrounded() {
        let plan = CleansePlan {
            plan_key: "k".to_string(),
            status: PlanStatus::Draft,
            project_snapshot: Default::default(),
            tasks: vec![CleanseTask {
                dataset_id: "a.b.c".to_string(),
                expected_model_path: None,
                invariants: vec![],
                implementation_spec: Some(dummy_cleanse_spec()),
                source_schema: vec![],
                status: TaskStatus::Pending,
                checklist: std_checklist("Author staging SQL"),
            }],
            batches: vec![vec!["a.b.c".to_string()]],
            work_groups: vec![],
            mutations: vec![],
            progress: PlanProgress::default(),
        };
        let err = PersistableCleansePlan::try_from(plan).unwrap_err();
        assert!(err.contains("cleanse_plan_grounding_failed"));
    }

    #[test]
    fn persistable_model_plan_rejects_non_terminal_ungrounded() {
        let plan = ModelPlan {
            plan_key: "k".to_string(),
            status: PlanStatus::Approved,
            project_snapshot: Default::default(),
            tasks: vec![ModelTask {
                name: "dim_orders".to_string(),
                folder: ModelFolder::Marts,
                goal: "".to_string(),
                inputs: vec![],
                expected_model_path: None,
                invariants: vec![],
                implementation_spec: Some(dummy_model_spec()),
                source_schema: vec![],
                grounded_inputs: vec![],
                status: TaskStatus::Pending,
                checklist: std_checklist("Author gold SQL"),
            }],
            batches: vec![vec!["dim_orders".to_string()]],
            work_groups: vec![],
            mutations: vec![],
            progress: PlanProgress::default(),
        };
        let err =
            PersistableModelPlan::from_plan(plan, &std::collections::BTreeSet::new()).unwrap_err();
        assert!(err.contains("model_plan_grounding_failed"));
    }

    #[test]
    fn persistable_terminal_plans_allow_non_executable_shape() {
        let cleanse = CleansePlan {
            plan_key: "k1".to_string(),
            status: PlanStatus::Cancelled,
            project_snapshot: Default::default(),
            tasks: vec![],
            batches: vec![],
            work_groups: vec![],
            mutations: vec![],
            progress: PlanProgress::default(),
        };
        let model = ModelPlan {
            plan_key: "k2".to_string(),
            status: PlanStatus::Completed,
            project_snapshot: Default::default(),
            tasks: vec![],
            batches: vec![],
            work_groups: vec![],
            mutations: vec![],
            progress: PlanProgress::default(),
        };
        assert!(PersistableCleansePlan::try_from(cleanse).is_ok());
        assert!(PersistableModelPlan::from_plan(model, &std::collections::BTreeSet::new()).is_ok());
    }

    #[test]
    fn validate_cleanse_plan_semantics_rejects_missing_design_spec_details() {
        let mut bad_spec = dummy_cleanse_spec();
        bad_spec.output_fields = vec![];
        let plan = CleansePlan {
            plan_key: "k".to_string(),
            status: PlanStatus::Draft,
            project_snapshot: Default::default(),
            tasks: vec![CleanseTask {
                dataset_id: "a.b.c".to_string(),
                expected_model_path: Some("models/staging/stg_b_c.sql".to_string()),
                invariants: vec![],
                implementation_spec: Some(bad_spec),
                source_schema: vec![],
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
        assert!(v
            .errors
            .iter()
            .any(|e| e.contains("implementation_spec.output_fields is empty")));
    }

    #[test]
    fn validate_cleanse_plan_semantics_rejects_empty_work_groups() {
        let plan = CleansePlan {
            plan_key: "k".to_string(),
            status: PlanStatus::Draft,
            project_snapshot: Default::default(),
            tasks: vec![CleanseTask {
                dataset_id: "a.b.c".to_string(),
                expected_model_path: Some("models/staging/stg_b_c.sql".to_string()),
                invariants: vec![],
                implementation_spec: Some(dummy_cleanse_spec()),
                source_schema: vec![],
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
        assert!(v.errors.iter().any(|e| e.contains("work_groups is empty")));
    }

    #[test]
    fn validate_cleanse_plan_semantics_rejects_duplicate_ids() {
        let mut plan = CleansePlan {
            plan_key: "k".to_string(),
            status: PlanStatus::Draft,
            project_snapshot: Default::default(),
            tasks: vec![
                CleanseTask {
                    dataset_id: "a.b.c".to_string(),
                    expected_model_path: Some("models/staging/stg_b_c.sql".to_string()),
                    invariants: vec![],
                    implementation_spec: Some(dummy_cleanse_spec()),
                    source_schema: vec![],
                    status: TaskStatus::Pending,
                    checklist: std_checklist("Author staging SQL"),
                },
                CleanseTask {
                    dataset_id: "a.b.c".to_string(),
                    expected_model_path: Some("models/staging/stg_b_c.sql".to_string()),
                    invariants: vec![],
                    implementation_spec: Some(dummy_cleanse_spec()),
                    source_schema: vec![],
                    status: TaskStatus::Pending,
                    checklist: std_checklist("Author staging SQL"),
                },
            ],
            batches: vec![vec!["a.b.c".to_string(), "a.b.c".to_string()]],
            work_groups: canonical_work_groups_from_batches(
                &[vec!["a.b.c".to_string()]],
                "cleanse",
            ),
            mutations: vec![],
            progress: PlanProgress::default(),
        };
        plan.work_groups[0].items.push(WorkGroupItemRef {
            task_id: "a.b.c".to_string(),
            checklist_item_id: CHECKLIST_SQL_MODEL.to_string(),
        });
        let v = validate_cleanse_plan_semantics(&plan);
        assert!(!v.ok, "expected validation to fail, errors: {:?}", v.errors);
        assert!(
            v.errors
                .iter()
                .any(|e| e.contains("duplicate task.dataset_id")),
            "expected 'duplicate task.dataset_id' in errors: {:?}",
            v.errors
        );
        assert!(
            v.errors
                .iter()
                .any(|e| e.contains("contains duplicate dataset_id")),
            "expected 'contains duplicate dataset_id' in errors: {:?}",
            v.errors
        );
        assert!(v
            .errors
            .iter()
            .any(|e| e.contains("duplicate task/checklist ref")));
    }

    #[test]
    fn validate_model_plan_semantics_rejects_missing_grain_in_design_spec() {
        let mut bad_spec = dummy_model_spec();
        bad_spec.grain = "".to_string();
        let plan = ModelPlan {
            plan_key: "k".to_string(),
            status: PlanStatus::Draft,
            project_snapshot: Default::default(),
            tasks: vec![ModelTask {
                name: "m".to_string(),
                folder: ModelFolder::Marts,
                goal: "g".to_string(),
                inputs: vec!["stg_x".to_string()],
                expected_model_path: Some("models/marts/m.sql".to_string()),
                invariants: vec![],
                implementation_spec: Some(bad_spec),
                source_schema: vec![],
                grounded_inputs: vec![],
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
        assert!(v
            .errors
            .iter()
            .any(|e| e.contains("implementation_spec.grain is empty")));
    }

    #[test]
    fn validate_model_plan_semantics_rejects_invalid_work_group_item_refs() {
        let mut plan = ModelPlan {
            plan_key: "k".to_string(),
            status: PlanStatus::Draft,
            project_snapshot: Default::default(),
            tasks: vec![ModelTask {
                name: "fct_orders".to_string(),
                folder: ModelFolder::Marts,
                goal: "orders fact".to_string(),
                inputs: vec!["stg_orders".to_string()],
                expected_model_path: Some("models/marts/fct_orders.sql".to_string()),
                invariants: vec![],
                implementation_spec: Some(dummy_model_spec()),
                source_schema: vec![],
                grounded_inputs: vec![],
                status: TaskStatus::Pending,
                checklist: std_checklist("Author gold SQL"),
            }],
            batches: vec![vec!["fct_orders".to_string()]],
            work_groups: canonical_work_groups_from_batches(
                &[vec!["fct_orders".to_string()]],
                "model",
            ),
            mutations: vec![],
            progress: PlanProgress::default(),
        };
        plan.work_groups[0].items[0].task_id = "missing_task".to_string();
        let allowed = std::collections::BTreeSet::from(["stg_orders".to_string()]);
        let v = validate_model_plan_semantics(&plan, Some(&allowed));
        assert!(!v.ok);
        assert!(v
            .errors
            .iter()
            .any(|e| e.contains("references unknown model task_id=missing_task")));
    }

    #[test]
    fn validate_model_plan_semantics_rejects_duplicate_ids() {
        let mut plan = ModelPlan {
            plan_key: "k".to_string(),
            status: PlanStatus::Draft,
            project_snapshot: Default::default(),
            tasks: vec![
                ModelTask {
                    name: "fct_orders".to_string(),
                    folder: ModelFolder::Marts,
                    goal: "orders fact".to_string(),
                    inputs: vec!["stg_orders".to_string()],
                    expected_model_path: Some("models/marts/fct_orders.sql".to_string()),
                    invariants: vec![],
                    implementation_spec: Some(dummy_model_spec()),
                    source_schema: vec![],
                    grounded_inputs: vec![],
                    status: TaskStatus::Pending,
                    checklist: std_checklist("Author gold SQL"),
                },
                ModelTask {
                    name: "fct_orders".to_string(),
                    folder: ModelFolder::Marts,
                    goal: "orders fact".to_string(),
                    inputs: vec!["stg_orders".to_string()],
                    expected_model_path: Some("models/marts/fct_orders.sql".to_string()),
                    invariants: vec![],
                    implementation_spec: Some(dummy_model_spec()),
                    source_schema: vec![],
                    grounded_inputs: vec![],
                    status: TaskStatus::Pending,
                    checklist: std_checklist("Author gold SQL"),
                },
            ],
            batches: vec![vec!["fct_orders".to_string(), "fct_orders".to_string()]],
            work_groups: canonical_work_groups_from_batches(
                &[vec!["fct_orders".to_string()]],
                "model",
            ),
            mutations: vec![],
            progress: PlanProgress::default(),
        };
        plan.work_groups[0].items.push(WorkGroupItemRef {
            task_id: "fct_orders".to_string(),
            checklist_item_id: CHECKLIST_SQL_MODEL.to_string(),
        });
        let allowed = std::collections::BTreeSet::from(["stg_orders".to_string()]);
        let v = validate_model_plan_semantics(&plan, Some(&allowed));
        assert!(!v.ok);
        assert!(v.errors.iter().any(|e| e.contains("duplicate task.name")));
        assert!(v
            .errors
            .iter()
            .any(|e| e.contains("contains duplicate model name")));
        assert!(v
            .errors
            .iter()
            .any(|e| e.contains("duplicate task/checklist ref")));
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
            project_snapshot: Default::default(),
            tasks: vec![
                CleanseTask {
                    dataset_id: "a.b.c".to_string(),
                    expected_model_path: None,
                    invariants: vec![],
                    implementation_spec: Some(dummy_cleanse_spec()),
                    source_schema: vec![],
                    status: TaskStatus::Pending,
                    checklist: std_checklist("Author staging SQL"),
                },
                CleanseTask {
                    dataset_id: "d.e.f".to_string(),
                    expected_model_path: None,
                    invariants: vec![],
                    implementation_spec: Some(dummy_cleanse_spec()),
                    source_schema: vec![],
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
            project_snapshot: Default::default(),
            tasks: vec![
                ModelTask {
                    name: "fct_a".to_string(),
                    folder: ModelFolder::Marts,
                    goal: "a".to_string(),
                    inputs: vec!["stg_x".to_string()],
                    expected_model_path: None,
                    invariants: vec![],
                    implementation_spec: Some(dummy_model_spec()),
                    source_schema: vec![],
                    grounded_inputs: vec![],
                    status: TaskStatus::Pending,
                    checklist: std_checklist("Author gold SQL"),
                },
                ModelTask {
                    name: "dim_b".to_string(),
                    folder: ModelFolder::Core,
                    goal: "b".to_string(),
                    inputs: vec!["stg_y".to_string()],
                    expected_model_path: None,
                    invariants: vec![],
                    implementation_spec: Some(dummy_model_spec()),
                    source_schema: vec![],
                    grounded_inputs: vec![],
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
    fn completion_predicate_requires_checklist_done_not_task_status() {
        let mut plan = CleansePlan {
            plan_key: "k".to_string(),
            status: PlanStatus::Approved,
            project_snapshot: Default::default(),
            tasks: vec![CleanseTask {
                dataset_id: "a.b.c".to_string(),
                expected_model_path: Some("models/staging/stg_b_c.sql".to_string()),
                invariants: vec![],
                implementation_spec: Some(dummy_cleanse_spec()),
                source_schema: vec![],
                status: TaskStatus::Done,
                checklist: std_checklist("Author staging SQL"),
            }],
            batches: vec![vec!["a.b.c".to_string()]],
            work_groups: canonical_work_groups_from_batches(
                &[vec!["a.b.c".to_string()]],
                "cleanse",
            ),
            mutations: vec![],
            progress: PlanProgress::default(),
        };
        // Force stale task status while checklist remains incomplete.
        plan.tasks[0].status = TaskStatus::Done;
        assert!(!cleanse_all_done(&plan));
    }

    #[test]
    fn cleanse_progress_marks_custom_checklist_from_apply_next_cleanse_batch_ctx() {
        let mut plan = CleansePlan {
            plan_key: "k".to_string(),
            status: PlanStatus::Approved,
            project_snapshot: Default::default(),
            tasks: vec![CleanseTask {
                dataset_id: "a.b.c".to_string(),
                expected_model_path: None,
                invariants: vec![],
                implementation_spec: Some(dummy_cleanse_spec()),
                source_schema: vec![],
                status: TaskStatus::Pending,
                checklist: vec![
                    PlanChecklistItem {
                        checklist_item_id: CHECKLIST_SQL_MODEL.to_string(),
                        label: "Author staging SQL".to_string(),
                        details: None,
                        status: ChecklistItemStatus::Pending,
                        origin: ChecklistOrigin::Initial,
                        evidence: vec![],
                    },
                    PlanChecklistItem {
                        checklist_item_id: "time_derivatives".to_string(),
                        label: "Time derivatives".to_string(),
                        details: None,
                        status: ChecklistItemStatus::Pending,
                        origin: ChecklistOrigin::Initial,
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
                Some({
                    let mut ctx = ExecutionContext::default();
                    ctx.set(
                        "plan_kind",
                        serde_json::Value::String("cleanse".to_string()),
                    );
                    ctx.set("plan_key", serde_json::Value::String("k".to_string()));
                    ctx.set("workgroup_id", serde_json::Value::String("wg".to_string()));
                    ctx.set("task_id", serde_json::Value::String("a.b.c".to_string()));
                    ctx.set(
                        "checklist_item_id",
                        serde_json::Value::String("time_derivatives".to_string()),
                    );
                    ctx
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
            project_snapshot: Default::default(),
            tasks: vec![CleanseTask {
                dataset_id: "a.b.c".to_string(),
                expected_model_path: None,
                invariants: vec![],
                implementation_spec: Some(dummy_cleanse_spec()),
                source_schema: vec![],
                status: TaskStatus::Pending,
                checklist: vec![
                    PlanChecklistItem {
                        checklist_item_id: CHECKLIST_SQL_MODEL.to_string(),
                        label: "Author staging SQL".to_string(),
                        details: None,
                        status: ChecklistItemStatus::Done,
                        origin: ChecklistOrigin::Initial,
                        evidence: vec![],
                    },
                    PlanChecklistItem {
                        checklist_item_id: "collision_id_contract".to_string(),
                        label: "Collision id contract".to_string(),
                        details: None,
                        status: ChecklistItemStatus::Pending,
                        origin: ChecklistOrigin::Initial,
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
                    "checklist_item_id":"collision_id_contract",
                    "attempted_dataset_ids":["a.b.c"],
                    "succeeded_dataset_ids":["a.b.c"]
                }),
                Some({
                    let mut ctx = ExecutionContext::default();
                    ctx.set(
                        "plan_kind",
                        serde_json::Value::String("cleanse".to_string()),
                    );
                    ctx.set("plan_key", serde_json::Value::String("k".to_string()));
                    ctx.set("workgroup_id", serde_json::Value::String("wg".to_string()));
                    ctx.set("task_id", serde_json::Value::String("a.b.c".to_string()));
                    ctx.set(
                        "checklist_item_id",
                        serde_json::Value::String("collision_id_contract".to_string()),
                    );
                    ctx
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
            project_snapshot: Default::default(),
            tasks: vec![
                ModelTask {
                    name: "dim_customers".to_string(),
                    folder: ModelFolder::Marts,
                    goal: "a".to_string(),
                    inputs: vec!["stg_test_raw_raw_customers".to_string()],
                    expected_model_path: None,
                    invariants: vec![],
                    implementation_spec: Some(dummy_model_spec()),
                    source_schema: vec![],
                    grounded_inputs: vec![],
                    status: TaskStatus::Pending,
                    checklist: std_checklist("Author gold SQL"),
                },
                ModelTask {
                    name: "dim_orders".to_string(),
                    folder: ModelFolder::Marts,
                    goal: "b".to_string(),
                    inputs: vec!["stg_test_raw_raw_orders".to_string()],
                    expected_model_path: None,
                    invariants: vec![],
                    implementation_spec: Some(dummy_model_spec()),
                    source_schema: vec![],
                    grounded_inputs: vec![],
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
                "file",
                serde_json::json!({
                    "op":"patch",
                    "path": "models/schema.yml",
                    "patch_text": "@@ ... @@\n+version: 2\n+\n+models:\n+  - name: dim_customers\n+    columns:\n+      - name: customer_id\n+sources:\n+  - name: test_raw\n"
                }),
                serde_json::json!({"ok": true}),
                Some({
                    let mut ctx = ExecutionContext::default();
                    ctx.set("plan_kind", serde_json::Value::String("model".to_string()));
                    ctx.set("plan_key", serde_json::Value::String("k".to_string()));
                    ctx.set("workgroup_id", serde_json::Value::String("wg".to_string()));
                    ctx.set(
                        "task_id",
                        serde_json::Value::String("dim_customers".to_string()),
                    );
                    ctx.set(
                        "checklist_item_id",
                        serde_json::Value::String(CHECKLIST_SCHEMA_CONTRACT.to_string()),
                    );
                    ctx
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
            project_snapshot: Default::default(),
            tasks: vec![
                CleanseTask {
                    dataset_id: "AwsDataCatalog.test_raw.raw_customers".to_string(),
                    expected_model_path: None,
                    invariants: vec![],
                    implementation_spec: Some(dummy_cleanse_spec()),
                    source_schema: vec![],
                    status: TaskStatus::Pending,
                    checklist: vec![],
                },
                CleanseTask {
                    dataset_id: "AwsDataCatalog.test_raw.raw_products".to_string(),
                    expected_model_path: None,
                    invariants: vec![],
                    implementation_spec: Some(dummy_cleanse_spec()),
                    source_schema: vec![],
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
            project_snapshot: Default::default(),
            tasks: vec![
                ModelTask {
                    name: "fct_ok".to_string(),
                    folder: ModelFolder::Marts,
                    goal: "ok".to_string(),
                    inputs: vec!["stg_customers".to_string()],
                    expected_model_path: None,
                    invariants: vec![],
                    implementation_spec: Some(dummy_model_spec()),
                    source_schema: vec![],
                    grounded_inputs: vec![],
                    status: TaskStatus::Pending,
                    checklist: vec![],
                },
                ModelTask {
                    name: "fct_bad".to_string(),
                    folder: ModelFolder::Marts,
                    goal: "bad".to_string(),
                    inputs: vec!["raw_orders".to_string()],
                    expected_model_path: None,
                    invariants: vec![],
                    implementation_spec: Some(dummy_model_spec()),
                    source_schema: vec![],
                    grounded_inputs: vec![],
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
    fn prune_model_plan_to_grounded_staging_models_prunes_tasks_with_empty_inputs() {
        let mut plan = ModelPlan {
            plan_key: "k".to_string(),
            status: PlanStatus::Draft,
            project_snapshot: Default::default(),
            tasks: vec![ModelTask {
                name: "fct_missing_inputs".to_string(),
                folder: ModelFolder::Marts,
                goal: "x".to_string(),
                inputs: vec![],
                expected_model_path: None,
                invariants: vec![],
                implementation_spec: Some(dummy_model_spec()),
                source_schema: vec![],
                grounded_inputs: vec![],
                status: TaskStatus::Pending,
                checklist: vec![],
            }],
            batches: vec![vec!["fct_missing_inputs".to_string()]],
            work_groups: vec![],
            mutations: vec![],
            progress: PlanProgress::default(),
        };
        let mut allowed = std::collections::BTreeSet::new();
        allowed.insert("stg_customers".to_string());
        prune_model_plan_to_grounded_staging_models(&mut plan, &allowed);
        assert!(plan.tasks.is_empty());
        assert!(plan.batches.is_empty());
    }

    #[test]
    fn prune_model_plan_to_grounded_staging_models_normalizes_staging_input_shapes() {
        let mut plan = ModelPlan {
            plan_key: "k".to_string(),
            status: PlanStatus::Draft,
            project_snapshot: Default::default(),
            tasks: vec![ModelTask {
                name: "fct_orders".to_string(),
                folder: ModelFolder::Marts,
                goal: "x".to_string(),
                inputs: vec![
                    "AwsDataCatalog.example1_silver.stg_test_raw_raw_orders".to_string(),
                    "{{ ref('stg_test_raw_raw_customers') }}".to_string(),
                    "models/staging/stg_test_raw_raw_order_items.sql".to_string(),
                ],
                expected_model_path: None,
                invariants: vec![],
                implementation_spec: Some(dummy_model_spec()),
                source_schema: vec![],
                grounded_inputs: vec![],
                status: TaskStatus::Pending,
                checklist: vec![],
            }],
            batches: vec![vec!["fct_orders".to_string()]],
            work_groups: vec![],
            mutations: vec![],
            progress: PlanProgress::default(),
        };
        let allowed = std::collections::BTreeSet::from([
            "stg_test_raw_raw_orders".to_string(),
            "stg_test_raw_raw_customers".to_string(),
            "stg_test_raw_raw_order_items".to_string(),
        ]);
        prune_model_plan_to_grounded_staging_models(&mut plan, &allowed);
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(
            plan.tasks[0].inputs,
            vec![
                "stg_test_raw_raw_customers".to_string(),
                "stg_test_raw_raw_order_items".to_string(),
                "stg_test_raw_raw_orders".to_string(),
            ]
        );
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
            project_snapshot: Default::default(),
            tasks: vec![ModelTask {
                name: "dim_customers".to_string(),
                folder: ModelFolder::Marts,
                goal: "x".to_string(),
                inputs: vec![],
                expected_model_path: None,
                invariants: vec![],
                implementation_spec: Some(dummy_model_spec()),
                source_schema: vec![],
                grounded_inputs: vec![],
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
            project_snapshot: Default::default(),
            tasks: vec![CleanseTask {
                dataset_id: "AwsDataCatalog.test_raw.raw_customers".to_string(),
                expected_model_path: None,
                invariants: vec![],
                implementation_spec: Some(dummy_cleanse_spec()),
                source_schema: vec![],
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
            project_snapshot: Default::default(),
            tasks: vec![CleanseTask {
                dataset_id: "AwsDataCatalog.test_raw.raw_customers".to_string(),
                expected_model_path: Some("models/staging/customers.sql".to_string()),
                invariants: vec![],
                implementation_spec: Some(dummy_cleanse_spec()),
                source_schema: vec![],
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
            project_snapshot: Default::default(),
            tasks: vec![CleanseTask {
                dataset_id: "AwsDataCatalog.test_raw.raw_customers".to_string(),
                expected_model_path: Some(
                    "models/staging/stg_test_raw_raw_customers.sql".to_string(),
                ),
                invariants: vec![],
                implementation_spec: Some(dummy_cleanse_spec()),
                source_schema: vec![],
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
                consecutive_infra_transient_failures: 0,
            },
        };
        let log = ThreadLog {
            steps: vec![step(
                "file",
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
            project_snapshot: Default::default(),
            tasks: vec![
                CleanseTask {
                    dataset_id: "AwsDataCatalog.test_raw.raw_orders".to_string(),
                    expected_model_path: Some(
                        "models/staging/stg_test_raw_raw_orders.sql".to_string(),
                    ),
                    invariants: vec![],
                    implementation_spec: Some(dummy_cleanse_spec()),
                    source_schema: vec![],
                    status: TaskStatus::Pending,
                    checklist: std_checklist("Author staging SQL"),
                },
                CleanseTask {
                    dataset_id: "AwsDataCatalog.test_raw.raw_customers".to_string(),
                    expected_model_path: Some(
                        "models/staging/stg_test_raw_raw_customers.sql".to_string(),
                    ),
                    invariants: vec![],
                    implementation_spec: Some(dummy_cleanse_spec()),
                    source_schema: vec![],
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
            let it =
                ensure_checklist_item(&mut t.checklist, CHECKLIST_SQL_MODEL, "Author staging SQL");
            it.status = ChecklistItemStatus::Done;
            recompute_task_status(t);
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
    fn cleanse_validate_failure_marks_needs_update_for_all_authored_tasks() {
        let mut plan = CleansePlan {
            plan_key: "k".to_string(),
            status: PlanStatus::Approved,
            project_snapshot: Default::default(),
            tasks: vec![
                CleanseTask {
                    dataset_id: "AwsDataCatalog.test_raw.raw_orders".to_string(),
                    expected_model_path: Some(
                        "models/staging/stg_test_raw_raw_orders.sql".to_string(),
                    ),
                    invariants: vec![],
                    implementation_spec: Some(dummy_cleanse_spec()),
                    source_schema: vec![],
                    status: TaskStatus::Pending,
                    checklist: std_checklist("Author staging SQL"),
                },
                CleanseTask {
                    dataset_id: "AwsDataCatalog.test_raw.raw_customers".to_string(),
                    expected_model_path: Some(
                        "models/staging/stg_test_raw_raw_customers.sql".to_string(),
                    ),
                    invariants: vec![],
                    implementation_spec: Some(dummy_cleanse_spec()),
                    source_schema: vec![],
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

        for t in plan.tasks.iter_mut() {
            let it =
                ensure_checklist_item(&mut t.checklist, CHECKLIST_SQL_MODEL, "Author staging SQL");
            it.status = ChecklistItemStatus::Done;
            recompute_task_status(t);
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
            ChecklistItemStatus::NeedsUpdate,
            "all tasks are marked NeedsUpdate on validate failure (no per-model implication)"
        );
    }

    #[test]
    fn apply_validate_done_event_updates_snapshot_for_cleanse() {
        let mut plan = CleansePlan {
            plan_key: "k".to_string(),
            status: PlanStatus::Approved,
            project_snapshot: Default::default(),
            tasks: vec![CleanseTask {
                dataset_id: "AwsDataCatalog.test_raw.raw_orders".to_string(),
                expected_model_path: Some("models/staging/stg_test_raw_raw_orders.sql".to_string()),
                invariants: vec![],
                implementation_spec: Some(dummy_cleanse_spec()),
                source_schema: vec![],
                status: TaskStatus::Pending,
                checklist: std_checklist("Author staging SQL"),
            }],
            batches: vec![vec!["AwsDataCatalog.test_raw.raw_orders".to_string()]],
            work_groups: vec![],
            mutations: vec![],
            progress: PlanProgress::default(),
        };

        apply_cleanse_progress_event(&mut plan, PlanProgressEvent::CleanseValidateDone);
        let snapshot = snapshot_cleanse_completion(&plan);
        assert!(!snapshot.all_done);
        assert_eq!(snapshot.pending_count, 2);
        assert!(snapshot.pending_refs.iter().all(|r| match r {
            PlanPendingRef::Cleanse {
                checklist_item_id, ..
            } => checklist_item_id != CHECKLIST_VALIDATE,
            _ => false,
        }));
    }

    #[test]
    fn apply_validate_done_event_updates_snapshot_for_model() {
        let mut plan = ModelPlan {
            plan_key: "k".to_string(),
            status: PlanStatus::Approved,
            project_snapshot: Default::default(),
            tasks: vec![ModelTask {
                name: "dim_orders".to_string(),
                folder: ModelFolder::Marts,
                goal: "x".to_string(),
                inputs: vec![],
                expected_model_path: Some("models/marts/dim_orders.sql".to_string()),
                invariants: vec![],
                implementation_spec: Some(dummy_model_spec()),
                source_schema: vec![],
                grounded_inputs: vec![],
                status: TaskStatus::Pending,
                checklist: std_checklist("Author gold SQL"),
            }],
            batches: vec![vec!["dim_orders".to_string()]],
            work_groups: vec![],
            mutations: vec![],
            progress: PlanProgress::default(),
        };

        apply_model_progress_event(&mut plan, PlanProgressEvent::ModelValidateDone);
        let snapshot = snapshot_model_completion(&plan);
        assert!(!snapshot.all_done);
        assert_eq!(snapshot.pending_count, 2);
        assert!(snapshot.pending_refs.iter().all(|r| match r {
            PlanPendingRef::Model {
                checklist_item_id, ..
            } => checklist_item_id != CHECKLIST_VALIDATE,
            _ => false,
        }));
    }
}
