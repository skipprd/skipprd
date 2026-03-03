use react_core::agent::AgentCtx;

use crate::data_engineer::plan;
use crate::data_engineer::plan_types::{CleansePlan, ModelPlan, PlanStatus};
use crate::data_engineer::track_spec::{TrackKind, TrackSpec};

#[derive(Clone)]
pub(super) enum TrackPlanDoc {
    Cleanse(CleansePlan),
    Model(ModelPlan),
}

impl TrackPlanDoc {
    pub(super) fn status(&self) -> PlanStatus {
        match self {
            Self::Cleanse(plan) => plan.status,
            Self::Model(plan) => plan.status,
        }
    }

    pub(super) fn set_status(&mut self, status: PlanStatus) {
        match self {
            Self::Cleanse(plan) => plan.status = status,
            Self::Model(plan) => plan.status = status,
        }
    }

    pub(super) fn plan_key(&self) -> &str {
        match self {
            Self::Cleanse(plan) => &plan.plan_key,
            Self::Model(plan) => &plan.plan_key,
        }
    }

    pub(super) fn tasks_len(&self) -> usize {
        match self {
            Self::Cleanse(plan) => plan.tasks.len(),
            Self::Model(plan) => plan.tasks.len(),
        }
    }

    pub(super) fn batches_len(&self) -> usize {
        match self {
            Self::Cleanse(plan) => plan.batches.len(),
            Self::Model(plan) => plan.batches.len(),
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.tasks_len() == 0 || self.batches_len() == 0
    }
}

pub(super) async fn load_active_plan(
    actx: &AgentCtx,
    track: TrackKind,
) -> Option<TrackPlanDoc> {
    match track {
        TrackKind::Cleanse => plan::load_cleanse_plan(actx).await.map(TrackPlanDoc::Cleanse),
        TrackKind::Model => plan::load_model_plan(actx).await.map(TrackPlanDoc::Model),
    }
}

pub(super) async fn load_active_plan_for_spec<S: TrackSpec>(
    actx: &AgentCtx,
) -> Option<TrackPlanDoc> {
    load_active_plan(actx, S::KIND).await
}

pub(super) async fn load_any_plan(actx: &AgentCtx, track: TrackKind) -> Option<TrackPlanDoc> {
    match track {
        TrackKind::Cleanse => plan::load_cleanse_plan_any(actx).await.map(TrackPlanDoc::Cleanse),
        TrackKind::Model => plan::load_model_plan_any(actx).await.map(TrackPlanDoc::Model),
    }
}

pub(super) async fn load_any_plan_for_spec<S: TrackSpec>(actx: &AgentCtx) -> Option<TrackPlanDoc> {
    load_any_plan(actx, S::KIND).await
}

pub(super) async fn save_plan(actx: &AgentCtx, plan: &TrackPlanDoc) -> Result<(), String> {
    match plan {
        TrackPlanDoc::Cleanse(plan) => crate::data_engineer::plan::save_cleanse_plan(actx, plan).await,
        TrackPlanDoc::Model(plan) => crate::data_engineer::plan::save_model_plan(actx, plan).await,
    }
}
