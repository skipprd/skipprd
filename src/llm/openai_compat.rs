use super::{ChatMessage, LargeLanguageModel, LlmConfig};

/// Minimal OpenAI-compatible HTTP provider stub. Returns errors until wired.
pub struct OpenAICompatModel {
    _cfg: LlmConfig,
}

impl OpenAICompatModel {
    pub fn new(cfg: LlmConfig) -> Self { Self { _cfg: cfg } }
}

impl LargeLanguageModel for OpenAICompatModel {
    fn chat(&self, _messages: &[ChatMessage]) -> Result<String, String> {
        Err("OpenAI-compatible HTTP provider not configured".to_string())
    }
    fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        Err("OpenAI-compatible HTTP embeddings not configured".to_string())
    }
}


