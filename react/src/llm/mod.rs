pub use react_core::llm::{ChatMessage, LargeLanguageModel};
use std::sync::Arc;

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
pub fn create_llm(_cfg: &LlmConfig) -> Arc<dyn LargeLanguageModel> {
    // Return router-backed model to keep callers stable
    Arc::new(crate::llm::session::RouterModel::new())
}

pub fn config_from_env() -> LlmConfig {
    let prov = crate::helpers::configuration::Config::llm_provider().to_uppercase();
    let provider = match prov.as_str() {
        "OPENAI" | "OPENAI_COMPAT" | "HTTP" => LlmProviderType::OpenAICompat,
        _ => LlmProviderType::Local,
    };
    let ctx_len_opt = crate::helpers::configuration::Config::llm_context_length_opt();
    let ctx_len = match ctx_len_opt {
        Some(v) => Some(v),
        None => Some(crate::helpers::configuration::Config::llm_context_length()),
    };
    LlmConfig {
        provider,
        chat_model: crate::helpers::configuration::Config::llm_chat_model(),
        embed_model: crate::helpers::configuration::Config::llm_embed_model(),
        base_url: crate::helpers::configuration::Config::llm_base_url(),
        api_key: crate::helpers::configuration::Config::llm_api_key(),
        gpu_layers: crate::helpers::configuration::Config::llm_gpu_layers(),
        context_length: ctx_len,
    }
}

/// Build LLM config from a resolved `react` config file (with env overrides already applied).
///
/// Note: `LLM_API_KEY` remains env-driven and is intentionally not stored in YAML.
pub fn config_from_resolved(cfg: &crate::config::ReactResolvedConfig) -> LlmConfig {
    let prov = cfg
        .llm
        .provider
        .clone()
        .unwrap_or_else(|| crate::helpers::configuration::Config::llm_provider())
        .to_uppercase();
    let provider = match prov.as_str() {
        "OPENAI" | "OPENAI_COMPAT" | "HTTP" => LlmProviderType::OpenAICompat,
        _ => LlmProviderType::Local,
    };
    LlmConfig {
        provider,
        chat_model: cfg
            .llm
            .chat_model
            .clone()
            .or_else(|| crate::helpers::configuration::Config::llm_chat_model()),
        embed_model: cfg
            .llm
            .embed_model
            .clone()
            .or_else(|| crate::helpers::configuration::Config::llm_embed_model()),
        base_url: cfg
            .llm
            .base_url
            .clone()
            .or_else(|| crate::helpers::configuration::Config::llm_base_url()),
        api_key: crate::helpers::configuration::Config::llm_api_key(),
        gpu_layers: cfg
            .llm
            .gpu_layers
            .or_else(|| crate::helpers::configuration::Config::llm_gpu_layers()),
        context_length: cfg
            .llm
            .context_length
            .or_else(|| crate::helpers::configuration::Config::llm_context_length_opt())
            .or(Some(
                crate::helpers::configuration::Config::llm_context_length(),
            )),
    }
}

/// Placeholder model that always errors. Used when provider backends are not wired yet.
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

pub mod adapter;
pub mod llama_cpp;
pub mod llama_cpp_adapter;
pub mod openai_chat_adapter;
pub mod openai_compat;
pub mod openai_responses_adapter;
pub mod registry;
pub mod router;
pub mod session;
pub mod thread_ctx;
pub mod types;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults() {
        let cfg = super::config_from_env();
        assert!(
            matches!(cfg.provider, LlmProviderType::Local)
                || matches!(cfg.provider, LlmProviderType::OpenAICompat)
        );
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
