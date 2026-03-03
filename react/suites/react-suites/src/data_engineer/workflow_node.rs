use crate::data_engineer::control_flow::Phase;
use crate::data_engineer::progress_controller::ExecutionState;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkflowNode {
    Preflight,
    CleansePlan,
    CleanseAuthor,
    CleanseValidate,
    CleanseReview,
    ModelPlan,
    ModelAuthor,
    ModelValidate,
    ModelReview,
    PublishAwaitApproval,
    Publish,
    PostPublishReview,
    Done,
}

impl WorkflowNode {
    pub fn from_phase(phase: Phase) -> Self {
        match phase {
            Phase::Preflight => Self::Preflight,
            Phase::CleansePlan => Self::CleansePlan,
            Phase::CleanseAuthor => Self::CleanseAuthor,
            Phase::CleanseValidate => Self::CleanseValidate,
            Phase::CleanseReview => Self::CleanseReview,
            Phase::ModelPlan => Self::ModelPlan,
            Phase::ModelAuthor => Self::ModelAuthor,
            Phase::ModelValidate => Self::ModelValidate,
            Phase::ModelReview => Self::ModelReview,
            Phase::PublishAwaitApproval => Self::PublishAwaitApproval,
            Phase::Publish => Self::Publish,
            Phase::PostPublishReview => Self::PostPublishReview,
            Phase::Done => Self::Done,
        }
    }

    pub fn as_phase(self) -> Phase {
        match self {
            Self::Preflight => Phase::Preflight,
            Self::CleansePlan => Phase::CleansePlan,
            Self::CleanseAuthor => Phase::CleanseAuthor,
            Self::CleanseValidate => Phase::CleanseValidate,
            Self::CleanseReview => Phase::CleanseReview,
            Self::ModelPlan => Phase::ModelPlan,
            Self::ModelAuthor => Phase::ModelAuthor,
            Self::ModelValidate => Phase::ModelValidate,
            Self::ModelReview => Phase::ModelReview,
            Self::PublishAwaitApproval => Phase::PublishAwaitApproval,
            Self::Publish => Phase::Publish,
            Self::PostPublishReview => Phase::PostPublishReview,
            Self::Done => Phase::Done,
        }
    }

    pub fn from_state(state: &ExecutionState) -> Self {
        let phase = state.current_phase.unwrap_or(Phase::Preflight);
        Self::from_phase(phase)
    }
}
