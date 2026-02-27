use crate::llm::types::{
    Capabilities, ChatRequest, ChatResponse, EmbedRequest, EmbedResponse, ProviderHttpRequest,
    ProviderHttpResponse,
};

pub trait Adapter: Send + Sync {
    fn capabilities(&self, model: &str) -> Capabilities;

    // Build HTTP request for chat; router will execute and pass response to parse
    fn build_chat_http(&self, req: &ChatRequest) -> Result<ProviderHttpRequest, String>;
    fn parse_chat_http(&self, resp: &ProviderHttpResponse) -> Result<ChatResponse, String>;

    // Build HTTP request for embeddings
    fn build_embed_http(&self, req: &EmbedRequest) -> Result<ProviderHttpRequest, String>;
    fn parse_embed_http(&self, resp: &ProviderHttpResponse) -> Result<EmbedResponse, String>;
}
