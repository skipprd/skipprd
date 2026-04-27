use react_core::llm::LlmCallOptions;

use super::DataEngineerSuite;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum PlanningLlmProfile {
    DiscoveryCleanse,
    DiscoveryModel,
    DesignMemo,
    DesignCritique,
    SkeletonOrCandidates,
    EnrichmentCompile,
    EnrichmentReason,
}

impl DataEngineerSuite {
    pub(super) fn planning_llm_options(
        profile: PlanningLlmProfile,
        prompt_id: &'static str,
        thread_id: Option<String>,
    ) -> Result<LlmCallOptions, String> {
        Ok(match profile {
            PlanningLlmProfile::DiscoveryCleanse => {
                let max_tokens =
                    std::env::var(super::env_util::env_keys::LLM_PLAN_MAX_TOKENS_CLEANSE)
                        .ok()
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(96_000)
                        .max(4_000);
                let reasoning_effort = Self::parse_reasoning_effort_env(
                    super::env_util::env_keys::LLM_PLAN_REASONING_EFFORT_CLEANSE,
                )
                .or_else(|| {
                    Self::parse_reasoning_effort_env(
                        super::env_util::env_keys::LLM_PLAN_REASONING_EFFORT,
                    )
                })
                .unwrap_or(react_core::llm::ReasoningEffort::Medium);
                LlmCallOptions {
                    prompt_id,
                    thread_id,
                    expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
                    max_output_tokens: Some(max_tokens),
                    reasoning_effort: Some(reasoning_effort),
                    ..Default::default()
                }
            }
            PlanningLlmProfile::DiscoveryModel => {
                let max_tokens =
                    std::env::var(super::env_util::env_keys::LLM_PLAN_MAX_TOKENS_MODEL)
                        .ok()
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(128_000)
                        .max(8_000);
                let reasoning_effort = Self::parse_reasoning_effort_env(
                    super::env_util::env_keys::LLM_PLAN_REASONING_EFFORT_MODEL,
                )
                .or_else(|| {
                    Self::parse_reasoning_effort_env(
                        super::env_util::env_keys::LLM_PLAN_REASONING_EFFORT,
                    )
                })
                .unwrap_or(react_core::llm::ReasoningEffort::Medium);
                LlmCallOptions {
                    prompt_id,
                    thread_id,
                    expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
                    max_output_tokens: Some(max_tokens),
                    reasoning_effort: Some(reasoning_effort),
                    ..Default::default()
                }
            }
            PlanningLlmProfile::DesignMemo => {
                let max_tokens = std::env::var(super::env_util::env_keys::LLM_PLAN_MEMO_MAX_TOKENS)
                    .ok()
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(64_000)
                    .max(8_000);
                let reasoning_effort = Self::parse_reasoning_effort_env(
                    super::env_util::env_keys::LLM_PLAN_REASONING_EFFORT,
                )
                .unwrap_or(react_core::llm::ReasoningEffort::Medium);
                LlmCallOptions {
                    prompt_id,
                    thread_id,
                    expected_format: react_core::llm::LlmExpectedFormat::Text,
                    max_output_tokens: Some(max_tokens),
                    reasoning_effort: Some(reasoning_effort),
                    ..Default::default()
                }
            }
            PlanningLlmProfile::DesignCritique => {
                let max_tokens =
                    std::env::var(super::env_util::env_keys::LLM_PLAN_CRITIQUE_MAX_TOKENS)
                        .ok()
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(16_000)
                        .max(4_000);
                LlmCallOptions {
                    prompt_id,
                    thread_id,
                    expected_format: react_core::llm::LlmExpectedFormat::JsonSchema(
                        react_core::schema_registry::OpenAiStrictSchema::for_type::<
                            crate::plan_schema::PlanDesignCritiqueV1,
                        >("suite.plan_design_critique.v1")
                        .map_err(|e| e.to_string())?,
                    ),
                    max_output_tokens: Some(max_tokens),
                    reasoning_effort: Some(react_core::llm::ReasoningEffort::Low),
                    ..Default::default()
                }
            }
            PlanningLlmProfile::SkeletonOrCandidates => {
                let max_tokens =
                    std::env::var(super::env_util::env_keys::LLM_PLAN_SKELETON_MAX_TOKENS)
                        .ok()
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(24_000)
                        .max(6_000);
                LlmCallOptions {
                    prompt_id,
                    thread_id,
                    expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
                    max_output_tokens: Some(max_tokens),
                    reasoning_effort: Some(react_core::llm::ReasoningEffort::Low),
                    ..Default::default()
                }
            }
            PlanningLlmProfile::EnrichmentCompile => {
                let max_tokens =
                    std::env::var(super::env_util::env_keys::LLM_PLAN_ENRICH_MAX_TOKENS)
                        .ok()
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(32_000)
                        .max(6_000);
                LlmCallOptions {
                    prompt_id,
                    thread_id,
                    expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
                    max_output_tokens: Some(max_tokens),
                    reasoning_effort: Some(react_core::llm::ReasoningEffort::Low),
                    ..Default::default()
                }
            }
            PlanningLlmProfile::EnrichmentReason => {
                let max_tokens =
                    std::env::var(super::env_util::env_keys::LLM_PLAN_ENRICH_REASON_MAX_TOKENS)
                        .ok()
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(8_000)
                        .max(2_000);
                LlmCallOptions {
                    prompt_id,
                    thread_id,
                    expected_format: react_core::llm::LlmExpectedFormat::Text,
                    max_output_tokens: Some(max_tokens),
                    reasoning_effort: Some(react_core::llm::ReasoningEffort::Low),
                    ..Default::default()
                }
            }
        })
    }

    pub(super) fn parse_reasoning_effort_env(
        var: &str,
    ) -> Option<react_core::llm::ReasoningEffort> {
        crate::env_util::parse_reasoning_effort_env(var)
    }
}
