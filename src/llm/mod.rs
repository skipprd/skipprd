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

pub fn config_from_env() -> LlmConfig {
    let prov = crate::helpers::configuration::Config::llm_provider().to_uppercase();
    let provider = match prov.as_str() {
        "OPENAI" | "OPENAI_COMPAT" | "HTTP" => LlmProviderType::OpenAICompat,
        _ => LlmProviderType::Local,
    };
    LlmConfig {
        provider,
        chat_model: crate::helpers::configuration::Config::llm_chat_model(),
        embed_model: crate::helpers::configuration::Config::llm_embed_model(),
        base_url: crate::helpers::configuration::Config::llm_base_url(),
        api_key: crate::helpers::configuration::Config::llm_api_key(),
        gpu_layers: crate::helpers::configuration::Config::llm_gpu_layers(),
        context_length: crate::helpers::configuration::Config::llm_context_length_opt(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults() {
        let cfg = super::config_from_env();
        assert!(matches!(cfg.provider, LlmProviderType::Local) || matches!(cfg.provider, LlmProviderType::OpenAICompat));
        // context_length may be None (auto-tune) or a positive value from env
        assert!(cfg.context_length.is_none() || cfg.context_length.unwrap() > 0);
    }

    #[test]
    fn factory_provider_selection() {
        std::env::set_var("LLM_PROVIDER", "OPENAI");
        let cfg = super::config_from_env();
        let _llm = create_llm(&cfg);
        std::env::remove_var("LLM_PROVIDER");
    }
}


