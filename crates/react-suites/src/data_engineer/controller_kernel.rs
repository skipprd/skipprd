use crate::data_engineer::retry_budget;
use crate::data_engineer::{plan, retry_budget::RetryBudget};

#[derive(Clone, Copy, Debug)]
pub enum PlanTrack {
    Cleanse,
    Model,
}

impl PlanTrack {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cleanse => "cleanse",
            Self::Model => "model",
        }
    }
}

pub fn build_batch_lock_prompt(
    track: PlanTrack,
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
        s.push_str(
            "- Apply a targeted mutating fix (`file op=patch|rm|mv`) to the failing artifact(s):\n",
        );
        for p in expected_paths.iter().take(6) {
            s.push_str("  - ");
            s.push_str(p);
            s.push('\n');
        }
        s.push_str(
            "\nThen retry. This lock exists to prevent infinite loops when batch application repeatedly fails.\n",
        );
    } else {
        s.push_str(
            "\nRecommended next step:\n- Apply a targeted mutating fix (`file op=patch|rm|mv`) to the failing DBT artifact(s), then retry.\n",
        );
    }
    s
}

pub fn subjective_retry_limit() -> usize {
    retry_budget::subjective_retry_limit()
}

pub fn subjective_retry_state_cap() -> usize {
    retry_budget::subjective_retry_state_cap()
}

pub fn batch_budget(progress: &plan::PlanProgress) -> RetryBudget {
    retry_budget::batch_budget(progress)
}

pub fn note_batch_result(progress: &mut plan::PlanProgress, ok: bool) -> RetryBudget {
    retry_budget::note_batch_result(progress, ok)
}

pub fn max_consecutive_batch_failures() -> usize {
    retry_budget::MAX_CONSECUTIVE_BATCH_FAILURES
}
