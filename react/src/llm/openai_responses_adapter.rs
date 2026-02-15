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
    /// The OpenAI Responses API `text.format` object.
    ///
    /// Examples:
    /// - `{ "type": "text" }`
    /// - `{ "type": "json_object" }`
    /// - `{ "type": "json_schema", "name": "...", "schema": {...}, "strict": true }`
    format: serde_json::Value,
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

        // Default to plain text, but allow callers to request structured output via `response_format`.
        // This maps directly to the Responses API `text.format` object.
        let format = match req.response_format {
            None | Some(ChatResponseFormat::Text) => serde_json::json!({"type":"text"}),
            Some(ChatResponseFormat::JsonObject) => serde_json::json!({"type":"json_object"}),
        };

        let body = RespReq {
            model: req.model.clone(),
            input: msgs,
            text: Some(RespText { format }),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_chat_http_passes_response_format_to_text_format() {
        let ad = OpenAIResponsesAdapter::new();
        let req = ChatRequest {
            model: "gpt-4.1-mini".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: "hi".to_string(),
            }],
            max_output_tokens: None,
            temperature: None,
            top_p: None,
            response_format: Some(ChatResponseFormat::JsonObject),
            thread_id: None,
        };
        let http = ad.build_chat_http(&req).expect("build");
        assert_eq!(http.url, "/v1/responses");
        let fmt = http
            .body
            .get("text")
            .and_then(|t| t.get("format"))
            .and_then(|f| f.get("type"))
            .and_then(|x| x.as_str())
            .unwrap_or("");
        assert_eq!(fmt, "json_object");
    }

    #[test]
    fn build_chat_http_defaults_to_text_format() {
        let ad = OpenAIResponsesAdapter::new();
        let req = ChatRequest {
            model: "gpt-4.1-mini".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: "hi".to_string(),
            }],
            max_output_tokens: None,
            temperature: None,
            top_p: None,
            response_format: None,
            thread_id: None,
        };
        let http = ad.build_chat_http(&req).expect("build");
        let fmt = http
            .body
            .get("text")
            .and_then(|t| t.get("format"))
            .and_then(|f| f.get("type"))
            .and_then(|x| x.as_str())
            .unwrap_or("");
        assert_eq!(fmt, "text");
    }
}
