#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use crate::llm::LlmProviderType;

    #[test]
    fn chat_and_embed_shape() {
        let server = MockServer::start();
        let chat_mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "application/json")
                .json_body_obj(&serde_json::json!({
                    "choices": [ { "message": {"role": "assistant", "content": "hi"} } ]
                }));
        });
        let emb_mock = server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(200)
                .header("content-type", "application/json")
                .json_body_obj(&serde_json::json!({
                    "data": [ { "embedding": [0.1, 0.2] }, { "embedding": [0.3, 0.4] } ]
                }));
        });

        let cfg = LlmConfig {
            provider: LlmProviderType::OpenAICompat,
            chat_model: Some("gpt-test".to_string()),
            embed_model: Some("text-emb".to_string()),
            base_url: Some(server.base_url()),
            api_key: Some("x".to_string()),
            gpu_layers: None,
            context_length: Some(1024),
        };
        let llm = OpenAICompatModel::new(cfg);
        let out = llm.chat(&[ChatMessage { role: "user".into(), content: "hello".into() }]).unwrap();
        assert!(out.contains("hi"));
        let emb = llm.embed(&vec!["a".into(), "b".into()]).unwrap();
        assert_eq!(emb.len(), 2);
        assert_eq!(emb[0].len(), 2);

        chat_mock.assert();
        emb_mock.assert();
    }
}
use super::{ChatMessage, LargeLanguageModel, LlmConfig};
use serde::{Deserialize, Serialize};

fn pretty_json(text: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| text.to_string()),
        Err(_) => text.to_string(),
    }
}

fn pretty_val(val: &serde_json::Value) -> String {
    serde_json::to_string_pretty(val).unwrap_or_else(|_| val.to_string())
}

#[derive(Serialize, Deserialize)]
struct OaiChatMessage { role: String, content: String }

#[derive(Serialize, Deserialize)]
struct OaiChatReq {
    model: String,
    messages: Vec<OaiChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
}

#[derive(Deserialize)]
struct OaiChatRespChoiceDelta { content: Option<String> }

#[derive(Deserialize)]
struct OaiChatRespChoice { message: Option<OaiChatMessage>, delta: Option<OaiChatRespChoiceDelta> }

#[derive(Deserialize)]
struct OaiChatResp { choices: Vec<OaiChatRespChoice> }

#[derive(Serialize, Deserialize)]
struct OaiEmbReq { model: String, input: Vec<String> }

#[derive(Deserialize)]
struct OaiEmbData { embedding: Vec<f32> }

#[derive(Deserialize)]
struct OaiEmbResp { data: Vec<OaiEmbData> }

/// Minimal OpenAI-compatible HTTP provider (blocking, no Tokio runtime required).
pub struct OpenAICompatModel {
    cfg: LlmConfig,
    agent: ureq::Agent,
}

impl OpenAICompatModel {
    pub fn new(cfg: LlmConfig) -> Self {
        // Increase default timeout to accommodate /v1/responses latency on newer models
        let http_timeout_secs: u64 = crate::helpers::configuration::Config::getenv("LLM_HTTP_TIMEOUT_SECS", "30").parse().unwrap_or(30);
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(http_timeout_secs))
            .build();
        Self { cfg, agent }
    }
}

impl LargeLanguageModel for OpenAICompatModel {
    fn chat(&self, messages: &[ChatMessage]) -> Result<String, String> {
        let base = self.cfg.base_url.clone().ok_or_else(|| "missing base_url".to_string())?;
        let model = self.cfg.chat_model.clone().ok_or_else(|| "missing chat_model".to_string())?;
        let use_responses = model.starts_with("gpt-5") || model.starts_with("o4");
        if use_responses {
            #[derive(serde::Serialize)]
            struct RespPart { #[serde(rename="type")] r#type: String, text: String }
            #[derive(serde::Serialize)]
            struct RespMsg { role: String, content: Vec<RespPart> }
            #[derive(serde::Serialize)]
            struct RespReq {
                model: String,
                // Use 'input' per Responses API, with typed content parts ('input_text')
                input: Vec<RespMsg>,
                #[serde(skip_serializing_if = "Option::is_none")]
                modalities: Option<Vec<String>>,
                #[serde(skip_serializing_if = "Option::is_none")]
                response_format: Option<serde_json::Value>,
                #[serde(skip_serializing_if = "Option::is_none")]
                max_output_tokens: Option<i32>,
            }
            #[derive(serde::Deserialize)]
            struct RespResp {
                #[serde(default)]
                output_text: Option<String>,
                #[serde(default)]
                output: Vec<serde_json::Value>,
            }
            let url = format!("{}/v1/responses", base.trim_end_matches('/'));
            let max_tokens: i32 = crate::helpers::configuration::Config::getenv("LLM_MAX_TOKENS", "8192").parse().unwrap_or(8192);
            // Map messages to Responses 'input' with typed content parts
            let mut msgs: Vec<RespMsg> = Vec::new();
            for m in messages.iter() {
                let role = if m.role.eq_ignore_ascii_case("system") { "system" } else if m.role.eq_ignore_ascii_case("assistant") { "assistant" } else { "user" };
                // Responses expects 'input_text' for plain text parts
                let part = RespPart { r#type: "input_text".to_string(), text: m.content.clone() };
                msgs.push(RespMsg { role: role.to_string(), content: vec![part] });
            }
            if msgs.is_empty() {
                // Fallback: collapse all messages into one user input
                let joined = messages.iter().map(|m| format!("{}: {}", m.role, m.content)).collect::<Vec<_>>().join("\n");
                msgs.push(RespMsg { role: "user".to_string(), content: vec![RespPart { r#type: "input_text".to_string(), text: joined }] });
            }
            let body = RespReq {
                model: model.clone(),
                input: msgs,
                modalities: Some(vec!["text".to_string()]),
                response_format: Some(serde_json::json!({"type": "text"})),
                max_output_tokens: Some(max_tokens),
            };
            let mut req = self.agent.request("POST", &url).set("Content-Type", "application/json");
            if let Some(k) = self.cfg.api_key.as_ref() { req = req.set("Authorization", &format!("Bearer {}", k)); }
            // Retries for network timeouts / transient errors
            let payload = serde_json::to_value(&body).map_err(|e| e.to_string())?;
            // Debug: pretty-print full prompt
            tracing::debug!("LLM(responses) request model='{}'\n{}", model, pretty_val(&payload));
            let mut attempt = 0usize;
            let max_retries = 3usize;
            let resp = loop {
                attempt += 1;
                let res = req.clone().send_json(payload.clone());
                match res {
                    Ok(r) => {
                        if r.status() == 429 && attempt < max_retries {
                            // Honor Retry-After if present (seconds), else exponential backoff with jitter
                            let retry_after = r.header("retry-after").and_then(|s| s.parse::<u64>().ok());
                            let backoff_ms = retry_after.map(|s| s.saturating_mul(1000)).unwrap_or_else(|| 500u64.saturating_mul(1u64 << (attempt as u32 - 1)).min(5_000));
                            let jitter = (rand::random::<u64>() % 250);
                            std::thread::sleep(std::time::Duration::from_millis(backoff_ms + jitter));
                            continue;
                        }
                        break Ok(r)
                    },
                    Err(e) => {
                        let es = e.to_string().to_lowercase();
                        let transient = es.contains("timed out") || es.contains("network error") || es.contains("connection") || es.contains("temporarily");
                        if attempt < max_retries && transient {
                            let backoff_ms = 200u64.saturating_mul(1u64 << (attempt as u32 - 1)).min(2_000);
                            std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
                            continue;
                        }
                        break Err(e);
                    }
                }
            }.map_err(|e| e.to_string())?;
            if !(200..300).contains(&resp.status()) {
                let status = resp.status();
                let body_text = resp.into_string().unwrap_or_else(|_| String::new());
                tracing::debug!("LLM(responses) non-2xx status={} body:\n{}", status, pretty_json(&body_text));
                let snippet = if body_text.len() > 500 { &body_text[..500] } else { &body_text };
                // Fallback: attempt chat.completions for compatibility if 400 and model is gpt-5.x
                if status == 400 && (model.starts_with("gpt-5") || model.starts_with("o4")) {
                    // Build chat.completions payload from the same messages
                    let url_cc = format!("{}/v1/chat/completions", base.trim_end_matches('/'));
                    let cc_body = OaiChatReq {
                        model: model.clone(),
                        messages: messages.iter().map(|m| OaiChatMessage { role: m.role.clone(), content: m.content.clone() }).collect(),
                        stream: Some(false),
                        max_tokens: Some(max_tokens as u32),
                        temperature: Some(crate::helpers::configuration::Config::getenv("LLM_TEMPERATURE", "0.2").parse().unwrap_or(0.2)),
                        top_p: Some(crate::helpers::configuration::Config::getenv("LLM_TOP_P", "1.0").parse().unwrap_or(1.0)),
                    };
                    let mut req_cc = self.agent.request("POST", &url_cc).set("Content-Type", "application/json");
                    if let Some(k) = self.cfg.api_key.as_ref() { req_cc = req_cc.set("Authorization", &format!("Bearer {}", k)); }
                    let cc_payload = serde_json::to_value(&cc_body).map_err(|e| e.to_string())?;
                    tracing::debug!("LLM(chat.completions) fallback request model='{}'\n{}", cc_body.model, pretty_val(&cc_payload));
                    let res_cc = req_cc.send_json(cc_payload);
                    match res_cc {
                        Ok(rcc) if (200..300).contains(&rcc.status()) => {
                            let text = rcc.into_string().map_err(|e| e.to_string())?;
                            tracing::debug!("LLM(chat.completions) fallback response:\n{}", pretty_json(&text));
                            let obj: OaiChatResp = serde_json::from_str(&text).map_err(|e| e.to_string())?;
                            let mut out = String::new();
                            for c in obj.choices.iter() {
                                if let Some(m) = &c.message { out.push_str(&m.content); }
                                if let Some(d) = &c.delta { if let Some(s) = &d.content { out.push_str(s); } }
                            }
                            return Ok(out);
                        }
                        Ok(rcc) => {
                            let status_cc = rcc.status();
                            let body_text_cc = rcc.into_string().unwrap_or_else(|_| String::new());
                            tracing::debug!("LLM(chat.completions) fallback non-2xx={} body:\n{}", status_cc, pretty_json(&body_text_cc));
                            return Err(format!("http 400 (responses) and fallback chat {}: {}", status_cc, snippet));
                        }
                        Err(ecc) => {
                            return Err(format!("http 400 (responses) and fallback chat error: {}; {}", ecc, snippet));
                        }
                    }
                }
                return Err(format!("http {}: {}", status, snippet));
            }
            let body_text = resp.into_string().map_err(|e| e.to_string())?;
            tracing::debug!("LLM(responses) response:\n{}", pretty_json(&body_text));
            let obj: RespResp = serde_json::from_str(&body_text).map_err(|e| e.to_string())?;
            if let Some(t) = obj.output_text {
                tracing::debug!("LLM(responses) output_text:\n{}", t);
                return Ok(t);
            }
            if let Some(t) = obj.output.get(0)
                .and_then(|v| v.get("content"))
                .and_then(|c| c.get(0))
                .and_then(|p| p.get("text"))
                .and_then(|x| x.as_str()) {
                tracing::debug!("LLM(responses) output.content[0].text:\n{}", t);
                return Ok(t.to_string());
            }
            Err("empty response".to_string())
        } else {
            let url = format!("{}/v1/chat/completions", base.trim_end_matches('/'));
            // latency-optimized defaults
            let max_tokens: u32 = crate::helpers::configuration::Config::getenv("LLM_MAX_TOKENS", "1024").parse().unwrap_or(1024);
            let temperature: f32 = crate::helpers::configuration::Config::getenv("LLM_TEMPERATURE", "0.2").parse().unwrap_or(0.2);
            let top_p: f32 = crate::helpers::configuration::Config::getenv("LLM_TOP_P", "1.0").parse().unwrap_or(1.0);
            let body = OaiChatReq {
                model,
                messages: messages.iter().map(|m| OaiChatMessage { role: m.role.clone(), content: m.content.clone() }).collect(),
                stream: Some(false),
                max_tokens: Some(max_tokens),
                temperature: Some(temperature),
                top_p: Some(top_p),
            };
            let mut req = self.agent.request("POST", &url).set("Content-Type", "application/json");
            if let Some(k) = self.cfg.api_key.as_ref() { req = req.set("Authorization", &format!("Bearer {}", k)); }
            // Retries for transient issues
            let payload = serde_json::to_value(&body).map_err(|e| e.to_string())?;
            // Debug: pretty-print full prompt
            tracing::debug!("LLM(chat.completions) request model='{}'\n{}", body.model, pretty_val(&payload));
            let mut attempt = 0usize;
            let max_retries = 3usize;
            let resp = loop {
                attempt += 1;
                let res = req.clone().send_json(payload.clone());
                match res {
                    Ok(r) => {
                        if r.status() == 429 && attempt < max_retries {
                            let retry_after = r.header("retry-after").and_then(|s| s.parse::<u64>().ok());
                            let backoff_ms = retry_after.map(|s| s.saturating_mul(1000)).unwrap_or_else(|| 500u64.saturating_mul(1u64 << (attempt as u32 - 1)).min(5_000));
                            let jitter = (rand::random::<u64>() % 250);
                            std::thread::sleep(std::time::Duration::from_millis(backoff_ms + jitter));
                            continue;
                        }
                        break Ok(r)
                    },
                    Err(e) => {
                        let es = e.to_string().to_lowercase();
                        let transient = es.contains("timed out") || es.contains("network error") || es.contains("connection") || es.contains("temporarily");
                        if attempt < max_retries && transient {
                            let backoff_ms = 200u64.saturating_mul(1u64 << (attempt as u32 - 1)).min(2_000);
                            std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
                            continue;
                        }
                        break Err(e);
                    }
                }
            }.map_err(|e| e.to_string())?;
            if !(200..300).contains(&resp.status()) {
                let status = resp.status();
                let body_text = resp.into_string().unwrap_or_else(|_| String::new());
                tracing::debug!("LLM(chat.completions) non-2xx status={} body:\n{}", status, pretty_json(&body_text));
                return Err(format!("http {}", status));
            }
            let body_text = resp.into_string().map_err(|e| e.to_string())?;
            tracing::debug!("LLM(chat.completions) response:\n{}", pretty_json(&body_text));
            let obj: OaiChatResp = serde_json::from_str(&body_text).map_err(|e| e.to_string())?;
            let mut out = String::new();
            for c in obj.choices.iter() {
                if let Some(m) = &c.message { out.push_str(&m.content); }
                if let Some(d) = &c.delta { if let Some(s) = &d.content { out.push_str(s); } }
            }
            tracing::debug!("LLM(chat.completions) text:\n{}", out);
            Ok(out)
        }
    }
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        let base = self.cfg.base_url.clone().ok_or_else(|| "missing base_url".to_string())?;
        let model = self.cfg.embed_model.clone().ok_or_else(|| "missing embed_model".to_string())?;
        let url = format!("{}/v1/embeddings", base.trim_end_matches('/'));
        // Debug: pretty-print embedding inputs
        tracing::debug!("LLM(embeddings) request model='{}' inputs:\n{}", model, pretty_json(&serde_json::to_string(texts).unwrap_or_else(|_| "[]".to_string())));
        let body = OaiEmbReq { model, input: texts.to_vec() };
        let mut req = self.agent.request("POST", &url).set("Content-Type", "application/json");
        if let Some(k) = self.cfg.api_key.as_ref() { req = req.set("Authorization", &format!("Bearer {}", k)); }
        // Retries for transient issues
        let payload = serde_json::to_value(&body).map_err(|e| e.to_string())?;
        let mut attempt = 0usize;
        let max_retries = 3usize;
        let resp = loop {
            attempt += 1;
            let res = req.clone().send_json(payload.clone());
            match res {
                Ok(r) => {
                    if r.status() == 429 && attempt < max_retries {
                        let retry_after = r.header("retry-after").and_then(|s| s.parse::<u64>().ok());
                        let backoff_ms = retry_after.map(|s| s.saturating_mul(1000)).unwrap_or_else(|| 500u64.saturating_mul(1u64 << (attempt as u32 - 1)).min(5_000));
                        let jitter = (rand::random::<u64>() % 250);
                        std::thread::sleep(std::time::Duration::from_millis(backoff_ms + jitter));
                        continue;
                    }
                    break Ok(r)
                },
                Err(e) => {
                    let es = e.to_string().to_lowercase();
                    let transient = es.contains("timed out") || es.contains("network error") || es.contains("connection") || es.contains("temporarily");
                    if attempt < max_retries && transient {
                        let backoff_ms = 200u64.saturating_mul(1u64 << (attempt as u32 - 1)).min(2_000);
                        std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
                        continue;
                    }
                    break Err(e);
                }
            }
        }.map_err(|e| e.to_string())?;
        if !(200..300).contains(&resp.status()) {
            let status = resp.status();
            let body_text = resp.into_string().unwrap_or_else(|_| String::new());
            tracing::debug!("LLM(embeddings) non-2xx status={} body:\n{}", status, pretty_json(&body_text));
            return Err(format!("http {}", status));
        }
        let body_text = resp.into_string().map_err(|e| e.to_string())?;
        // Do not log raw embedding vectors; parse silently and report only shape
        let obj: OaiEmbResp = serde_json::from_str(&body_text).map_err(|e| e.to_string())?;
        let vecs: Vec<Vec<f32>> = obj.data.into_iter().map(|d| d.embedding).collect();
        let dim = vecs.get(0).map(|v| v.len()).unwrap_or(0);
        tracing::debug!("LLM(embeddings) shape: count={} dim={}", vecs.len(), dim);
        Ok(vecs)
    }
}


