use super::{ChatMessage, LargeLanguageModel, LlmConfig};

/// Stub for a llama.cpp-backed model using the `utilityai/llama-cpp-rs` wrapper.
/// This implementation is intentionally minimal and returns errors until wired.
pub struct LlamaCppModel {
    _cfg: LlmConfig,
}

impl LlamaCppModel {
    pub fn new(cfg: LlmConfig) -> Self { Self { _cfg: cfg } }
}

impl LargeLanguageModel for LlamaCppModel {
    fn chat(&self, _messages: &[ChatMessage]) -> Result<String, String> {
        Err("llama-cpp-rs not initialized".to_string())
    }

    fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        Err("llama-cpp-rs embeddings not initialized".to_string())
    }
}


