use react_core::llm::{LlmCallOptions, LlmExpectedFormat};
use react_core::resolved_config::LlmResolved;

/// Routes LLM calls to the appropriate model with role-appropriate sampling parameters.
///
/// `reason_model` is for planning/diagnosis (higher capability, higher temp with retries).
/// `task_model` is for mechanical tool-calling (cheaper, low temperature).
/// If no `task_model` is configured, `reason_model` is used for both roles at low temperature.
#[derive(Clone, Debug)]
pub struct ModelDispatch {
    pub reason_model: String,
    pub task_model: String,
}

impl ModelDispatch {
    pub fn from_resolved(cfg: &LlmResolved) -> Self {
        let reason = cfg
            .reason_model
            .clone()
            .unwrap_or_else(|| "gpt-4o-mini".to_string());
        let task = cfg.task_model.clone().unwrap_or_else(|| reason.clone());
        Self {
            reason_model: reason,
            task_model: task,
        }
    }

    /// Build call options for the task (gather/tool-calling) model.
    pub fn task_call_options(&self, prompt_id: &'static str) -> LlmCallOptions {
        LlmCallOptions {
            prompt_id,
            model: Some(self.task_model.clone()),
            temperature: Some(0.1),
            expected_format: LlmExpectedFormat::Text,
            reasoning_effort: Some(react_core::llm::ReasoningEffort::Medium),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_model_falls_back_to_reason() {
        let cfg = LlmResolved {
            reason_model: Some("gpt-5.4".into()),
            task_model: None,
            ..Default::default()
        };
        let d = ModelDispatch::from_resolved(&cfg);
        assert_eq!(d.task_model, "gpt-5.4");
    }

    #[test]
    fn explicit_task_model_used() {
        let cfg = LlmResolved {
            reason_model: Some("gpt-5.4".into()),
            task_model: Some("gpt-4.1".into()),
            ..Default::default()
        };
        let d = ModelDispatch::from_resolved(&cfg);
        assert_eq!(d.task_model, "gpt-4.1");
        assert_eq!(d.reason_model, "gpt-5.4");
    }
}
