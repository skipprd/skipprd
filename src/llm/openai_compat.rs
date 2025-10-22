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
struct OaiChatReq { model: String, messages: Vec<OaiChatMessage>, stream: Option<bool> }

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

/// Minimal OpenAI-compatible HTTP provider stub. Returns errors until wired.
pub struct OpenAICompatModel {
    cfg: LlmConfig,
    client: reqwest::blocking::Client,
}

impl OpenAICompatModel {
    pub fn new(cfg: LlmConfig) -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();
        Self { cfg, client }
    }
}

impl LargeLanguageModel for OpenAICompatModel {
    fn chat(&self, messages: &[ChatMessage]) -> Result<String, String> {
        let base = self.cfg.base_url.clone().ok_or_else(|| "missing base_url".to_string())?;
        let model = self.cfg.chat_model.clone().ok_or_else(|| "missing chat_model".to_string())?;
        let url = format!("{}/v1/chat/completions", base.trim_end_matches('/'));
        let body = OaiChatReq {
            model,
            messages: messages.iter().map(|m| OaiChatMessage { role: m.role.clone(), content: m.content.clone() }).collect(),
            stream: Some(false),
        };
        let mut req = self.client.post(&url).json(&body).header("Content-Type", "application/json");
        if let Some(k) = self.cfg.api_key.as_ref() { req = req.header("Authorization", format!("Bearer {}", k)); }
        let resp = req.send().map_err(|e| e.to_string())?;
        if !resp.status().is_success() { return Err(format!("http {}", resp.status())); }
        let obj: OaiChatResp = resp.json().map_err(|e| e.to_string())?;
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
        let mut req = self.client.post(&url).json(&body).header("Content-Type", "application/json");
        if let Some(k) = self.cfg.api_key.as_ref() { req = req.header("Authorization", format!("Bearer {}", k)); }
        let resp = req.send().map_err(|e| e.to_string())?;
        if !resp.status().is_success() { return Err(format!("http {}", resp.status())); }
        let obj: OaiEmbResp = resp.json().map_err(|e| e.to_string())?;
        Ok(obj.data.into_iter().map(|d| d.embedding).collect())
    }
}


