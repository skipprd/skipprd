use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ChatResponseFormat {
    Text,
    JsonObject,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub response_format: Option<ChatResponseFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ChatResponse {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct EmbedRequest {
    pub model: String,
    pub inputs: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct EmbedResponse {
    pub vectors: Vec<Vec<f32>>,
    pub dim: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Capabilities {
    pub supports_responses_api: bool,
    pub supports_stream: bool,
    pub context_window: usize,
    pub embed_input_tokens: usize,
}

#[derive(Clone, Debug)]
pub struct ProviderHttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: serde_json::Value,
}

#[derive(Clone, Debug)]
pub struct ProviderHttpResponse {
    pub status: u16,
    pub body_text: String,
}
