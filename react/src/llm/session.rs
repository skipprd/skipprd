use std::sync::Arc;

use crate::llm::{create_llm, ChatMessage, LlmConfig, LargeLanguageModel};
use crate::llm::router::LlmRouter;
use crate::llm::thread_ctx;

/// Lightweight session wrapper around the configured LLM.
/// For local llama.cpp this will reuse the shared model underneath; for HTTP it reuses the HTTP client.
#[derive(Clone)]
pub struct LlmSession {
    llm: Arc<dyn LargeLanguageModel>,
}

impl LlmSession {
    pub fn new(cfg: &LlmConfig) -> Self {
        Self { llm: create_llm(cfg) }
    }

    /// Run a strict JSON prompt with a conservative token cap enforced in the backend.
    /// Returns the raw model text; callers should parse JSON strictly.
    pub fn chat_strict(&self, prompt: &str) -> Result<String, String> {
        self.llm.chat(&[ChatMessage { role: "user".into(), content: prompt.into() }])
    }
}

/// Router-backed model to adapt the existing LargeLanguageModel interface
pub struct RouterModel {
    router: LlmRouter,
}

impl RouterModel {
    pub fn new() -> Self {
        Self { router: LlmRouter::new() }
    }
}

impl LargeLanguageModel for RouterModel {
    fn chat(&self, messages: &[ChatMessage]) -> Result<String, String> {
        let model = crate::helpers::configuration::Config::llm_chat_model().unwrap_or_else(|| "gpt-4o-mini".to_string());
        // Heuristic: when the caller is asking for machine-readable JSON, enforce JSON output at the
        // provider level (OpenAI Responses supports `text.format.type = json_object`).
        //
        // This prevents occasional invalid "JSON-looking" text like literal newlines inside JSON strings.
        let mut wants_json = false;
        for m in messages.iter() {
            // Only inspect user/system text; assistant messages may contain previous JSON.
            if !m.role.eq_ignore_ascii_case("user") && !m.role.eq_ignore_ascii_case("system") {
                continue;
            }
            let t = m.content.to_lowercase();
            if t.contains("respond with strict json")
                || t.contains("respond with json only")
                || t.contains("respond with json")
                || t.contains("strict json only")
            {
                wants_json = true;
                break;
            }
        }

        let req = crate::llm::types::ChatRequest {
            model,
            messages: messages.iter().map(|m| crate::llm::types::ChatMessage { role: m.role.clone(), content: m.content.clone() }).collect(),
            max_output_tokens: crate::helpers::configuration::Config::getenv("LLM_MAX_TOKENS", "1024").parse().ok(),
            temperature: crate::helpers::configuration::Config::getenv("LLM_TEMPERATURE", "0.2").parse().ok(),
            top_p: crate::helpers::configuration::Config::getenv("LLM_TOP_P", "1.0").parse().ok(),
            response_format: if wants_json { Some(serde_json::json!({"type":"json_object"})) } else { None },
            thread_id: thread_ctx::current_thread_id(),
        };
        let r = self.router.chat(&req)?;
        Ok(r.text)
    }
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        if texts.is_empty() { return Ok(Vec::new()); }
        let model = crate::helpers::configuration::Config::llm_embed_model().unwrap_or_else(|| "text-embedding-3-small".to_string());
        let req = crate::llm::types::EmbedRequest { model, inputs: texts.to_vec() };
        let r = self.router.embed(&req)?;
        Ok(r.vectors)
    }
}


