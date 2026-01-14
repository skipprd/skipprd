use crate::llm::adapter::Adapter;
use crate::llm::types::*;

pub struct LlamaCppAdapter;
impl LlamaCppAdapter { pub fn new() -> Self { Self {} } }

impl Adapter for LlamaCppAdapter {
    fn capabilities(&self, _model: &str) -> Capabilities {
        Capabilities {
            supports_responses_api: false,
            supports_stream: false,
            context_window: crate::helpers::configuration::Config::llm_context_length(),
            embed_input_tokens: 2048,
        }
    }

    fn build_chat_http(&self, _req: &ChatRequest) -> Result<ProviderHttpRequest, String> {
        // Router will not use HTTP for llama.cpp; we return a sentinel URL for detection
        Ok(ProviderHttpRequest {
            method: "LOCAL".to_string(),
            url: "local://chat".to_string(),
            headers: vec![],
            body: serde_json::json!({}),
        })
    }

    fn parse_chat_http(&self, _resp: &ProviderHttpResponse) -> Result<ChatResponse, String> {
        Err("local-llama: not an HTTP provider".to_string())
    }

    fn build_embed_http(&self, _req: &EmbedRequest) -> Result<ProviderHttpRequest, String> {
        Ok(ProviderHttpRequest {
            method: "LOCAL".to_string(),
            url: "local://embed".to_string(),
            headers: vec![],
            body: serde_json::json!({}),
        })
    }

    fn parse_embed_http(&self, _resp: &ProviderHttpResponse) -> Result<EmbedResponse, String> {
        Err("local-llama: not an HTTP provider".to_string())
    }
}


