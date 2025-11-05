use std::sync::Arc;

use crate::llm::{create_llm, ChatMessage, LlmConfig};

/// Lightweight session wrapper around the configured LLM.
/// For local llama.cpp this will reuse the shared model underneath; for HTTP it reuses the HTTP client.
#[derive(Clone)]
pub struct LlmSession {
    llm: Arc<dyn crate::llm::LargeLanguageModel>,
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


