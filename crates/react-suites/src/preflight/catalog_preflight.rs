#[derive(Clone, Debug, Default)]
pub struct PreflightOutcome {}

pub async fn run_preflight_on_bundle(_thread_id: &str, _agent_type: &str) -> PreflightOutcome {
    // The previous implementation wrote standard thread steps for intent/decision.
    // We'll reintroduce that here once suites start depending on preflight decisions again.
    PreflightOutcome {}
}
