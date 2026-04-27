use serde_json::{json, Value};

use react_core::storage::{retry_get_bytes, retry_list_prefix};
use react_core::suite::SuiteCtx;

use super::plan as de_plan;
use crate::track_spec::TrackKind;

fn serde_str<T: serde::Serialize>(v: T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
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
        "status": serde_str(it.status),
        "origin": serde_str(it.origin),
        "evidence": evidence,
    })
}

fn work_group_to_value(wg: de_plan::PlanWorkGroup) -> Value {
    json!({
        "groupId": wg.group_id,
        "label": wg.label,
        "kind": serde_str(wg.kind),
        "items": wg.items.into_iter().map(|it| json!({
            "taskId": it.task_id,
            "checklistItemId": it.checklist_item_id,
        })).collect::<Vec<_>>(),
        "dependsOnGroupIds": wg.depends_on_group_ids,
    })
}

fn plan_parse_error_snapshot(plan_kind: TrackKind, plan_key: &str, err: &str) -> Value {
    let task = if plan_kind.is_cleanse() {
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
        "status": serde_str(p.status),
        "tasks": p.tasks.into_iter().map(|t| {
            json!({
                "taskKind": "cleanse",
                "taskId": t.dataset_id,
                "dataset_id": t.dataset_id,
                "expected_model_path": t.expected_model_path,
                "invariants": if t.invariants.is_empty() { None } else { Some(t.invariants) },
                "status": serde_str(t.status),
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
        "status": serde_str(p.status),
        "tasks": p.tasks.into_iter().map(|t| {
            json!({
                "taskKind": "model",
                "taskId": t.name,
                "name": t.name,
                "folder": t.folder.as_str(),
                "goal": if t.goal.trim().is_empty() { None } else { Some(t.goal) },
                "inputs": if t.inputs.is_empty() { None } else { Some(t.inputs) },
                "expected_model_path": t.expected_model_path,
                "invariants": if t.invariants.is_empty() { None } else { Some(t.invariants) },
                "status": serde_str(t.status),
                "checklist": t.checklist.into_iter().map(checklist_item_to_value).collect::<Vec<_>>()
            })
        }).collect::<Vec<_>>(),
        "workGroups": p.work_groups.into_iter().map(work_group_to_value).collect::<Vec<_>>(),
        "projectSnapshot": p.project_snapshot,
    })
}

pub async fn load_latest_plans_ws(ctx: &SuiteCtx, thread_id: &str) -> Vec<Value> {
    let base = ctx
        .keyspace()
        .threads_prefix(ctx.scope())
        .trim_end_matches("/threads")
        .trim_end_matches('/')
        .to_string();
    let pref = format!("{}/plans/{}/", base, thread_id.trim());
    let mut keys = retry_list_prefix(ctx.storage().as_ref(), &pref)
        .await
        .unwrap_or_default();
    keys.sort();

    let mut cleanse_active: Option<Value> = None;
    let mut newest_terminal_cleanse: Option<de_plan::CleansePlan> = None;
    for k in keys.iter().filter(|k| k.ends_with("_cleanse.json")) {
        if let Ok(bytes) = retry_get_bytes(ctx.storage().as_ref(), k).await {
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
                        TrackKind::Cleanse,
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
        if let Ok(bytes) = retry_get_bytes(ctx.storage().as_ref(), k).await {
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
                        TrackKind::Model,
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
