use once_cell::sync::OnceCell;
use react_core::resolved_config::{LlmProvider, ReactResolvedConfig};

static RESOLVED_CONFIG: OnceCell<ReactResolvedConfig> = OnceCell::new();

pub fn bind_resolved_config(cfg: &ReactResolvedConfig) {
    let _ = RESOLVED_CONFIG.set(cfg.clone());
}

pub fn resolved_config() -> Option<&'static ReactResolvedConfig> {
    RESOLVED_CONFIG.get()
}

pub fn llm_provider() -> Option<LlmProvider> {
    resolved_config().map(|cfg| cfg.llm.provider)
}

pub fn llm_base_url() -> Option<String> {
    resolved_config().and_then(|cfg| cfg.llm.base_url.clone())
}

pub fn llm_chat_model() -> Option<String> {
    resolved_config().and_then(|cfg| cfg.llm.chat_model.clone())
}

pub fn llm_embed_model() -> Option<String> {
    resolved_config().and_then(|cfg| cfg.llm.embed_model.clone())
}

pub fn llm_gpu_layers() -> Option<usize> {
    resolved_config().and_then(|cfg| cfg.llm.gpu_layers)
}

pub fn llm_context_length() -> Option<usize> {
    resolved_config().and_then(|cfg| cfg.llm.context_length)
}

pub fn llm_http_timeout_secs() -> Option<u64> {
    resolved_config().and_then(|cfg| cfg.llm.http_timeout_secs)
}

pub fn llm_max_tokens() -> Option<u32> {
    resolved_config().and_then(|cfg| cfg.llm.max_tokens)
}

pub fn llm_temperature() -> Option<f32> {
    resolved_config().and_then(|cfg| cfg.llm.temperature)
}

pub fn llm_top_p() -> Option<f32> {
    resolved_config().and_then(|cfg| cfg.llm.top_p)
}
