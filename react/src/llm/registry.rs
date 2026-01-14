use std::sync::Arc;

use crate::helpers::configuration::Config;

use super::adapter::Adapter;

pub enum ProviderKind {
    OpenAIGeneric,
    OpenAIChat,
    OpenAIResponses,
    LlamaCpp,
}

fn provider_from_env() -> ProviderKind {
    let p = Config::llm_provider().to_uppercase();
    match p.as_str() {
        "OPENAI" | "OPENAI_COMPAT" | "HTTP" => ProviderKind::OpenAIGeneric,
        "OPENAI_RESPONSES" => ProviderKind::OpenAIResponses,
        "LLAMA_CPP" => ProviderKind::LlamaCpp,
        // Default OpenAI-compatible chat
        _ => ProviderKind::OpenAIChat,
    }
}

pub fn pick_adapter_from_config() -> Arc<dyn Adapter> {
    match provider_from_env() {
        ProviderKind::OpenAIResponses => Arc::new(crate::llm::openai_responses_adapter::OpenAIResponsesAdapter::new()),
        ProviderKind::LlamaCpp => Arc::new(crate::llm::llama_cpp_adapter::LlamaCppAdapter::new()),
        ProviderKind::OpenAIChat => Arc::new(crate::llm::openai_chat_adapter::OpenAIChatAdapter::new()),
        ProviderKind::OpenAIGeneric => Arc::new(crate::llm::openai_chat_adapter::OpenAIChatAdapter::new()),
    }
}

pub fn pick_openai_adapter_for_model(model: &str) -> Arc<dyn Adapter> {
    let m = model.to_lowercase();
    // Inference rules: gpt-5.* and o4.* → Responses API, otherwise Chat
    if m.starts_with("gpt-5") || m.starts_with("o4") {
        Arc::new(crate::llm::openai_responses_adapter::OpenAIResponsesAdapter::new())
    } else {
        Arc::new(crate::llm::openai_chat_adapter::OpenAIChatAdapter::new())
    }
}


