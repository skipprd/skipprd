use std::sync::Arc;
use once_cell::sync::OnceCell;
use dashmap::DashMap;
use std::time::{Duration, Instant};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::helpers::configuration::Config;
use tracing::debug;

use super::adapter::Adapter;
use super::registry::{pick_adapter_from_config, pick_openai_adapter_for_model};
use super::types::{
    ChatRequest, ChatResponse, EmbedRequest, EmbedResponse, ProviderHttpResponse,
};
use crate::llm::LargeLanguageModel;

fn pretty_json(text: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| text.to_string()),
        Err(_) => text.to_string(),
    }
}

pub struct LlmRouter {
    adapter: Arc<dyn Adapter>,
    base_url: Option<String>,
    api_key: Option<String>,
    http: ureq::Agent,
}

#[derive(Clone)]
struct MemoEntry {
    at: Instant,
    resp: ChatResponse,
}
static CHAT_MEMO: OnceCell<DashMap<String, MemoEntry>> = OnceCell::new();
fn chat_memo() -> &'static DashMap<String, MemoEntry> { CHAT_MEMO.get_or_init(|| DashMap::new()) }

impl LlmRouter {
    pub fn new() -> Self {
        let adapter = pick_adapter_from_config();
        let timeout_secs: u64 = Config::getenv("LLM_HTTP_TIMEOUT_SECS", "30").parse().unwrap_or(30);
        let http = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .build();
        Self {
            adapter,
            base_url: Config::llm_base_url(),
            api_key: Config::llm_api_key(),
            http,
        }
    }

    pub fn chat(&self, req: &ChatRequest) -> Result<ChatResponse, String> {
        // Choose adapter dynamically for OpenAI based on model family, preserving local llama behavior
        let provider = Config::llm_provider().to_uppercase();
        let adapter: Arc<dyn Adapter> = match provider.as_str() {
            "LLAMA_CPP" => self.adapter.clone(),
            "OPENAI" | "OPENAI_COMPAT" | "HTTP" => pick_openai_adapter_for_model(&req.model),
            _ => self.adapter.clone(),
        };
        // Memoization key: model + hash(messages JSON)
        let mut hasher = DefaultHasher::new();
        if let Ok(msgs_json) = serde_json::to_string(&req.messages) {
            msgs_json.hash(&mut hasher);
        } else {
            // fallback: concatenate roles+contents
            for m in req.messages.iter() {
                m.role.hash(&mut hasher);
                m.content.hash(&mut hasher);
            }
        }
        let key = format!("{}|{:016x}", req.model, hasher.finish());
        // TTL 120s
        if let Some(entry) = chat_memo().get(&key) {
            if entry.at.elapsed() < Duration::from_secs(120) {
                tracing::debug!("LLM(router) chat memo hit");
                return Ok(entry.resp.clone());
            }
        }
        let http_req = adapter.build_chat_http(req)?;
        // Local llama.cpp branch (no HTTP)
        if http_req.url.starts_with("local://chat") {
            let cfg = crate::llm::config_from_env();
            let model = crate::llm::llama_cpp::LlamaCppModel::new(cfg);
            let msgs: Vec<crate::llm::ChatMessage> = req.messages.iter().map(|m| crate::llm::ChatMessage { role: m.role.clone(), content: m.content.clone() }).collect();
            let text = model.chat(&msgs)?;
            return Ok(ChatResponse { text, raw: None });
        }
        // Log request (pretty JSON and readable message text)
        debug!("LLM(router) request chat {} {}\n{}", http_req.method, http_req.url, serde_json::to_string_pretty(&http_req.body).unwrap_or_default());
        if !req.messages.is_empty() {
            let joined = req.messages.iter().map(|m| format!("{}: {}", m.role, m.content)).collect::<Vec<_>>().join("\n");
            debug!("LLM(router) request content (text):\n{}", joined);
        }
        // Execute
        let mut r = self.http
            .request(&http_req.method, &format!("{}{}", self.base_prefix(), http_req.url))
            .set("Content-Type", "application/json");
        if let Some(k) = self.api_key.as_ref() { r = r.set("Authorization", &format!("Bearer {}", k)); }
        for (h, v) in http_req.headers.iter() { r = r.set(h, v); }
        let resp = r.send_json(serde_json::to_value(&http_req.body).map_err(|e| e.to_string())?);
        let (status, body_text) = match resp {
            Ok(resp_ok) => (resp_ok.status(), resp_ok.into_string().unwrap_or_default()),
            Err(ureq::Error::Status(s, r)) => (s, r.into_string().unwrap_or_default()),
            Err(e) => return Err(e.to_string()),
        };
        let ph = ProviderHttpResponse { status: status as u16, body_text };
        // Log response (pretty)
        debug!("LLM(router) response chat status={}\n{}", ph.status, pretty_json(&ph.body_text));
        if !(200..300).contains(&(ph.status as i32)) {
            return Err(format!("http {}", ph.status));
        }
        let parsed = adapter.parse_chat_http(&ph)?;
        // Pretty print parsed text if it's JSON; otherwise print raw text
        let pretty_text = pretty_json(&parsed.text);
        debug!("LLM(router) response text:\n{}", pretty_text);
        // store in memo
        chat_memo().insert(key, MemoEntry { at: Instant::now(), resp: parsed.clone() });
        Ok(parsed)
    }

    pub fn embed(&self, req: &EmbedRequest) -> Result<EmbedResponse, String> {
        let provider = Config::llm_provider().to_uppercase();
        let adapter: Arc<dyn Adapter> = match provider.as_str() {
            "LLAMA_CPP" => self.adapter.clone(),
            "OPENAI" | "OPENAI_COMPAT" | "HTTP" => pick_openai_adapter_for_model(&req.model),
            _ => self.adapter.clone(),
        };
        let http_req = adapter.build_embed_http(req)?;
        if http_req.url.starts_with("local://embed") {
            let cfg = crate::llm::config_from_env();
            let model = crate::llm::llama_cpp::LlamaCppModel::new(cfg);
            let vecs = model.embed(&req.inputs)?;
            let dim = vecs.get(0).map(|v| v.len()).unwrap_or(0);
            return Ok(EmbedResponse { vectors: vecs, dim });
        }
        // Log request
        debug!("LLM(router) request embed {} {}\n{}", http_req.method, http_req.url, serde_json::to_string_pretty(&http_req.body).unwrap_or_default());
        // Execute
        let mut r = self.http
            .request(&http_req.method, &format!("{}{}", self.base_prefix(), http_req.url))
            .set("Content-Type", "application/json");
        if let Some(k) = self.api_key.as_ref() { r = r.set("Authorization", &format!("Bearer {}", k)); }
        for (h, v) in http_req.headers.iter() { r = r.set(h, v); }
        let resp = r.send_json(serde_json::to_value(&http_req.body).map_err(|e| e.to_string())?);
        let (status, body_text) = match resp {
            Ok(resp_ok) => (resp_ok.status(), resp_ok.into_string().unwrap_or_default()),
            Err(ureq::Error::Status(s, r)) => (s, r.into_string().unwrap_or_default()),
            Err(e) => return Err(e.to_string()),
        };
        let ph = ProviderHttpResponse { status: status as u16, body_text };
        // Log response (pretty; avoid printing large vectors)
        debug!("LLM(router) response embed status={}\n{}", ph.status, pretty_json(&ph.body_text));
        if !(200..300).contains(&(ph.status as i32)) {
            return Err(format!("http {}", ph.status));
        }
        adapter.parse_embed_http(&ph)
    }

    fn base_prefix(&self) -> String {
        self.base_url.as_ref().map(|s| s.trim_end_matches('/').to_string()).unwrap_or_default()
    }
}


