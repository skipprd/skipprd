use serde_json::{json, Value};

use crate::suite::SuiteCtx;

use super::plan as de_plan;

fn map_plan_status(s: de_plan::PlanStatus) -> &'static str {
    match s {
        de_plan::PlanStatus::Draft => "draft",
        de_plan::PlanStatus::Approved => "approved",
        de_plan::PlanStatus::Completed => "completed",
        de_plan::PlanStatus::Cancelled => "cancelled",
    }
}

fn map_task_status(s: de_plan::TaskStatus) -> &'static str {
    match s {
        de_plan::TaskStatus::Pending => "pending",
        de_plan::TaskStatus::InProgress => "in_progress",
        de_plan::TaskStatus::Done => "done",
        de_plan::TaskStatus::Blocked => "blocked",
        de_plan::TaskStatus::NeedsUpdate => "needs_update",
    }
}

fn map_checklist_status(s: de_plan::ChecklistItemStatus) -> &'static str {
    match s {
        de_plan::ChecklistItemStatus::Pending => "pending",
        de_plan::ChecklistItemStatus::InProgress => "in_progress",
        de_plan::ChecklistItemStatus::Done => "done",
        de_plan::ChecklistItemStatus::Blocked => "blocked",
        de_plan::ChecklistItemStatus::NeedsUpdate => "needs_update",
    }
}

fn map_checklist_origin(s: de_plan::ChecklistOrigin) -> &'static str {
    match s {
        de_plan::ChecklistOrigin::Initial => "initial",
        de_plan::ChecklistOrigin::ReviewActionable => "review_actionable",
    }
}

fn map_work_group_kind(k: de_plan::WorkGroupKind) -> &'static str {
    match k {
        de_plan::WorkGroupKind::AuthorSql => "author_sql",
        de_plan::WorkGroupKind::AuthorSchema => "author_schema",
        de_plan::WorkGroupKind::Validate => "validate",
    }
}

#[derive(Clone, Copy)]
enum WsPlanKind {
    Cleanse,
    Model,
}

impl WsPlanKind {
    fn as_str(self) -> &'static str {
        match self {
            WsPlanKind::Cleanse => "cleanse",
            WsPlanKind::Model => "model",
        }
    }
}

fn checklist_item_to_value(it: de_plan::PlanChecklistItem) -> Value {
    let evidence = if it.evidence.is_empty() {
        None
    } else {
        Some(
            it.evidence
                .into_iter()
                .map(|e| {
                    json!({
                        "kind": e.kind,
                        "toolName": e.tool_name,
                        "toolId": e.tool_id,
                        "stepIdx": e.step_idx as i32,
                        "ts": e.ts,
                    })
                })
                .collect::<Vec<_>>(),
        )
    };
    json!({
        "checklistItemId": it.checklist_item_id,
        "label": it.label,
        "details": it.details,
        "status": map_checklist_status(it.status),
        "origin": map_checklist_origin(it.origin),
        "originStepIdx": it.origin_step_idx.map(|x| x as i32),
        "evidence": evidence,
    })
}

fn work_group_to_value(wg: de_plan::PlanWorkGroup) -> Value {
    json!({
        "groupId": wg.group_id,
        "label": wg.label,
        "kind": map_work_group_kind(wg.kind),
        "items": wg.items.into_iter().map(|it| json!({
            "taskId": it.task_id,
            "checklistItemId": it.checklist_item_id,
        })).collect::<Vec<_>>(),
        "dependsOnGroupIds": wg.depends_on_group_ids,
    })
}

fn plan_parse_error_snapshot(plan_kind: WsPlanKind, plan_key: &str, err: &str) -> Value {
    let task = if matches!(plan_kind, WsPlanKind::Cleanse) {
        json!({
            "taskKind": "cleanse",
            "taskId": "parse_error",
            "dataset_id": "parse_error",
            "status": "needs_update",
            "checklist": [{
                "checklistItemId": "parse_error",
                "label": "Plan failed to deserialize",
                "details": err.to_string(),
                "status": "needs_update",
                "origin": "initial"
            }]
        })
    } else {
        json!({
            "taskKind": "model",
            "taskId": "parse_error",
            "name": "parse_error",
            "status": "needs_update",
            "checklist": [{
                "checklistItemId": "parse_error",
                "label": "Plan failed to deserialize",
                "details": err.to_string(),
                "status": "needs_update",
                "origin": "initial"
            }]
        })
    };
    json!({
        "planKind": plan_kind.as_str(),
        "planKey": plan_key,
        "status": "cancelled",
        "tasks": [task],
        "workGroups": [],
        "projectSnapshot": { "parse_error": err.to_string() }
    })
}

fn cleanse_plan_to_value(p: de_plan::CleansePlan) -> Value {
    json!({
        "planKind": "cleanse",
        "planKey": p.plan_key,
        "status": map_plan_status(p.status),
        "tasks": p.tasks.into_iter().map(|t| {
            json!({
                "taskKind": "cleanse",
                "taskId": t.dataset_id,
                "dataset_id": t.dataset_id,
                "expected_model_path": t.expected_model_path,
                "invariants": if t.invariants.is_empty() { None } else { Some(t.invariants) },
                "status": map_task_status(t.status),
                "checklist": t.checklist.into_iter().map(checklist_item_to_value).collect::<Vec<_>>()
            })
        }).collect::<Vec<_>>(),
        "workGroups": p.work_groups.into_iter().map(work_group_to_value).collect::<Vec<_>>(),
        "projectSnapshot": p.project_snapshot,
    })
}

fn model_plan_to_value(p: de_plan::ModelPlan) -> Value {
    json!({
        "planKind": "model",
        "planKey": p.plan_key,
        "status": map_plan_status(p.status),
        "tasks": p.tasks.into_iter().map(|t| {
            json!({
                "taskKind": "model",
                "taskId": t.name,
                "name": t.name,
                "folder": if t.folder.trim().is_empty() { None } else { Some(t.folder) },
                "goal": if t.goal.trim().is_empty() { None } else { Some(t.goal) },
                "inputs": if t.inputs.is_empty() { None } else { Some(t.inputs) },
                "expected_model_path": t.expected_model_path,
                "invariants": if t.invariants.is_empty() { None } else { Some(t.invariants) },
                "status": map_task_status(t.status),
                "checklist": t.checklist.into_iter().map(checklist_item_to_value).collect::<Vec<_>>()
            })
        }).collect::<Vec<_>>(),
        "workGroups": p.work_groups.into_iter().map(work_group_to_value).collect::<Vec<_>>(),
        "projectSnapshot": p.project_snapshot,
    })
}

pub async fn load_latest_plans_ws(
    ctx: &SuiteCtx,
    thread_id: &str,
) -> Vec<Value> {
    let base = ctx
        .keyspace
        .threads_prefix(&ctx.scope)
        .trim_end_matches("/threads")
        .trim_end_matches('/')
        .to_string();
    let pref = format!("{}/plans/{}/", base, thread_id.trim());
    let mut keys = ctx.storage.list_prefix(&pref).await.unwrap_or_default();
    keys.sort();

    let mut cleanse_active: Option<Value> = None;
    let mut newest_terminal_cleanse: Option<de_plan::CleansePlan> = None;
    for k in keys.iter().filter(|k| k.ends_with("_cleanse.json")) {
        if let Ok(bytes) = ctx.storage.get_bytes(k).await {
            match serde_json::from_slice::<de_plan::CleansePlan>(&bytes) {
                Ok(mut p) => {
                    if p.plan_key.trim().is_empty() {
                        p.plan_key = k.to_string();
                    }
                    if !p.status.is_terminal() {
                        cleanse_active = Some(cleanse_plan_to_value(p));
                        break;
                    } else {
                        newest_terminal_cleanse = Some(p);
                    }
                }
                Err(e) => {
                    cleanse_active = Some(plan_parse_error_snapshot(
                        WsPlanKind::Cleanse,
                        k,
                        &format!("failed to parse cleanse plan JSON at {}: {}", k, e),
                    ));
                    break;
                }
            }
        }
    }

    let cleanse = if let Some(s) = cleanse_active {
        Some(s)
    } else {
        newest_terminal_cleanse.map(cleanse_plan_to_value)
    };

    let mut model_active: Option<Value> = None;
    let mut newest_terminal_model: Option<de_plan::ModelPlan> = None;
    for k in keys.iter().filter(|k| k.ends_with("_model.json")) {
        if let Ok(bytes) = ctx.storage.get_bytes(k).await {
            match serde_json::from_slice::<de_plan::ModelPlan>(&bytes) {
                Ok(mut p) => {
                    if p.plan_key.trim().is_empty() {
                        p.plan_key = k.to_string();
                    }
                    if !p.status.is_terminal() {
                        model_active = Some(model_plan_to_value(p));
                        break;
                    } else {
                        newest_terminal_model = Some(p);
                    }
                }
                Err(e) => {
                    model_active = Some(plan_parse_error_snapshot(
                        WsPlanKind::Model,
                        k,
                        &format!("failed to parse model plan JSON at {}: {}", k, e),
                    ));
                    break;
                }
            }
        }
    }

    let model = if let Some(s) = model_active {
        Some(s)
    } else {
        newest_terminal_model.map(model_plan_to_value)
    };

    [cleanse, model].into_iter().flatten().collect()
}
