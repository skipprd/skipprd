use super::{ChatMessage, LargeLanguageModel, LlmConfig};

#[cfg(feature = "llama_cpp")]
mod inner {
    use super::*;
    use llama_cpp_rs::{inference::{InferenceParameters}, model::ModelParameters, LLama};
    pub fn chat(cfg: &LlmConfig, messages: &[ChatMessage]) -> Result<String, String> {
        let model_path = cfg.chat_model.clone().ok_or_else(|| "missing chat_model (path to GGUF)".to_string())?;
        let params = ModelParameters { context_size: cfg.context_length.unwrap_or(4096), n_gpu_layers: cfg.gpu_layers.unwrap_or(0) as i32, ..Default::default() };
        let llm = LLama::load_from_file(&model_path, params).map_err(|e| e.to_string())?;
        let mut session = llm.inference_session(InferenceParameters::default());
        let prompt = messages.iter().map(|m| format!("{}: {}\n", m.role, m.content)).collect::<String>();
        let mut output = String::new();
        session.inference_with_prompt(&llm, &Default::default(), &prompt, None, &mut Default::default(), |t| { output.push_str(t); true }).map_err(|e| e.to_string())?;
        Ok(output)
    }
    pub fn embed(_cfg: &LlmConfig, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        // If crate exposes embeddings, wire here. Placeholder returns zero vectors of 768 dims
        Ok(texts.iter().map(|_| vec![0.0; 768]).collect())
    }
}

#[cfg(not(feature = "llama_cpp"))]
mod inner {
    use super::*;
    pub fn chat(_cfg: &LlmConfig, messages: &[ChatMessage]) -> Result<String, String> {
        let prompt = messages.iter().map(|m| format!("{}: {}\n", m.role, m.content)).collect::<String>();
        Ok(format!("[local-llm-disabled]\n{}", prompt))
    }
    pub fn embed(_cfg: &LlmConfig, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        Ok(texts.iter().map(|_| vec![0.0; 768]).collect())
    }
}

/// Stub for a llama.cpp-backed model using the `utilityai/llama-cpp-rs` wrapper.
/// This implementation is intentionally minimal and returns errors until wired.
pub struct LlamaCppModel { cfg: LlmConfig }

impl LlamaCppModel {
    pub fn new(cfg: LlmConfig) -> Self {
        Self { cfg }
    }
}

impl LargeLanguageModel for LlamaCppModel {
    fn chat(&self, messages: &[ChatMessage]) -> Result<String, String> {
        inner::chat(&self.cfg, messages)
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        inner::embed(&self.cfg, texts)
    }
}


