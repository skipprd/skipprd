/// Minimal ReAct configuration shim.
///
/// This replaces the ingest-oriented `skippr::helpers::configuration::Config` after the crate split.
/// We intentionally keep this small and environment-driven.

use std::sync::OnceLock;

#[derive(Clone, Debug)]
struct ScopePreference {
    tenant: String,
    workspace: String,
    project_id: String,
}

static SCOPE_PREFERENCE: OnceLock<ScopePreference> = OnceLock::new();

pub struct Config;

impl Config {
    pub fn getenv(key: &str, default: &str) -> String {
        std::env::var(key)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| default.to_string())
    }

    pub fn llm_provider() -> String {
        if let Some(provider) = crate::runtime_settings::llm_provider() {
            return provider.to_string();
        }
        Self::getenv("LLM_PROVIDER", "OPENAI_COMPAT")
    }

    pub fn llm_base_url() -> Option<String> {
        if let Some(v) = crate::runtime_settings::llm_base_url() {
            return Some(v);
        }
        std::env::var("LLM_BASE_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
    }

    pub fn llm_api_key() -> Option<String> {
        std::env::var("LLM_API_KEY")
            .ok()
            .filter(|v| !v.trim().is_empty())
    }

    pub fn llm_chat_model() -> Option<String> {
        if let Some(v) = crate::runtime_settings::llm_chat_model() {
            return Some(v);
        }
        std::env::var("LLM_CHAT_MODEL")
            .ok()
            .filter(|v| !v.trim().is_empty())
    }

    pub fn llm_embed_model() -> Option<String> {
        if let Some(v) = crate::runtime_settings::llm_embed_model() {
            return Some(v);
        }
        std::env::var("LLM_EMBED_MODEL")
            .ok()
            .filter(|v| !v.trim().is_empty())
    }

    pub fn llm_gpu_layers() -> Option<usize> {
        if let Some(v) = crate::runtime_settings::llm_gpu_layers() {
            return Some(v);
        }
        std::env::var("LLM_GPU_LAYERS")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
    }

    pub fn llm_context_length_opt() -> Option<usize> {
        if let Some(v) = crate::runtime_settings::llm_context_length() {
            return Some(v);
        }
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

    pub fn set_scope_preference(tenant: String, workspace: String, project_id: String) {
        let _ = SCOPE_PREFERENCE.set(ScopePreference {
            tenant,
            workspace,
            project_id,
        });
    }

    pub fn get_tenant() -> String {
        if let Some(scope) = SCOPE_PREFERENCE.get() {
            let value = scope.tenant.trim();
            if !value.is_empty() {
                return value.to_string();
            }
        }
        Self::getenv("TENANT", "default")
    }

    pub fn get_workspace_name() -> String {
        if let Some(scope) = SCOPE_PREFERENCE.get() {
            let value = scope.workspace.trim();
            if !value.is_empty() {
                return value.to_string();
            }
        }
        Self::getenv("WORKSPACE", "default")
    }

    pub fn get_project_id() -> String {
        if let Some(scope) = SCOPE_PREFERENCE.get() {
            let value = scope.project_id.trim();
            if !value.is_empty() {
                return value.to_string();
            }
        }
        Self::getenv("PROJECT_ID", "default")
    }

    pub fn get_pipeline_name() -> String {
        // Legacy naming: "pipeline" is the project scope in ReAct.
        let project_id = Self::get_project_id();
        if project_id != "default" {
            return project_id;
        }
        Self::getenv("PIPELINE", "default")
    }
}
