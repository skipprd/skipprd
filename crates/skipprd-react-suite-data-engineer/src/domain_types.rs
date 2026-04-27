use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Proceed,
    PatchImpl,
    PlanChange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewTier {
    Silver,
    Gold,
    Unknown,
}

impl Default for ReviewTier {
    fn default() -> Self {
        ReviewTier::Unknown
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewDecisionMeta {
    pub decision: ReviewDecision,
    #[serde(default)]
    pub tier: ReviewTier,
    #[serde(default)]
    pub dataset_ids: Vec<String>,
    #[serde(default)]
    pub review_ref: Option<ReviewArtifactRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewArtifactRef {
    pub key: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewSummaryOutput {
    #[serde(default)]
    pub project_notes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewBatchOutput {
    #[serde(default)]
    pub findings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewUnifyOutput {
    pub decision: ReviewDecision,
    #[serde(default)]
    pub tier: ReviewTier,
    #[serde(default)]
    pub dataset_ids: Vec<String>,
    pub final_review_text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardBlockKind {
    PlanJsonInvalid,
    PlanGrounding,
    PlanSemanticInvalid,
    PlanDesignCritique,
    BatchLocked,
    AuthoringCompletion,
    AuthoringToValidate,
    MissingGoldModels,
    PrecheckFailed,
    ValidateExecutionFailed,
    ValidateRetryExhausted,
    MissingThreadStep,
    PhaseExecutionError,
}

impl GuardBlockKind {
    pub fn as_str(self) -> &'static str {
        match self {
            GuardBlockKind::PlanJsonInvalid => "plan_json_invalid",
            GuardBlockKind::PlanGrounding => "plan_grounding",
            GuardBlockKind::PlanSemanticInvalid => "plan_semantic_invalid",
            GuardBlockKind::PlanDesignCritique => "plan_design_critique",
            GuardBlockKind::BatchLocked => "batch_locked",
            GuardBlockKind::AuthoringCompletion => "authoring_completion",
            GuardBlockKind::AuthoringToValidate => "authoring_to_validate",
            GuardBlockKind::MissingGoldModels => "missing_gold_models",
            GuardBlockKind::PrecheckFailed => "precheck_failed",
            GuardBlockKind::ValidateExecutionFailed => "validate_execution_failed",
            GuardBlockKind::ValidateRetryExhausted => "validate_retry_exhausted",
            GuardBlockKind::MissingThreadStep => "missing_thread_step",
            GuardBlockKind::PhaseExecutionError => "phase_execution_error",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ControllerEvent {
    ValidatePassed,
    ValidateFailed {
        brief: String,
        failure_hash: String,
        compile_ok: bool,
        run_ok: bool,
    },
    ValidateContractError {
        reason: String,
        brief: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidateOutcomeV2 {
    pub ok: bool,
    pub compile_ok: bool,
    pub run_ok: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ValidateObservationContract {
    pub observation: Value,
    pub outcome_v2: ValidateOutcomeV2,
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_GUARD_BLOCK_KINDS: [GuardBlockKind; 12] = [
        GuardBlockKind::PlanJsonInvalid,
        GuardBlockKind::PlanGrounding,
        GuardBlockKind::PlanSemanticInvalid,
        GuardBlockKind::PlanDesignCritique,
        GuardBlockKind::BatchLocked,
        GuardBlockKind::AuthoringCompletion,
        GuardBlockKind::AuthoringToValidate,
        GuardBlockKind::MissingGoldModels,
        GuardBlockKind::PrecheckFailed,
        GuardBlockKind::ValidateExecutionFailed,
        GuardBlockKind::MissingThreadStep,
        GuardBlockKind::PhaseExecutionError,
    ];

    #[test]
    fn guard_block_kind_as_str_matches_serde() {
        for kind in ALL_GUARD_BLOCK_KINDS {
            let serde_name = serde_json::to_value(kind)
                .unwrap()
                .as_str()
                .unwrap()
                .to_string();
            assert_eq!(kind.as_str(), serde_name, "as_str drift for {:?}", kind);
        }
    }
}
