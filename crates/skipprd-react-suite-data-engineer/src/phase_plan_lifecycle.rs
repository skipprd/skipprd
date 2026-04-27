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
    let mut dirty = false;
    for task in plan.tasks.iter_mut() {
        let mut merged: Vec<crate::plan_types::SourceColumnDef> = Vec::new();
        for gi in task.grounded_inputs.iter_mut() {
            if gi.relation_fqn.trim().is_empty() {
                continue;
            }
            if let Some(fresh) =
                crate::facts::resolve_source_schema_live(query.as_ref(), &gi.relation_fqn).await
            {
                gi.source_schema = fresh;
                dirty = true;
            }
            merged.extend(gi.source_schema.iter().cloned());
        }
        if !merged.is_empty() {
            task.source_schema = merged;
        }
    }
    if dirty {
        if let Err(e) = crate::plan::save_model_plan(actx, &plan).await {
            tracing::warn!("failed to persist refreshed plan schemas: {e}");
        }
    }
}
