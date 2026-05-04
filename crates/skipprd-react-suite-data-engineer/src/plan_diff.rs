use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::plan_types::{ModelPlan, ModelTask};

pub(crate) fn model_task_contract_value(task: &ModelTask) -> Value {
    serde_json::json!({
        "name": task.name,
        "folder": task.folder,
        "goal": task.goal,
        "inputs": task.inputs,
        "expected_model_path": task.expected_model_path,
        "invariants": task.invariants,
        "implementation_spec": task.implementation_spec,
    })
}

fn model_contracts_by_task(plan: &ModelPlan) -> BTreeMap<String, Value> {
    plan.tasks
        .iter()
        .map(|task| {
            (
                task.name.trim().to_string(),
                model_task_contract_value(task),
            )
        })
        .collect()
}

pub(crate) fn guard_model_plan_amendment(
    before: &ModelPlan,
    after: &ModelPlan,
    allowed_task_ids: &BTreeSet<String>,
) -> Result<(), String> {
    let before_contracts = model_contracts_by_task(before);
    let after_contracts = model_contracts_by_task(after);
    let task_ids: BTreeSet<String> = before_contracts
        .keys()
        .chain(after_contracts.keys())
        .cloned()
        .collect();
    let mut violations = Vec::new();

    for task_id in task_ids {
        if allowed_task_ids.contains(&task_id) {
            continue;
        }
        if before_contracts.get(&task_id) != after_contracts.get(&task_id) {
            violations.push(task_id);
        }
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "model plan amendment changed unrelated task contract(s): {}",
            violations.join(", ")
        ))
    }
}

pub(crate) fn restore_unamended_model_task_contracts(
    before: &ModelPlan,
    after: &mut ModelPlan,
    allowed_task_ids: &BTreeSet<String>,
) {
    for task in after.tasks.iter_mut() {
        if allowed_task_ids.contains(task.name.trim()) {
            continue;
        }
        let Some(original) = before.tasks.iter().find(|old| old.name == task.name) else {
            continue;
        };
        task.folder = original.folder.clone();
        task.goal = original.goal.clone();
        task.inputs = original.inputs.clone();
        task.expected_model_path = original.expected_model_path.clone();
        task.invariants = original.invariants.clone();
        task.implementation_spec = original.implementation_spec.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan_progress::canonical_task_checklist;
    use crate::plan_types::{
        ModelFolder, ModelImplementationSpec, ModelPlan, ModelTask, PlanProgress, PlanSnapshot,
        PlanStatus, TaskStatus,
    };
    use crate::track_spec::TrackKind;

    fn task(name: &str, output_name: &str) -> ModelTask {
        ModelTask {
            name: name.to_string(),
            folder: ModelFolder::Marts,
            goal: format!("Build {name}"),
            inputs: vec!["stg_orders".to_string()],
            expected_model_path: Some(format!("models/marts/{name}.sql")),
            invariants: vec![],
            implementation_spec: Some(ModelImplementationSpec {
                spec_version: 1,
                grain: "1 row per id".to_string(),
                inputs: vec!["stg_orders".to_string()],
                joins: vec![],
                metrics: vec![],
                output_fields: vec![crate::plan_types::OutputFieldSpec {
                    name: output_name.to_string(),
                    kind: crate::plan_types::FieldKind::Raw,
                    expression: output_name.to_string(),
                    source_columns: vec![output_name.to_string()],
                    data_type: None,
                    nullable: false,
                    description: None,
                }],
                assumptions: vec![],
                evidence_claim_refs: vec![],
            }),
            source_schema: vec![],
            grounded_inputs: vec![],
            status: TaskStatus::Pending,
            checklist: canonical_task_checklist(TrackKind::Model),
        }
    }

    fn plan(tasks: Vec<ModelTask>) -> ModelPlan {
        ModelPlan {
            plan_key: "plan".to_string(),
            status: PlanStatus::Approved,
            project_snapshot: PlanSnapshot::default(),
            tasks,
            batches: vec![vec!["dim_customers".to_string()]],
            work_groups: vec![],
            mutations: vec![],
            progress: PlanProgress::default(),
        }
    }

    #[test]
    fn amendment_rejects_unrelated_task_contract_change() {
        let before = plan(vec![
            task("dim_customers", "customer_id"),
            task("fct_orders", "order_id"),
        ]);
        let mut after = before.clone();
        after.tasks[1].goal = "Different goal".to_string();
        let allowed = BTreeSet::from(["dim_customers".to_string()]);

        let err = guard_model_plan_amendment(&before, &after, &allowed).unwrap_err();

        assert!(err.contains("fct_orders"));
    }
}
