use std::sync::Arc;

use crate::llm::{create_llm, ChatMessage, LlmConfig, LargeLanguageModel};
use crate::llm::router::LlmRouter;

/// Lightweight session wrapper around the configured LLM.
/// For local llama.cpp this will reuse the shared model underneath; for HTTP it reuses the HTTP client.
#[derive(Clone)]
pub struct LlmSession {
    llm: Arc<dyn LargeLanguageModel>,
}

impl LlmSession {
    pub fn new(cfg: &LlmConfig) -> Self {
        Self { llm: create_llm(cfg) }
    }

    /// Run a strict JSON prompt with a conservative token cap enforced in the backend.
    /// Returns the raw model text; callers should parse JSON strictly.
    pub fn chat_strict(&self, prompt: &str) -> Result<String, String> {
        self.llm.chat(&[ChatMessage { role: "user".into(), content: prompt.into() }])
    }
}

/// Router-backed model to adapt the existing LargeLanguageModel interface
pub struct RouterModel {
    router: LlmRouter,
}

impl RouterModel {
    pub fn new() -> Self {
        Self { router: LlmRouter::new() }
    }
}

impl LargeLanguageModel for RouterModel {
    fn chat(&self, messages: &[ChatMessage]) -> Result<String, String> {
        let model = crate::helpers::configuration::Config::llm_chat_model().unwrap_or_else(|| "gpt-4o-mini".to_string());
        let req = crate::llm::types::ChatRequest {
            model,
            messages: messages.iter().map(|m| crate::llm::types::ChatMessage { role: m.role.clone(), content: m.content.clone() }).collect(),
            max_output_tokens: crate::helpers::configuration::Config::getenv("LLM_MAX_TOKENS", "256").parse().ok(),
            temperature: crate::helpers::configuration::Config::getenv("LLM_TEMPERATURE", "0.2").parse().ok(),
            top_p: crate::helpers::configuration::Config::getenv("LLM_TOP_P", "1.0").parse().ok(),
            response_format: None,
            thread_id: None,
        };
        let r = self.router.chat(&req)?;
        Ok(r.text)
    }
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        if texts.is_empty() { return Ok(Vec::new()); }
        let model = crate::helpers::configuration::Config::llm_embed_model().unwrap_or_else(|| "text-embedding-3-small".to_string());
        let req = crate::llm::types::EmbedRequest { model, inputs: texts.to_vec() };
        let r = self.router.embed(&req)?;
        Ok(r.vectors)
    }
}


