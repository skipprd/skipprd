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
        let http_timeout_secs: u64 = crate::helpers::configuration::Config::getenv("LLM_HTTP_TIMEOUT_SECS", "10").parse().unwrap_or(10);
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
        let url = format!("{}/v1/chat/completions", base.trim_end_matches('/'));
        // latency-optimized defaults
        let max_tokens: u32 = crate::helpers::configuration::Config::getenv("LLM_MAX_TOKENS", "256").parse().unwrap_or(256);
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
        let resp = req.send_json(serde_json::to_value(&body).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        if !(200..300).contains(&resp.status()) { return Err(format!("http {}", resp.status())); }
        let obj: OaiChatResp = resp.into_json().map_err(|e| e.to_string())?;
        let mut out = String::new();
        for c in obj.choices.iter() {
            if let Some(m) = &c.message { out.push_str(&m.content); }
            if let Some(d) = &c.delta { if let Some(s) = &d.content { out.push_str(s); } }
        }
        Ok(out)
    }
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        let base = self.cfg.base_url.clone().ok_or_else(|| "missing base_url".to_string())?;
        let model = self.cfg.embed_model.clone().ok_or_else(|| "missing embed_model".to_string())?;
        let url = format!("{}/v1/embeddings", base.trim_end_matches('/'));
        let body = OaiEmbReq { model, input: texts.to_vec() };
        let mut req = self.agent.request("POST", &url).set("Content-Type", "application/json");
        if let Some(k) = self.cfg.api_key.as_ref() { req = req.set("Authorization", &format!("Bearer {}", k)); }
        let resp = req.send_json(serde_json::to_value(&body).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        if !(200..300).contains(&resp.status()) { return Err(format!("http {}", resp.status())); }
        let obj: OaiEmbResp = resp.into_json().map_err(|e| e.to_string())?;
        Ok(obj.data.into_iter().map(|d| d.embedding).collect())
    }
}


