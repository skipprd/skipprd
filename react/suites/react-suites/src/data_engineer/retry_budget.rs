use crate::data_engineer::plan;
use crate::data_engineer::progress_controller::BatchFailureKind;

pub const MAX_CONSECUTIVE_BATCH_FAILURES: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryBudget {
    pub used: usize,
    pub limit: usize,
}

impl RetryBudget {
    pub fn exhausted(self) -> bool {
        self.used >= self.limit
    }
}

pub fn subjective_retry_limit() -> usize {
    std::env::var("AGENT_MAX_SUBJECTIVE_RETRIES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(2)
        .max(1)
        .min(6)
}

pub fn subjective_retry_state_cap() -> usize {
    // ExecutionState stores the first attempt as `1`, while callers compare with `> limit`.
    // Keep the +1 cap so persisted counters and guard behavior stay deterministic.
    subjective_retry_limit().saturating_add(1)
}

pub fn batch_budget(progress: &plan::PlanProgress) -> RetryBudget {
    RetryBudget {
        used: progress.consecutive_batch_failures,
        limit: MAX_CONSECUTIVE_BATCH_FAILURES,
    }
}

pub fn note_batch_result(progress: &mut plan::PlanProgress, ok: bool) -> RetryBudget {
    if ok {
        progress.consecutive_batch_failures = 0;
    } else {
        progress.consecutive_batch_failures = progress.consecutive_batch_failures.saturating_add(1);
        progress.total_batch_failures = progress.total_batch_failures.saturating_add(1);
    }
    batch_budget(progress)
}

pub fn note_batch_result_with_failure_kind(
    progress: &mut plan::PlanProgress,
    ok: bool,
    failure_kind: Option<BatchFailureKind>,
) -> RetryBudget {
    if ok {
        return note_batch_result(progress, true);
    }
    if matches!(failure_kind, Some(BatchFailureKind::InfraTransient)) {
        // Transient upstream outages must not consume deterministic batch-lock budget.
        return batch_budget(progress);
    }
    note_batch_result(progress, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_batch_result_updates_consecutive_and_total() {
        let mut progress = plan::PlanProgress::default();
        let b1 = note_batch_result(&mut progress, false);
        assert_eq!(b1.used, 1);
        assert_eq!(progress.total_batch_failures, 1);

        let b2 = note_batch_result(&mut progress, false);
        assert_eq!(b2.used, 2);
        assert_eq!(progress.total_batch_failures, 2);

        let b3 = note_batch_result(&mut progress, true);
        assert_eq!(b3.used, 0);
        assert_eq!(progress.total_batch_failures, 2);
    }

    #[test]
    fn infra_transient_failure_does_not_consume_budget() {
        let mut progress = plan::PlanProgress::default();
        let budget = note_batch_result_with_failure_kind(
            &mut progress,
            false,
            Some(BatchFailureKind::InfraTransient),
        );
        assert_eq!(budget.used, 0);
        assert_eq!(progress.total_batch_failures, 0);
    }
}
