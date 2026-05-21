use crate::plan_types::{CleansePlan, ModelPlan};

#[derive(Clone, Debug)]
pub(crate) enum PlanSemanticGateOutcome {
    Accepted(crate::plan::PlanSemanticValidation),
    PlanDefect(crate::plan::PlanSemanticValidation),
}

impl PlanSemanticGateOutcome {
    pub(crate) fn into_validation(self) -> crate::plan::PlanSemanticValidation {
        match self {
            Self::Accepted(validation) | Self::PlanDefect(validation) => validation,
        }
    }
}

pub(crate) fn gate_cleanse_plan(plan: &mut CleansePlan) -> PlanSemanticGateOutcome {
    let validation = crate::plan::ensure_cleanse_plan_semantically_valid_or_repaired(plan);
    if validation.ok {
        PlanSemanticGateOutcome::Accepted(validation)
    } else {
        PlanSemanticGateOutcome::PlanDefect(validation)
    }
}

pub(crate) fn gate_model_plan(
    plan: &mut ModelPlan,
    allowed_staging_models: &std::collections::BTreeSet<String>,
) -> PlanSemanticGateOutcome {
    let validation =
        crate::plan::ensure_model_plan_semantically_valid_or_repaired(plan, allowed_staging_models);
    if validation.ok {
        PlanSemanticGateOutcome::Accepted(validation)
    } else {
        PlanSemanticGateOutcome::PlanDefect(validation)
    }
}
