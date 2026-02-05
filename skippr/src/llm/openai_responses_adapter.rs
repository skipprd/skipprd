use crate::llm::adapter::Adapter;
use crate::llm::types::*;
use serde::{Deserialize, Serialize};

pub struct OpenAIResponsesAdapter;
impl OpenAIResponsesAdapter {
    pub fn new() -> Self {
        Self {}
    }
}

#[derive(Serialize)]
struct RespPart {
    #[serde(rename = "type")]
    r#type: String,
    text: String,
}
#[derive(Serialize)]
struct RespMsg {
    role: String,
    content: Vec<RespPart>,
}
#[derive(Serialize)]
struct RespReq {
    model: String,
    input: Vec<RespMsg>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<RespText>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<i32>,
}
#[derive(Serialize)]
struct RespText {
    format: RespFormat,
}
#[derive(Serialize)]
struct RespFormat {
    #[serde(rename = "type")]
    r#type: String,
}
#[derive(Deserialize)]
struct RespResp {
    #[serde(default)]
    output_text: Option<String>,
    #[serde(default)]
    output: Vec<serde_json::Value>,
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

impl Adapter for OpenAIResponsesAdapter {
    fn capabilities(&self, _model: &str) -> Capabilities {
        Capabilities {
            supports_responses_api: true,
            supports_stream: false,
            context_window: 128_000,
            embed_input_tokens: 8_000,
        }
    }

    fn build_chat_http(&self, req: &ChatRequest) -> Result<ProviderHttpRequest, String> {
        let mut msgs: Vec<RespMsg> = Vec::new();
        for m in req.messages.iter() {
            let role = if m.role.eq_ignore_ascii_case("system") {
                "system"
            } else if m.role.eq_ignore_ascii_case("assistant") {
                "assistant"
            } else {
                "user"
            };
            let part = RespPart {
                r#type: "input_text".to_string(),
                text: m.content.clone(),
            };
            msgs.push(RespMsg {
                role: role.to_string(),
                content: vec![part],
            });
        }
        if msgs.is_empty() {
            msgs.push(RespMsg {
                role: "user".to_string(),
                content: vec![RespPart {
                    r#type: "input_text".to_string(),
                    text: String::new(),
                }],
            });
        }
        let body = RespReq {
            model: req.model.clone(),
            input: msgs,
            text: Some(RespText {
                format: RespFormat {
                    r#type: "text".to_string(),
                },
            }),
            max_output_tokens: req.max_output_tokens.map(|v| v as i32),
        };
        Ok(ProviderHttpRequest {
            method: "POST".to_string(),
            url: "/v1/responses".to_string(),
            headers: vec![],
            body: serde_json::to_value(body).map_err(|e| e.to_string())?,
        })
    }

    fn parse_chat_http(&self, resp: &ProviderHttpResponse) -> Result<ChatResponse, String> {
        let obj: RespResp = serde_json::from_str(&resp.body_text).map_err(|e| e.to_string())?;
        if let Some(t) = obj.output_text {
            return Ok(ChatResponse {
                text: t,
                raw: serde_json::from_str(&resp.body_text).ok(),
            });
        }
        if let Some(t) = obj
            .output
            .get(0)
            .and_then(|v| v.get("content"))
            .and_then(|c| c.get(0))
            .and_then(|p| p.get("text"))
            .and_then(|x| x.as_str())
        {
            return Ok(ChatResponse {
                text: t.to_string(),
                raw: serde_json::from_str(&resp.body_text).ok(),
            });
        }
        Err("empty response".to_string())
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
