use std::sync::Arc;

/// High-level abstraction for large language models used by Skipprd.
/// Implementations may be local (llama.cpp) or remote (OpenAI-compatible HTTP).
pub trait LargeLanguageModel: Send + Sync {
    fn chat(&self, messages: &[ChatMessage]) -> Result<String, String>;
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String>;
}

#[derive(Clone, Debug)]
pub struct ChatMessage {
    pub role: String,  // "system" | "user" | "assistant"
    pub content: String,
}

#[derive(Clone, Debug)]
pub enum LlmProviderType {
    Local,
    OpenAICompat,
}

#[derive(Clone, Debug)]
pub struct LlmConfig {
    pub provider: LlmProviderType,
    pub chat_model: Option<String>,
    pub embed_model: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub gpu_layers: Option<usize>,
    pub context_length: Option<usize>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        LlmConfig {
            provider: LlmProviderType::Local,
            chat_model: None,
            embed_model: None,
            base_url: None,
            api_key: None,
            gpu_layers: None,
            context_length: Some(4096),
        }
    }
}

/// Factory to build an LLM from configuration.
/// Note: Concrete providers are optional at compile-time; when not linked,
/// this returns a no-op placeholder that errors on use.
pub fn create_llm(cfg: &LlmConfig) -> Arc<dyn LargeLanguageModel> {
    match cfg.provider {
        LlmProviderType::Local => Arc::new(crate::llm::llama_cpp::LlamaCppModel::new(cfg.clone())),
        LlmProviderType::OpenAICompat => Arc::new(crate::llm::openai_compat::OpenAICompatModel::new(cfg.clone())),
    }
}

/// Placeholder model that always errors. Used when provider backends are not wired yet.
pub struct NullModel {}

impl NullModel {
    pub fn new() -> Self { Self {} }
}

impl LargeLanguageModel for NullModel {
    fn chat(&self, _messages: &[ChatMessage]) -> Result<String, String> {
        Err("LLM provider not configured".to_string())
    }
    fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        Err("LLM provider not configured".to_string())
    }
}

pub mod llama_cpp;
pub mod openai_compat;


