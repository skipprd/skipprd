use std::sync::Arc;

use crate::llm::router::LlmRouter;
use crate::llm::thread_ctx;
use crate::llm::types::ChatResponseFormat;
use crate::llm::{create_llm, ChatMessage, LargeLanguageModel, LlmConfig};
use react_core::llm::{LlmCallOptions, LlmExpectedFormat, ReasoningEffort};

fn openai_schema_name(s: &str) -> String {
    // OpenAI Responses `text.format.name` requires: ^[a-zA-Z0-9_-]+$
    // Our schema IDs include dots; sanitize to a stable provider-safe name.
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "schema".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_schema_name_sanitizes_dots_and_weird_chars() {
        assert_eq!(openai_schema_name("agent.step.v1"), "agent_step_v1");
        assert_eq!(openai_schema_name("patch_protocol/single-file@v1"), "patch_protocol_single-file_v1");
    }
}

/// Lightweight session wrapper around the configured LLM.
/// For local llama.cpp this will reuse the shared model underneath; for HTTP it reuses the HTTP client.
#[derive(Clone)]
pub struct LlmSession {
    llm: Arc<dyn LargeLanguageModel>,
}

impl LlmSession {
    pub fn new(cfg: &LlmConfig) -> Self {
        Self {
            llm: create_llm(cfg),
        }
    }

    /// Run a strict JSON prompt with a conservative token cap enforced in the backend.
    /// Returns the raw model text; callers should parse JSON strictly.
    pub fn chat_strict(&self, prompt: &str) -> Result<String, String> {
        self.llm.chat(
            &[ChatMessage {
            role: "user".into(),
            content: prompt.into(),
        }],
            &LlmCallOptions {
                prompt_id: "react.session.chat_strict",
                thread_id: None,
                expected_format: LlmExpectedFormat::JsonObject,
                max_output_tokens: None,
                temperature: None,
                top_p: None,
                reasoning_effort: None,
            },
        )
    }
}

/// Router-backed model to adapt the existing LargeLanguageModel interface
pub struct RouterModel {
    router: LlmRouter,
}

impl RouterModel {
    pub fn new() -> Self {
        Self {
            router: LlmRouter::new(),
        }
    }
}

impl LargeLanguageModel for RouterModel {
    fn chat(&self, messages: &[ChatMessage], options: &LlmCallOptions) -> Result<String, String> {
        let model = crate::helpers::configuration::Config::llm_chat_model()
            .unwrap_or_else(|| "gpt-4o-mini".to_string());

        let default_max_output_tokens = crate::helpers::configuration::Config::getenv(
            "LLM_MAX_TOKENS",
            "1024",
        )
        .parse()
        .ok();
        let default_temperature = crate::helpers::configuration::Config::getenv("LLM_TEMPERATURE", "0.2")
            .parse()
            .ok();
        let default_top_p = crate::helpers::configuration::Config::getenv("LLM_TOP_P", "1.0")
            .parse()
            .ok();

        let max_output_tokens = options.max_output_tokens.or(default_max_output_tokens);
        let temperature = options.temperature.or(default_temperature);
        let top_p = options.top_p.or(default_top_p);
        let reasoning_effort = match options.reasoning_effort.unwrap_or(ReasoningEffort::Low) {
            ReasoningEffort::None => Some("none".to_string()),
            ReasoningEffort::Low => Some("low".to_string()),
            ReasoningEffort::Medium => Some("medium".to_string()),
            ReasoningEffort::High => Some("high".to_string()),
        };
        let prompt_id = Some(options.prompt_id.to_string());

        let req = crate::llm::types::ChatRequest {
            model,
            messages: messages
                .iter()
                .map(|m| crate::llm::types::ChatMessage {
                    role: m.role.clone(),
                    content: m.content.clone(),
                })
                .collect(),
            max_output_tokens,
            temperature,
            top_p,
            response_format: match options.expected_format {
                LlmExpectedFormat::Text => None,
                LlmExpectedFormat::JsonObject => Some(ChatResponseFormat::JsonObject),
                LlmExpectedFormat::JsonSchema(id) => Some(ChatResponseFormat::JsonSchema {
                    name: openai_schema_name(id.name()),
                    schema: react_core::schema_registry::json_schema(id),
                    strict: true,
                }),
            },
            reasoning_effort,
            prompt_id,
            thread_id: options
                .thread_id
                .clone()
                .or_else(|| thread_ctx::current_thread_id()),
        };
        let r = self.router.chat(&req)?;
        Ok(r.text)
    }
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let model = crate::helpers::configuration::Config::llm_embed_model()
            .unwrap_or_else(|| "text-embedding-3-small".to_string());
        let req = crate::llm::types::EmbedRequest {
            model,
            inputs: texts.to_vec(),
        };
        let r = self.router.embed(&req)?;
        Ok(r.vectors)
    }
}
