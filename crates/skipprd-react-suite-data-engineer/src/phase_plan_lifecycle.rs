use react_core::agent::AgentCtx;
use react_core::suite::SuiteCtx;

use crate::plan;
use crate::plan_types::{CleansePlan, ModelPlan, PlanStatus, TrackPlan};
use crate::track_spec::TrackKind;

/// Enum wrapper allowing a single variable to hold either plan type while
/// still exposing the `TrackPlan` trait. Pattern matching on the variants
/// is used by callers that need access to the concrete plan (e.g. phase_plan,
/// plan_review_helpers). This wrapper is the price of having two distinct
/// task types; the macro below keeps the delegation boilerplate-free.
#[derive(Clone)]
pub(super) enum TrackPlanDoc {
    Cleanse(CleansePlan),
    Model(ModelPlan),
}

macro_rules! delegate_track_plan {
    ($self:ident, $method:ident $(, $arg:ident : $ty:ty)*) => {
        match $self {
            Self::Cleanse(p) => p.$method($($arg),*),
            Self::Model(p) => p.$method($($arg),*),
        }
    };
}

impl TrackPlan for TrackPlanDoc {
    fn plan_key(&self) -> &str {
        delegate_track_plan!(self, plan_key)
    }
    fn status(&self) -> PlanStatus {
        delegate_track_plan!(self, status)
    }
    fn set_status(&mut self, status: PlanStatus) {
        delegate_track_plan!(self, set_status, status: PlanStatus)
    }
    fn tasks_len(&self) -> usize {
        delegate_track_plan!(self, tasks_len)
    }
    fn batches_len(&self) -> usize {
        delegate_track_plan!(self, batches_len)
    }
    fn progress_mut(&mut self) -> &mut crate::plan_types::PlanProgress {
        delegate_track_plan!(self, progress_mut)
    }
    fn executable_plan_issues(&self) -> Vec<String> {
        delegate_track_plan!(self, executable_plan_issues)
    }
}

pub(super) async fn load_plan_for_track(actx: &AgentCtx, track: TrackKind) -> Option<TrackPlanDoc> {
    match track {
        TrackKind::Cleanse => plan::load_cleanse_plan(actx)
            .await
            .ok()
            .flatten()
            .map(TrackPlanDoc::Cleanse),
        TrackKind::Model => plan::load_model_plan(actx)
            .await
            .ok()
            .flatten()
            .map(TrackPlanDoc::Model),
    }
}

pub(super) async fn save_plan(actx: &AgentCtx, plan: &TrackPlanDoc) -> Result<(), String> {
    match plan {
        TrackPlanDoc::Cleanse(plan) => crate::plan::save_cleanse_plan(actx, plan)
            .await
            .map_err(|e| e.to_string()),
        TrackPlanDoc::Model(plan) => crate::plan::save_model_plan(actx, plan)
            .await
            .map_err(|e| e.to_string()),
    }
}

/// Re-resolve `grounded_inputs[].source_schema` from the warehouse for every
/// model task in the active plan, then persist the updated plan.
///
/// Call after repair successfully validates — upstream column changes are
/// reflected in the warehouse views at that point, so the plan stays
/// authoritative without gold authoring ever seeing stale schemas.
pub(crate) async fn refresh_model_plan_grounded_schemas(sctx: &SuiteCtx, actx: &AgentCtx) {
    let query = match crate::ctx_ext::sctx_query(sctx) {
        Some(q) => q,
        None => return,
    };
    let mut plan = match crate::plan::load_model_plan(actx).await.ok().flatten() {
        Some(p) => p,
        None => return,
    };

    let relation_fqns: Vec<String> = plan
        .tasks
        .iter()
        .flat_map(|task| task.grounded_inputs.iter())
        .map(|input| input.relation_fqn.trim().to_string())
        .filter(|relation| !relation.is_empty())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let refreshed = refresh_relation_schemas(query, relation_fqns).await;
    let dirty = apply_refreshed_grounded_input_schemas(&mut plan, &refreshed);
    if dirty {
        if let Err(e) = crate::plan::save_model_plan(actx, &plan).await {
            tracing::warn!("failed to persist refreshed plan schemas: {e}");
        }
    }
}

async fn refresh_relation_schemas(
    query: std::sync::Arc<dyn crate::providers::QueryProvider>,
    relation_fqns: Vec<String>,
) -> std::collections::BTreeMap<String, Vec<crate::plan_types::SourceColumnDef>> {
    let concurrency = query
        .max_concurrency()
        .clamp(1, 8)
        .min(relation_fqns.len().max(1));
    let mut pending = relation_fqns.into_iter();
    let mut lookups = tokio::task::JoinSet::new();

    while lookups.len() < concurrency {
        let Some(relation_fqn) = pending.next() else {
            break;
        };
        spawn_relation_schema_lookup(&mut lookups, query.clone(), relation_fqn);
    }

    let mut refreshed = std::collections::BTreeMap::new();
    while let Some(joined) = lookups.join_next().await {
        let next = pending.next();
        match joined {
            Ok((relation_fqn, Some(schema))) => {
                refreshed.insert(relation_fqn, schema);
            }
            Ok((_relation_fqn, None)) => {}
            Err(e) => {
                tracing::debug!("refresh_model_plan_grounded_schemas: lookup task failed: {e}");
            }
        }
        if let Some(relation_fqn) = next {
            spawn_relation_schema_lookup(&mut lookups, query.clone(), relation_fqn);
        }
    }
    refreshed
}

fn spawn_relation_schema_lookup(
    lookups: &mut tokio::task::JoinSet<(String, Option<Vec<crate::plan_types::SourceColumnDef>>)>,
    query: std::sync::Arc<dyn crate::providers::QueryProvider>,
    relation_fqn: String,
) {
    lookups.spawn(async move {
        let schema = crate::facts::resolve_source_schema_live(query.as_ref(), &relation_fqn).await;
        (relation_fqn, schema)
    });
}

fn apply_refreshed_grounded_input_schemas(
    plan: &mut ModelPlan,
    refreshed: &std::collections::BTreeMap<String, Vec<crate::plan_types::SourceColumnDef>>,
) -> bool {
    let mut dirty = false;
    for task in plan.tasks.iter_mut() {
        let mut merged: Vec<crate::plan_types::SourceColumnDef> = Vec::new();
        for gi in task.grounded_inputs.iter_mut() {
            if let Some(fresh) = refreshed.get(gi.relation_fqn.trim()) {
                if !source_columns_equal(&gi.source_schema, fresh) {
                    gi.source_schema = fresh.clone();
                    dirty = true;
                }
            }
            merged.extend(gi.source_schema.iter().cloned());
        }
        if !merged.is_empty() && !source_columns_equal(&task.source_schema, &merged) {
            task.source_schema = merged;
            dirty = true;
        }
    }
    dirty
}

fn source_columns_equal(
    left: &[crate::plan_types::SourceColumnDef],
    right: &[crate::plan_types::SourceColumnDef],
) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right.iter())
            .all(|(l, r)| l.name == r.name && l.data_type == r.data_type)
}

#[cfg(test)]
mod tests {
    use super::apply_refreshed_grounded_input_schemas;
    use crate::plan_types::{
        GroundedModelInput, ModelFolder, ModelPlan, ModelTask, PlanStatus, SourceColumnDef,
        TaskStatus,
    };
    use std::collections::BTreeMap;

    fn col(name: &str, data_type: &str) -> SourceColumnDef {
        SourceColumnDef {
            name: name.to_string(),
            data_type: data_type.to_string(),
        }
    }

    #[test]
    fn refreshed_grounded_input_schema_overwrites_plan_contract_schema() {
        let mut plan = ModelPlan {
            plan_key: "model-plan.json".to_string(),
            status: PlanStatus::Approved,
            project_snapshot: Default::default(),
            tasks: vec![ModelTask {
                name: "agg_orders".to_string(),
                folder: ModelFolder::Marts,
                goal: String::new(),
                expected_model_path: None,
                invariants: vec![],
                inputs: vec!["fct_orders".to_string()],
                implementation_spec: None,
                source_schema: vec![col("total_amount", "number")],
                grounded_inputs: vec![GroundedModelInput {
                    input_name: "fct_orders".to_string(),
                    model_rel_path: "models/marts/fct_orders.sql".to_string(),
                    relation_fqn: "ANALYTICS.GOLD.FCT_ORDERS".to_string(),
                    source_schema: vec![col("total_amount", "number")],
                }],
                status: TaskStatus::Pending,
                checklist: vec![],
            }],
            batches: vec![vec!["agg_orders".to_string()]],
            work_groups: vec![],
            mutations: vec![],
            progress: Default::default(),
        };
        let refreshed = BTreeMap::from([(
            "ANALYTICS.GOLD.FCT_ORDERS".to_string(),
            vec![col("TOTAL_AMOUNT", "NUMBER")],
        )]);

        assert!(apply_refreshed_grounded_input_schemas(
            &mut plan, &refreshed
        ));
        assert_eq!(
            plan.tasks[0].grounded_inputs[0].source_schema[0].name,
            "TOTAL_AMOUNT"
        );
        assert_eq!(plan.tasks[0].source_schema[0].name, "TOTAL_AMOUNT");
    }
}
