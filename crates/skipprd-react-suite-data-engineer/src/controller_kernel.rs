use crate::retry_budget;
use crate::{plan, retry_budget::RetryBudget};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardReason {
    MutationRequiredAfterValidateFailure,
    ProbeRequiredAfterRuntimeFailure,
}

impl GuardReason {
    pub fn code(self) -> &'static str {
        match self {
            Self::MutationRequiredAfterValidateFailure => {
                "mutation_required_after_validate_failure"
            }
            Self::ProbeRequiredAfterRuntimeFailure => "probe_required_after_runtime_failure",
        }
    }

    pub fn user_message(self) -> &'static str {
        match self {
            Self::MutationRequiredAfterValidateFailure => {
                "dbt_validate is blocked after a failed validation until you APPLY A FIX to the dbt project.\n\
                 Next step must be a mutating fix action (e.g. `staging_model` or `file` with a mutation op to update schema/tests)."
            }
            Self::ProbeRequiredAfterRuntimeFailure => {
                "dbt_validate (build/run) is blocked after a runtime failure until you run meaningful SQL probes.\n\
                 Next step must include `run_sql` against the failing relation(s) (not `SELECT 1`) to diagnose data issues, then APPLY a fix."
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchLockReason {
    ConsecutiveFailureBudgetExhausted,
}

impl BatchLockReason {
    pub fn message(self) -> &'static str {
        match self {
            Self::ConsecutiveFailureBudgetExhausted => {
                "too many consecutive batch failures; apply a targeted mutating fix (file mutation op) before retrying"
            }
        }
    }
}

pub fn guard_block_error(reason: GuardReason) -> String {
    format!("guard_block:{}: {}", reason.code(), reason.user_message())
}

pub fn batch_lock_error_message(reason: BatchLockReason) -> &'static str {
    reason.message()
}

pub fn build_batch_lock_prompt(
    track: crate::track_spec::TrackKind,
    plan_key: &str,
    consecutive: usize,
    total: usize,
    next_items: &[String],
    expected_paths: &[String],
) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "Plan-batched authoring is locked ({kind}).\n\n\
Reason:\n\
- consecutive_batch_failures = {consecutive} (limit {})\n\
- total_batch_failures = {total}\n\n\
Plan:\n\
- plan_key: {plan_key}\n",
        retry_budget::MAX_CONSECUTIVE_BATCH_FAILURES,
        kind = track.as_str(),
    ));
    if !next_items.is_empty() {
        s.push_str("\nNext batch items:\n");
        for it in next_items.iter().take(6) {
            s.push_str("- ");
            s.push_str(it);
            s.push('\n');
        }
    }
    if !expected_paths.is_empty() {
        s.push_str("\nRecommended next step:\n");
        s.push_str(&format!(
            "- Apply a targeted mutating fix (`file {}`) to the failing artifact(s):\n",
            crate::tool_ops::general_mutation_ops_label(),
        ));
        for p in expected_paths.iter().take(6) {
            s.push_str("  - ");
            s.push_str(p);
            s.push('\n');
        }
        s.push_str(
            "\nThen retry. This lock exists to prevent infinite loops when batch application repeatedly fails.\n",
        );
    } else {
        s.push_str(&format!(
            "\nRecommended next step:\n- Apply a targeted mutating fix (`file {}`) to the failing DBT artifact(s), then retry.\n",
            crate::tool_ops::general_mutation_ops_label(),
        ));
    }
    s
}

pub fn publish_retry_limit() -> usize {
    crate::env_util::max_publish_retries()
}

pub fn batch_budget(progress: &plan::PlanProgress) -> RetryBudget {
    retry_budget::batch_budget(progress)
}

pub fn note_batch_result(progress: &mut plan::PlanProgress, ok: bool) -> RetryBudget {
    retry_budget::note_batch_result(progress, ok)
}

pub fn note_batch_result_with_failure_kind(
    progress: &mut plan::PlanProgress,
    ok: bool,
    failure_kind: Option<crate::failure_kind::FailureKind>,
) -> RetryBudget {
    retry_budget::note_batch_result_with_failure_kind(progress, ok, failure_kind)
}

pub fn max_consecutive_batch_failures() -> usize {
    retry_budget::MAX_CONSECUTIVE_BATCH_FAILURES
}

#[cfg(test)]
pub(crate) fn batch_lock_reason_message(reason: BatchLockReason) -> &'static str {
    match reason {
        BatchLockReason::ConsecutiveFailureBudgetExhausted => reason.message(),
    }
}

#[cfg(test)]
mod tests {
    use super::{batch_lock_error_message, guard_block_error, BatchLockReason, GuardReason};

    #[test]
    fn guard_block_error_emits_canonical_reason_code() {
        let msg = guard_block_error(GuardReason::MutationRequiredAfterValidateFailure);
        assert!(msg.starts_with("guard_block:mutation_required_after_validate_failure:"));
    }

    #[test]
    fn batch_lock_error_message_is_stable() {
        let msg = batch_lock_error_message(BatchLockReason::ConsecutiveFailureBudgetExhausted);
        assert!(msg.contains("too many consecutive batch failures"));
    }
}
