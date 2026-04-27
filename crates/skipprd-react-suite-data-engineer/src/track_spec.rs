use crate::control_flow::Phase;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

    pub fn from_any_phase(phase: Phase) -> Option<Self> {
        match phase {
            Phase::CleansePlan
            | Phase::CleanseAuthor
            | Phase::CleanseValidate
            | Phase::CleanseReview => Some(Self::Cleanse),
            Phase::ModelPlan | Phase::ModelAuthor | Phase::ModelValidate | Phase::ModelReview => {
                Some(Self::Model)
            }
            _ => None,
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
