use std::sync::Arc;

/// High-level abstraction for large language models used by the ReAct runtime.
/// Implementations may be local (llama.cpp) or remote (OpenAI-compatible HTTP).
pub trait LargeLanguageModel: Send + Sync {
    fn chat(&self, messages: &[ChatMessage]) -> Result<String, String>;
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String>;
}

#[derive(Clone, Debug)]
pub struct ChatMessage {
    pub role: String, // "system" | "user" | "assistant"
    pub content: String,
}

/// Placeholder model that always errors. Useful for tests that need a default.
pub struct NullModel {}

impl NullModel {
    pub fn new() -> Self {
        Self {}
    }
}

impl LargeLanguageModel for NullModel {
    fn chat(&self, _messages: &[ChatMessage]) -> Result<String, String> {
        Err("LLM provider not configured".to_string())
    }
    fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        Err("LLM provider not configured".to_string())
    }
}

pub type DynLlm = Arc<dyn LargeLanguageModel>;
