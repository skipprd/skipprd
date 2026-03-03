use std::sync::Arc;

use react_core::resolved_config::LlmProvider;

use super::adapter::Adapter;

pub enum ProviderKind {
    OpenAIGeneric,
    LlamaCpp,
}

fn provider_from_config() -> ProviderKind {
    match crate::runtime_settings::llm_provider().unwrap_or(LlmProvider::Null) {
        LlmProvider::Openai | LlmProvider::OpenaiCompat | LlmProvider::Http => {
            ProviderKind::OpenAIGeneric
        }
        LlmProvider::LlamaCpp | LlmProvider::Null => ProviderKind::LlamaCpp,
    }
}

pub fn pick_adapter_from_config() -> Arc<dyn Adapter> {
    match provider_from_config() {
        ProviderKind::LlamaCpp => Arc::new(crate::llm::llama_cpp_adapter::LlamaCppAdapter::new()),
        ProviderKind::OpenAIGeneric => {
            Arc::new(crate::llm::openai_chat_adapter::OpenAIChatAdapter::new())
        }
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
