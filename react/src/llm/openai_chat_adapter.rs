use crate::llm::adapter::Adapter;
use crate::llm::types::*;
use serde::{Deserialize, Serialize};

pub struct OpenAIChatAdapter;

impl OpenAIChatAdapter {
    pub fn new() -> Self {
        Self {}
    }
}

#[derive(Serialize, Deserialize)]
struct OaiChatMessage {
    role: String,
    content: String,
}
#[derive(Serialize)]
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
struct OaiChatRespChoiceDelta {
    content: Option<String>,
}
#[derive(Deserialize)]
struct OaiChatRespChoice {
    message: Option<OaiChatMessage>,
    delta: Option<OaiChatRespChoiceDelta>,
}
#[derive(Deserialize)]
struct OaiChatResp {
    choices: Vec<OaiChatRespChoice>,
}

#[derive(Serialize)]
struct OaiEmbReq {
    model: String,
    input: Vec<String>,
}
#[derive(Deserialize)]
struct OaiEmbData {
    embedding: Vec<f32>,
}
#[derive(Deserialize)]
struct OaiEmbResp {
    data: Vec<OaiEmbData>,
}

impl Adapter for OpenAIChatAdapter {
    fn capabilities(&self, _model: &str) -> Capabilities {
        Capabilities {
            supports_responses_api: false,
            supports_stream: false,
            context_window: 128_000,
            embed_input_tokens: 8_000,
        }
    }

    fn build_chat_http(&self, req: &ChatRequest) -> Result<ProviderHttpRequest, String> {
        let body = OaiChatReq {
            model: req.model.clone(),
            messages: req
                .messages
                .iter()
                .map(|m| OaiChatMessage {
                    role: m.role.clone(),
                    content: m.content.clone(),
                })
                .collect(),
            stream: Some(false),
            max_tokens: req.max_output_tokens,
            temperature: req.temperature,
            top_p: req.top_p,
        };
        Ok(ProviderHttpRequest {
            method: "POST".to_string(),
            url: "/v1/chat/completions".to_string(),
            headers: vec![],
            body: serde_json::to_value(body).map_err(|e| e.to_string())?,
        })
    }

    fn parse_chat_http(&self, resp: &ProviderHttpResponse) -> Result<ChatResponse, String> {
        let obj: OaiChatResp = serde_json::from_str(&resp.body_text).map_err(|e| e.to_string())?;
        let mut out = String::new();
        for c in obj.choices.iter() {
            if let Some(m) = &c.message {
                out.push_str(&m.content);
            }
            if let Some(d) = &c.delta {
                if let Some(s) = &d.content {
                    out.push_str(s);
                }
            }
        }
        Ok(ChatResponse {
            text: out,
            raw: serde_json::from_str(&resp.body_text).ok(),
        })
    }

    fn build_embed_http(&self, req: &EmbedRequest) -> Result<ProviderHttpRequest, String> {
        let body = OaiEmbReq {
            model: req.model.clone(),
            input: req.inputs.clone(),
        };
        Ok(ProviderHttpRequest {
            method: "POST".to_string(),
            url: "/v1/embeddings".to_string(),
            headers: vec![],
            body: serde_json::to_value(body).map_err(|e| e.to_string())?,
        })
    }

    fn parse_embed_http(&self, resp: &ProviderHttpResponse) -> Result<EmbedResponse, String> {
        let obj: OaiEmbResp = serde_json::from_str(&resp.body_text).map_err(|e| e.to_string())?;
        let vecs: Vec<Vec<f32>> = obj.data.into_iter().map(|d| d.embedding).collect();
        let dim = vecs.get(0).map(|v| v.len()).unwrap_or(0);
        Ok(EmbedResponse { vectors: vecs, dim })
    }
}
