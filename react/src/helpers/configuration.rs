/// Minimal ReAct configuration shim.
///
/// This replaces the ingest-oriented `skippr::helpers::configuration::Config` after the crate split.
/// We intentionally keep this small and environment-driven.

pub struct Config;

impl Config {
    pub fn getenv(key: &str, default: &str) -> String {
        std::env::var(key)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| default.to_string())
    }

    pub fn llm_provider() -> String {
        Self::getenv("LLM_PROVIDER", "OPENAI_CHAT")
    }

    pub fn llm_base_url() -> Option<String> {
        std::env::var("LLM_BASE_URL").ok().filter(|v| !v.trim().is_empty())
    }

    pub fn llm_api_key() -> Option<String> {
        std::env::var("LLM_API_KEY").ok().filter(|v| !v.trim().is_empty())
    }

    pub fn llm_chat_model() -> Option<String> {
        std::env::var("LLM_CHAT_MODEL").ok().filter(|v| !v.trim().is_empty())
    }

    pub fn llm_embed_model() -> Option<String> {
        std::env::var("LLM_EMBED_MODEL").ok().filter(|v| !v.trim().is_empty())
    }

    pub fn llm_gpu_layers() -> Option<usize> {
        std::env::var("LLM_GPU_LAYERS")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
    }

    pub fn llm_context_length_opt() -> Option<usize> {
        std::env::var("LLM_CONTEXT_LENGTH")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
    }

    pub fn llm_context_length() -> usize {
        Self::llm_context_length_opt().unwrap_or(4096)
    }

    pub fn stats_histogram_enabled() -> bool {
        Self::getenv("STATS_HISTOGRAM_ENABLED", "false")
            .to_lowercase()
            .as_str()
            == "true"
    }
}

