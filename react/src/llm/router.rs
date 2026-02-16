use dashmap::DashMap;
use once_cell::sync::OnceCell;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::helpers::configuration::Config;
use tracing::debug;

use super::adapter::Adapter;
use super::registry::{pick_adapter_from_config, pick_openai_adapter_for_model};
use super::types::{
    ChatRequest, ChatResponse, ChatResponseFormat, EmbedRequest, EmbedResponse, ProviderHttpResponse,
};
use crate::llm::LargeLanguageModel;
use react_core::llm::ChatMessage as CoreChatMessage;
use react_core::llm_observability::{self, PartInput};

fn pretty_json(text: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| text.to_string()),
        Err(_) => text.to_string(),
    }
}

fn router_parts_for_messages(req: &ChatRequest) -> (Vec<CoreChatMessage>, Vec<PartInput>) {
    let mut core_msgs: Vec<CoreChatMessage> = Vec::new();
    let mut parts: Vec<PartInput> = Vec::new();
    for (i, m) in req.messages.iter().enumerate() {
        core_msgs.push(CoreChatMessage {
            role: m.role.clone(),
            content: m.content.clone(),
        });
        parts.push(PartInput {
            name: format!("msg.{:02}.{}", i, m.role.trim().to_lowercase()),
            text: m.content.clone(),
        });
    }
    (core_msgs, parts)
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
fn chat_memo() -> &'static DashMap<String, MemoEntry> {
    CHAT_MEMO.get_or_init(|| DashMap::new())
}

impl LlmRouter {
    pub fn new() -> Self {
        let adapter = pick_adapter_from_config();
        // Default timeout bumped to accommodate /v1/responses latency; still overridable via env/config.
        let timeout_secs: u64 = Config::getenv("LLM_HTTP_TIMEOUT_SECS", "120")
            .parse()
            .unwrap_or(120);
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
            let msgs: Vec<crate::llm::ChatMessage> = req
                .messages
                .iter()
                .map(|m| crate::llm::ChatMessage {
                    role: m.role.clone(),
                    content: m.content.clone(),
                })
                .collect();
            let expected_format = matches!(req.response_format, Some(ChatResponseFormat::JsonObject));
            let opts = react_core::llm::LlmCallOptions {
                prompt_id: "react.router.local_llama_chat",
                thread_id: None,
                expected_format: if expected_format {
                    react_core::llm::LlmExpectedFormat::JsonObject
                } else {
                    react_core::llm::LlmExpectedFormat::Text
                },
                max_output_tokens: None,
                temperature: None,
                top_p: None,
                reasoning_effort: None,
            };
            let text = model.chat(&msgs, &opts)?;
            return Ok(ChatResponse { text, raw: None });
        }
        // Log request (pretty JSON and readable message text)
        let body_json_len = serde_json::to_string(&http_req.body)
            .map(|s| s.len())
            .unwrap_or(0);
        debug!(
            "LLM(router) request chat {} {} messages={} body_json_bytes={}",
            http_req.method,
            http_req.url,
            req.messages.len(),
            body_json_len
        );
        // Opt-in only: full request bodies can be enormous and leak prompts/secrets.
        if Config::getenv("LLM_LOG_REQUEST_BODIES", "0") == "1" {
            debug!(
                "LLM(router) request body:\n{}",
                serde_json::to_string_pretty(&http_req.body).unwrap_or_default()
            );
        }
        // Execute
        let full_url = format!("{}{}", self.base_prefix(), http_req.url);
        let payload = serde_json::to_value(&http_req.body).map_err(|e| e.to_string())?;
        let max_retries: usize = Config::getenv("LLM_HTTP_MAX_RETRIES", "3")
            .parse()
            .unwrap_or(3)
            .max(1)
            .min(10);

        // LLM observability (parts): best-effort, thread-scoped when req.thread_id is provided.
        let obs_thread_id = req.thread_id.clone().filter(|s| !s.trim().is_empty());
        let (call_id_opt, prompt_hash_opt, built_parts_opt) =
            if llm_observability::llm_calls_enabled() {
                if let Some(tid) = obs_thread_id.as_deref() {
                    let call_id = llm_observability::next_call_id(tid);
                    let (core_msgs, parts) = router_parts_for_messages(req);
                    let prompt_hash = llm_observability::prompt_hash_for_messages(&core_msgs);
                    let built = llm_observability::build_parts_for_thread(tid, &parts);
                    let prompt_id = req.prompt_id.as_deref().unwrap_or("-");
                    debug!(
                    "LLM_CALL thread_id={} call_id={} agent={} phase={} model={} prompt_id={} response_pending=1",
                    tid,
                    call_id,
                    "router",
                    "router",
                    req.model,
                    prompt_id
                );
                    for p in built.parts.iter() {
                        let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("-");
                        let hash = p.get("hash").and_then(|v| v.as_str()).unwrap_or("-");
                        let text = p.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        debug!(
                            "LLM_PART thread_id={} call_id={} name={} hash={} text={}",
                            tid, call_id, name, hash, text
                        );
                    }
                    (Some(call_id), Some(prompt_hash), Some(built))
                } else {
                    (None, None, None)
                }
            } else {
                (None, None, None)
            };

        let mut attempt = 0usize;
        let (status, body_text) = loop {
            attempt += 1;
            let mut r = self
                .http
                .request(&http_req.method, &full_url)
                .set("Content-Type", "application/json");
            if let Some(k) = self.api_key.as_ref() {
                r = r.set("Authorization", &format!("Bearer {}", k));
            }
            for (h, v) in http_req.headers.iter() {
                r = r.set(h, v);
            }

            let resp = r.send_json(payload.clone());
            match resp {
                Ok(resp_ok) => break (resp_ok.status(), resp_ok.into_string().unwrap_or_default()),
                Err(ureq::Error::Status(s, rr)) => {
                    // Retry 429 with backoff
                    if s == 429 && attempt < max_retries {
                        let retry_after =
                            rr.header("retry-after").and_then(|v| v.parse::<u64>().ok());
                        let backoff_ms = retry_after
                            .map(|secs| secs.saturating_mul(1000))
                            .unwrap_or_else(|| {
                                500u64
                                    .saturating_mul(1u64 << (attempt as u32 - 1))
                                    .min(5_000)
                            });
                        let jitter = rand::random::<u64>() % 250;
                        std::thread::sleep(Duration::from_millis(backoff_ms + jitter));
                        continue;
                    }
                    break (s, rr.into_string().unwrap_or_default());
                }
                Err(e) => {
                    let es = e.to_string().to_lowercase();
                    let transient = es.contains("timed out")
                        || es.contains("timeout")
                        || es.contains("network error")
                        || es.contains("connection")
                        || es.contains("temporarily");
                    if transient && attempt < max_retries {
                        let backoff_ms = 250u64
                            .saturating_mul(1u64 << (attempt as u32 - 1))
                            .min(2_000);
                        std::thread::sleep(Duration::from_millis(backoff_ms));
                        continue;
                    }
                    return Err(format!("LLM request failed: {}: {}", full_url, e));
                }
            }
        };
        let ph = ProviderHttpResponse {
            status: status as u16,
            body_text,
        };
        // Log response (pretty)
        debug!(
            "LLM(router) response chat status={} body_bytes={}",
            ph.status,
            ph.body_text.len()
        );
        if Config::getenv("LLM_LOG_RESPONSE_BODIES", "0") == "1" {
            debug!(
                "LLM(router) response chat body:\n{}",
                pretty_json(&ph.body_text)
            );
        }
        if !(200..300).contains(&(ph.status as i32)) {
            let snippet = if ph.body_text.len() > 500 {
                &ph.body_text[..500]
            } else {
                &ph.body_text
            };
            return Err(format!(
                "LLM request failed: {}: http {}: {}",
                full_url, ph.status, snippet
            ));
        }
        // Fail fast on Responses truncation due to output token budget.
        // When this happens, the response may contain *no usable output_text* or a partial/truncated JSON object.
        // In either case, callers should stop immediately and increase max_output_tokens.
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&ph.body_text) {
            let resp_id = v.get("id").and_then(|x| x.as_str()).unwrap_or("");
            let status = v.get("status").and_then(|x| x.as_str()).unwrap_or("");
            let reason = v
                .get("incomplete_details")
                .and_then(|x| x.get("reason"))
                .and_then(|x| x.as_str())
                .unwrap_or("");
            if status == "incomplete" && reason == "max_output_tokens" {
                let budget = req.max_output_tokens.unwrap_or(0);
                let usage_out = v
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0);
                let usage_in = v
                    .get("usage")
                    .and_then(|u| u.get("input_tokens"))
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0);
                let effort = req
                    .reasoning_effort
                    .as_deref()
                    .unwrap_or("(unset)");
                let tid = obs_thread_id.as_deref().unwrap_or("-");
                let call_id = call_id_opt
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "-".to_string());
                let prompt_id = req.prompt_id.as_deref().unwrap_or("-");
                return Err(format!(
                    "LLM output truncated (Responses status=incomplete reason=max_output_tokens). \
Increase max_output_tokens for this call. thread_id={} call_id={} prompt_id={} model={} reasoning_effort={} max_output_tokens={} usage_in={} usage_out={} response_id={}",
                    tid,
                    call_id,
                    prompt_id,
                    req.model,
                    effort,
                    budget,
                    usage_in,
                    usage_out,
                    resp_id
                ));
            }
        }

        let parsed = adapter.parse_chat_http(&ph)?;
        // Observability response logging (no truncation). If not enabled, do not print parsed text at all by default.
        if let (Some(tid), Some(call_id), Some(prompt_hash), Some(built)) = (
            obs_thread_id.as_deref(),
            call_id_opt,
            prompt_hash_opt,
            built_parts_opt,
        ) {
            let response_hash = llm_observability::sha256_hex_str(&parsed.text);
            let response_text = if llm_observability::llm_response_text_enabled() {
                Some(llm_observability::redact_common_secrets(&parsed.text))
            } else {
                None
            };
            if let Some(rt) = response_text.as_deref() {
                debug!(
                    "LLM_RESPONSE thread_id={} call_id={} prompt_id={} response_hash={} response_text={}",
                    tid,
                    call_id,
                    req.prompt_id.as_deref().unwrap_or("-"),
                    response_hash,
                    rt
                );
            } else {
                debug!(
                    "LLM_RESPONSE thread_id={} call_id={} prompt_id={} response_hash={} response_text=disabled",
                    tid,
                    call_id,
                    req.prompt_id.as_deref().unwrap_or("-"),
                    response_hash
                );
            }
            // Keep variables used (to avoid accidental drop warnings if future edits extend this).
            let _ = (prompt_hash, built);
        } else if Config::getenv("LLM_LOG_PARSED_TEXT", "0") == "1" {
            debug!("LLM(router) parsed text:\n{}", pretty_json(&parsed.text));
        } else {
            debug!(
                "LLM(router) parsed text omitted (set LLM_LOG_PARSED_TEXT=1 to print): chars={} sha256={}",
                parsed.text.len(),
                llm_observability::sha256_hex_str(&parsed.text)
            );
        }
        // store in memo
        chat_memo().insert(
            key,
            MemoEntry {
                at: Instant::now(),
                resp: parsed.clone(),
            },
        );
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
        let body_json_len = serde_json::to_string(&http_req.body)
            .map(|s| s.len())
            .unwrap_or(0);
        debug!(
            "LLM(router) request embed {} {} inputs={} body_json_bytes={}",
            http_req.method,
            http_req.url,
            req.inputs.len(),
            body_json_len
        );
        if Config::getenv("LLM_LOG_REQUEST_BODIES", "0") == "1" {
            debug!(
                "LLM(router) request embed body:\n{}",
                serde_json::to_string_pretty(&http_req.body).unwrap_or_default()
            );
        }
        // Execute
        let full_url = format!("{}{}", self.base_prefix(), http_req.url);
        let payload = serde_json::to_value(&http_req.body).map_err(|e| e.to_string())?;
        let max_retries: usize = Config::getenv("LLM_HTTP_MAX_RETRIES", "3")
            .parse()
            .unwrap_or(3)
            .max(1)
            .min(10);

        let mut attempt = 0usize;
        let (status, body_text) = loop {
            attempt += 1;
            let mut r = self
                .http
                .request(&http_req.method, &full_url)
                .set("Content-Type", "application/json");
            if let Some(k) = self.api_key.as_ref() {
                r = r.set("Authorization", &format!("Bearer {}", k));
            }
            for (h, v) in http_req.headers.iter() {
                r = r.set(h, v);
            }

            let resp = r.send_json(payload.clone());
            match resp {
                Ok(resp_ok) => break (resp_ok.status(), resp_ok.into_string().unwrap_or_default()),
                Err(ureq::Error::Status(s, rr)) => {
                    if s == 429 && attempt < max_retries {
                        let retry_after =
                            rr.header("retry-after").and_then(|v| v.parse::<u64>().ok());
                        let backoff_ms = retry_after
                            .map(|secs| secs.saturating_mul(1000))
                            .unwrap_or_else(|| {
                                500u64
                                    .saturating_mul(1u64 << (attempt as u32 - 1))
                                    .min(5_000)
                            });
                        let jitter = rand::random::<u64>() % 250;
                        std::thread::sleep(Duration::from_millis(backoff_ms + jitter));
                        continue;
                    }
                    break (s, rr.into_string().unwrap_or_default());
                }
                Err(e) => {
                    let es = e.to_string().to_lowercase();
                    let transient = es.contains("timed out")
                        || es.contains("timeout")
                        || es.contains("network error")
                        || es.contains("connection")
                        || es.contains("temporarily");
                    if transient && attempt < max_retries {
                        let backoff_ms = 250u64
                            .saturating_mul(1u64 << (attempt as u32 - 1))
                            .min(2_000);
                        std::thread::sleep(Duration::from_millis(backoff_ms));
                        continue;
                    }
                    return Err(format!("LLM request failed: {}: {}", full_url, e));
                }
            }
        };
        let ph = ProviderHttpResponse {
            status: status as u16,
            body_text,
        };
        // Log response (pretty; avoid printing large vectors)
        debug!(
            "LLM(router) response embed status={} body_bytes={}",
            ph.status,
            ph.body_text.len()
        );
        if Config::getenv("LLM_LOG_RESPONSE_BODIES", "0") == "1" {
            debug!(
                "LLM(router) response embed body:\n{}",
                pretty_json(&ph.body_text)
            );
        }
        if !(200..300).contains(&(ph.status as i32)) {
            let snippet = if ph.body_text.len() > 500 {
                &ph.body_text[..500]
            } else {
                &ph.body_text
            };
            return Err(format!(
                "LLM request failed: {}: http {}: {}",
                full_url, ph.status, snippet
            ));
        }
        adapter.parse_embed_http(&ph)
    }

    fn base_prefix(&self) -> String {
        self.base_url
            .as_ref()
            .map(|s| s.trim_end_matches('/').to_string())
            .unwrap_or_default()
    }
}
