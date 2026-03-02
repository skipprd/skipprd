use crate::data_engineer::control_flow::Phase;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuthoringKind {
    Cleanse,
    Model,
}

#[derive(Clone, Debug)]
pub(crate) struct AuthoringCtx {
    pub phase: Phase,
}

#[derive(Clone, Debug)]
pub(crate) enum AuthoringTurnResult {
    Continue,
    HardError { message: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuthoringToolPolicy {
    HardMutationSingleTarget,
    HardMutationSchemaRepair,
    HardMutationGeneric,
    BatchingCleanseSql,
    BatchingCleanseSchema,
    BatchingModelSql,
    BatchingModelSchema,
    GeneralAuthoring,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AuthoringToolPolicyInput {
    pub hard_mutation_only: bool,
    pub single_target_repair: bool,
    pub allow_probe_sql: bool,
    pub plan_batched_cleanse_sql: bool,
    pub plan_batched_cleanse_schema: bool,
    pub plan_batched_model_sql: bool,
    pub plan_batched_model_schema: bool,
}

pub(crate) fn derive_authoring_tool_policy(input: AuthoringToolPolicyInput) -> AuthoringToolPolicy {
    if input.hard_mutation_only {
        if input.single_target_repair {
            return AuthoringToolPolicy::HardMutationSingleTarget;
        }
        if input.allow_probe_sql {
            return AuthoringToolPolicy::HardMutationSchemaRepair;
        }
        return AuthoringToolPolicy::HardMutationGeneric;
    }
    if input.plan_batched_cleanse_sql {
        return AuthoringToolPolicy::BatchingCleanseSql;
    }
    if input.plan_batched_cleanse_schema {
        return AuthoringToolPolicy::BatchingCleanseSchema;
    }
    if input.plan_batched_model_sql {
        return AuthoringToolPolicy::BatchingModelSql;
    }
    if input.plan_batched_model_schema {
        return AuthoringToolPolicy::BatchingModelSchema;
    }
    AuthoringToolPolicy::GeneralAuthoring
}

pub(crate) trait AuthoringPlanAdapter {
    fn kind(&self) -> AuthoringKind;
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CleanseAdapter;

impl AuthoringPlanAdapter for CleanseAdapter {
    fn kind(&self) -> AuthoringKind {
        AuthoringKind::Cleanse
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ModelAdapter;

impl AuthoringPlanAdapter for ModelAdapter {
    fn kind(&self) -> AuthoringKind {
        AuthoringKind::Model
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum AuthoringAdapter {
    Cleanse(CleanseAdapter),
    Model(ModelAdapter),
}

impl AuthoringAdapter {
    pub(crate) fn kind(self) -> AuthoringKind {
        match self {
            Self::Cleanse(v) => v.kind(),
            Self::Model(v) => v.kind(),
        }
    }

}

pub(crate) fn adapter_for_phase(phase: Phase) -> Option<AuthoringAdapter> {
    match phase {
        Phase::CleanseAuthor => Some(AuthoringAdapter::Cleanse(CleanseAdapter)),
        Phase::ModelAuthor => Some(AuthoringAdapter::Model(ModelAdapter)),
        _ => None,
    }
}

pub(crate) struct AuthoringDriver;

impl AuthoringDriver {
    pub(crate) fn run_turn(
        ctx: &AuthoringCtx,
        snapshot: &crate::data_engineer::progress_controller::AuthoringProgressSnapshot,
    ) -> AuthoringTurnResult {
        Self::stepboundary_outcome(ctx, snapshot)
    }

    pub(crate) fn stepboundary_outcome(
        ctx: &AuthoringCtx,
        snapshot: &crate::data_engineer::progress_controller::AuthoringProgressSnapshot,
    ) -> AuthoringTurnResult {
        if !snapshot.progress_made
            && snapshot.reason
                == Some(
                    crate::data_engineer::progress_controller::AuthoringNoProgressReason::NoMutationObservedInHardRepair,
                )
        {
            return AuthoringTurnResult::HardError {
                message: format!(
                    "failed to make progress for this thread: no mutating file operation observed while hard_mutation_repair_mode=true and repair stall budget was exhausted (phase={}). Apply a direct file mutation (patch/rm/mv) to the failing model path before retrying.",
                    ctx.phase.as_str()
                ),
            };
        }
        AuthoringTurnResult::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_policy_is_parity_shaped_for_cleanse_and_model_schema_batches() {
        let cleanse = derive_authoring_tool_policy(AuthoringToolPolicyInput {
            plan_batched_cleanse_schema: true,
            ..Default::default()
        });
        let model = derive_authoring_tool_policy(AuthoringToolPolicyInput {
            plan_batched_model_schema: true,
            ..Default::default()
        });
        assert_eq!(cleanse, AuthoringToolPolicy::BatchingCleanseSchema);
        assert_eq!(model, AuthoringToolPolicy::BatchingModelSchema);
    }

    #[test]
    fn stepboundary_returns_hard_error_for_no_progress_in_hard_repair() {
        let ctx = AuthoringCtx {
            phase: Phase::ModelAuthor,
        };
        let snapshot = crate::data_engineer::progress_controller::AuthoringProgressSnapshot {
            progress_made: false,
            reason: Some(
                crate::data_engineer::progress_controller::AuthoringNoProgressReason::NoMutationObservedInHardRepair,
            ),
        };
        let out = AuthoringDriver::stepboundary_outcome(&ctx, &snapshot);
        assert!(matches!(out, AuthoringTurnResult::HardError { .. }));
    }
}
