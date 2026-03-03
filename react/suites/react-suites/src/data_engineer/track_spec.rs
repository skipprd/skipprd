use crate::data_engineer::control_flow::Phase;
use crate::data_engineer::plan_kind::PlanKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackKind {
    Cleanse,
    Model,
}

impl TrackKind {
    pub fn try_from_plan_phase(phase: Phase) -> Result<Self, String> {
        match phase {
            Phase::CleansePlan => Ok(Self::Cleanse),
            Phase::ModelPlan => Ok(Self::Model),
            other => Err(format!(
                "expected plan phase for TrackKind resolution, got '{}'",
                other.as_str()
            )),
        }
    }

    pub fn is_cleanse(self) -> bool {
        matches!(self, Self::Cleanse)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cleanse => "cleanse",
            Self::Model => "model",
        }
    }

    pub fn plan_kind(self) -> PlanKind {
        match self {
            Self::Cleanse => PlanKind::Cleanse,
            Self::Model => PlanKind::Model,
        }
    }

    pub fn execution_plan_kind(self) -> react_core::session::ExecutionPlanKind {
        match self {
            Self::Cleanse => react_core::session::ExecutionPlanKind::new("cleanse"),
            Self::Model => react_core::session::ExecutionPlanKind::new("model"),
        }
    }

    pub fn author_phase(self) -> Phase {
        match self {
            Self::Cleanse => Phase::CleanseAuthor,
            Self::Model => Phase::ModelAuthor,
        }
    }

    pub fn plan_phase(self) -> Phase {
        match self {
            Self::Cleanse => Phase::CleansePlan,
            Self::Model => Phase::ModelPlan,
        }
    }

    pub fn validate_phase(self) -> Phase {
        match self {
            Self::Cleanse => Phase::CleanseValidate,
            Self::Model => Phase::ModelValidate,
        }
    }
}

pub trait TrackSpec {
    type Plan;
    type TaskId;

    const KIND: TrackKind;
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CleanseDatasetId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ModelItemName(pub String);

pub struct CleanseSpec;
pub struct ModelSpec;

impl TrackSpec for CleanseSpec {
    type Plan = crate::data_engineer::plan::CleansePlan;
    type TaskId = CleanseDatasetId;

    const KIND: TrackKind = TrackKind::Cleanse;
}

impl TrackSpec for ModelSpec {
    type Plan = crate::data_engineer::plan::ModelPlan;
    type TaskId = ModelItemName;

    const KIND: TrackKind = TrackKind::Model;
}
