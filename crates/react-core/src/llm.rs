use std::sync::Arc;

/// Optional per-call overrides for LLM sampling/limits.
///
/// When fields are `None`, implementations should fall back to their configured defaults
/// (e.g., env/config values).
#[derive(Clone, Copy, Debug, Default)]
pub struct LlmCallOptions {
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
}

/// High-level abstraction for large language models used by the ReAct runtime.
/// Implementations may be local (llama.cpp) or remote (OpenAI-compatible HTTP).
pub trait LargeLanguageModel: Send + Sync {
    fn chat(&self, messages: &[ChatMessage]) -> Result<String, String>;
    /// Optional override-capable chat API.
    ///
    /// Default implementation ignores `options` and delegates to `chat()` to preserve
    /// backward compatibility for existing model providers.
    fn chat_with_options(
        &self,
        messages: &[ChatMessage],
        options: Option<&LlmCallOptions>,
    ) -> Result<String, String> {
        let _ = options;
        self.chat(messages)
    }
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
