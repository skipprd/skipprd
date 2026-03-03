use react_core::agent::AgentCtx;

use crate::data_engineer::plan_progress::{
    CHECKLIST_SQL_MODEL, checklist_status, missing_required_checklist_items,
    required_checklist_item_ids, work_groups_cover_task_checklist,
};
use crate::data_engineer::plan_types::{
    ChecklistItemStatus, CleansePlan, ModelPlan, PlanWorkGroup,
};

#[derive(Clone, Debug)]
pub struct PlanSemanticValidation {
    pub ok: bool,
    pub errors: Vec<String>,
    pub issues: Vec<PlanSemanticIssue>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanSemanticIssueCode {
    MissingPlanKey,
    MissingTasks,
    DuplicateTaskId,
    InvalidBatch,
    MissingImplementationSpec,
    MissingChecklistItems,
    UnknownTaskReference,
    MissingWorkGroups,
    MissingWorkGroupCoverage,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanSemanticIssue {
    pub code: PlanSemanticIssueCode,
    pub task_id: Option<String>,
    pub message: String,
}

impl PlanSemanticValidation {
    pub fn messages(&self) -> Vec<String> {
        if !self.errors.is_empty() {
            return self.errors.clone();
        }
        self.issues.iter().map(|i| i.message.clone()).collect()
    }
}

fn classify_semantic_issue(err: &str) -> PlanSemanticIssue {
    let msg = err.trim().to_string();
    let task_id = err
        .split_once(':')
        .map(|(h, _)| h.trim().to_string())
        .filter(|s| !s.is_empty() && s != "task" && s != "work_group");
    let lower = msg.to_ascii_lowercase();
    let code = if lower.contains("plan_key") {
        PlanSemanticIssueCode::MissingPlanKey
    } else if lower.contains("tasks is empty") {
        PlanSemanticIssueCode::MissingTasks
    } else if lower.contains("duplicate task.") {
        PlanSemanticIssueCode::DuplicateTaskId
    } else if lower.contains("batches[") || lower.contains("batch") {
        PlanSemanticIssueCode::InvalidBatch
    } else if lower.contains("implementation_spec")
        || lower.contains("expected_model_path")
        || lower.contains("goal is empty")
        || lower.contains("inputs is empty")
    {
        PlanSemanticIssueCode::MissingImplementationSpec
    } else if lower.contains("missing required checklist")
        || lower.contains("missing checklist_item_id")
    {
        PlanSemanticIssueCode::MissingChecklistItems
    } else if lower.contains("references unknown") || lower.contains("not present in tasks") {
        PlanSemanticIssueCode::UnknownTaskReference
    } else if lower.contains("work_groups is empty") {
        PlanSemanticIssueCode::MissingWorkGroups
    } else if lower.contains("not scheduled in work_groups") {
        PlanSemanticIssueCode::MissingWorkGroupCoverage
    } else {
        PlanSemanticIssueCode::Other
    };
    PlanSemanticIssue {
        code,
        task_id,
        message: msg,
    }
}

fn issues_from_errors(errors: &[String]) -> Vec<PlanSemanticIssue> {
    errors.iter().map(|e| classify_semantic_issue(e)).collect()
}

fn duplicate_values(values: &[String]) -> Vec<String> {
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for v in values.iter() {
        let t = v.trim();
        if t.is_empty() {
            continue;
        }
        *counts.entry(t.to_string()).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .filter_map(|(k, n)| if n > 1 { Some(k) } else { None })
        .collect()
}

fn duplicate_workgroup_refs(groups: &[PlanWorkGroup]) -> Vec<(String, String)> {
    let mut counts = std::collections::BTreeMap::<(String, String), usize>::new();
    for g in groups.iter() {
        for it in g.items.iter() {
            let tid = it.task_id.trim();
            let cid = it.checklist_item_id.trim();
            if tid.is_empty() || cid.is_empty() {
                continue;
            }
            *counts
                .entry((tid.to_string(), cid.to_string()))
                .or_insert(0) += 1;
        }
    }
    counts
        .into_iter()
        .filter_map(|(k, n)| if n > 1 { Some(k) } else { None })
        .collect()
}

fn is_runnable_checklist_status(s: ChecklistItemStatus) -> bool {
    matches!(
        s,
        ChecklistItemStatus::Pending
            | ChecklistItemStatus::InProgress
            | ChecklistItemStatus::NeedsUpdate
    )
}

pub fn validate_cleanse_plan_semantics(plan: &CleansePlan) -> PlanSemanticValidation {
    let mut errors: Vec<String> = Vec::new();
    if plan.plan_key.trim().is_empty() {
        errors.push("plan_key is missing".to_string());
    }
    if plan.tasks.is_empty() {
        errors.push("tasks is empty".to_string());
    }
    let task_ids = plan
        .tasks
        .iter()
        .map(|t| t.dataset_id.clone())
        .collect::<Vec<_>>();
    for dup in duplicate_values(&task_ids) {
        errors.push(format!("duplicate task.dataset_id is not allowed: {}", dup));
    }
    for (bi, b) in plan.batches.iter().enumerate() {
        if b.len() > 5 {
            errors.push(format!("batches[{bi}] has >5 items (len={})", b.len()));
        }
        for dup in duplicate_values(b) {
            errors.push(format!(
                "batches[{bi}] contains duplicate dataset_id '{}' (duplicates are forbidden)",
                dup
            ));
        }
    }
    for t in plan.tasks.iter() {
        if t.dataset_id.trim().is_empty() {
            errors.push("task.dataset_id is empty".to_string());
            continue;
        }
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
        let missing = missing_required_checklist_items(&t.checklist);
        if !missing.is_empty() {
            errors.push(format!(
                "{}: missing required checklist items: {}",
                t.dataset_id,
                missing.join(", ")
            ));
        }
    }
    for (bi, b) in plan.batches.iter().enumerate() {
        for ds in b.iter() {
            if plan.tasks.iter().find(|t| t.dataset_id == *ds).is_none() {
                errors.push(format!(
                    "batches[{bi}] references dataset_id not present in tasks: {ds}"
                ));
            }
        }
    }
    if plan.work_groups.is_empty() {
        errors.push("work_groups is empty".to_string());
    }
    for (task_id, checklist_item_id) in duplicate_workgroup_refs(&plan.work_groups) {
        errors.push(format!(
            "work_groups contains duplicate task/checklist ref: task_id='{}' checklist_item_id='{}'",
            task_id, checklist_item_id
        ));
    }
    for g in plan.work_groups.iter() {
        if g.items.len() > 5 {
            errors.push(format!(
                "work_group {} has >5 items (len={})",
                g.group_id,
                g.items.len()
            ));
        }
        for it in g.items.iter() {
            if plan
                .tasks
                .iter()
                .find(|t| t.dataset_id == it.task_id)
                .is_none()
            {
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
    for t in plan.tasks.iter() {
        for checklist_id in required_checklist_item_ids() {
            if !work_groups_cover_task_checklist(&plan.work_groups, &t.dataset_id, checklist_id) {
                errors.push(format!(
                    "task {} checklist '{}' is not scheduled in work_groups",
                    t.dataset_id, checklist_id
                ));
            }
        }
    }
    PlanSemanticValidation {
        ok: errors.is_empty(),
        errors: errors.clone(),
        issues: issues_from_errors(&errors),
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
    let task_names = plan.tasks.iter().map(|t| t.name.clone()).collect::<Vec<_>>();
    for dup in duplicate_values(&task_names) {
        errors.push(format!("duplicate task.name is not allowed: {}", dup));
    }
    for (bi, b) in plan.batches.iter().enumerate() {
        if b.len() > 5 {
            errors.push(format!("batches[{bi}] has >5 items (len={})", b.len()));
        }
        for dup in duplicate_values(b) {
            errors.push(format!(
                "batches[{bi}] contains duplicate model name '{}' (duplicates are forbidden)",
                dup
            ));
        }
    }
    for t in plan.tasks.iter() {
        if t.name.trim().is_empty() {
            errors.push("task.name is empty".to_string());
            continue;
        }
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
        if t.implementation_spec.output_fields.is_empty()
            && t.implementation_spec.metrics.is_empty()
        {
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
                        errors.push(format!(
                            "{}: input '{}' not grounded in models/staging/",
                            t.name, inp
                        ));
                    }
                }
            }
        }
        let missing = missing_required_checklist_items(&t.checklist);
        if !missing.is_empty() {
            errors.push(format!(
                "{}: missing required checklist items: {}",
                t.name,
                missing.join(", ")
            ));
        }
    }
    for (bi, b) in plan.batches.iter().enumerate() {
        for name in b.iter() {
            if plan.tasks.iter().find(|t| t.name == *name).is_none() {
                errors.push(format!(
                    "batches[{bi}] references model name not present in tasks: {name}"
                ));
            }
        }
    }
    if plan.work_groups.is_empty() {
        errors.push("work_groups is empty".to_string());
    }
    for (task_id, checklist_item_id) in duplicate_workgroup_refs(&plan.work_groups) {
        errors.push(format!(
            "work_groups contains duplicate task/checklist ref: task_id='{}' checklist_item_id='{}'",
            task_id, checklist_item_id
        ));
    }
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
    for t in plan.tasks.iter() {
        for checklist_id in required_checklist_item_ids() {
            if !work_groups_cover_task_checklist(&plan.work_groups, &t.name, checklist_id) {
                errors.push(format!(
                    "task {} checklist '{}' is not scheduled in work_groups",
                    t.name, checklist_id
                ));
            }
        }
    }
    PlanSemanticValidation {
        ok: errors.is_empty(),
        errors: errors.clone(),
        issues: issues_from_errors(&errors),
    }
}

pub async fn ensure_cleanse_plan_semantically_valid_or_repaired(
    ctx: &AgentCtx,
    plan: &mut CleansePlan,
) -> Result<PlanSemanticValidation, String> {
    crate::data_engineer::plan_grounding::normalize_cleanse_plan_defaults(plan);
    let v0 = validate_cleanse_plan_semantics(plan);
    if v0.ok {
        return Ok(v0);
    }
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
    let _ = ctx;
    let _ = allowed_staging_models;
    Ok(v0)
}
