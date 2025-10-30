use super::{ChatMessage, LargeLanguageModel, LlmConfig};
use crate::helpers::configuration::Config;
use std::fs;
use std::path::Path;

#[cfg(feature = "llama_cpp")]
mod inner {
    use super::*;
    use std::num::NonZeroU32;
    use llama_cpp_2::context::params::LlamaContextParams;
    use llama_cpp_2::llama_backend::LlamaBackend;
    use llama_cpp_2::llama_batch::LlamaBatch;
    use llama_cpp_2::model::{self, LlamaModel};
    use llama_cpp_2::model::params::LlamaModelParams;
    use llama_cpp_2::sampling::LlamaSampler;
    use llama_cpp_2::{send_logs_to_tracing, LogOptions};
    fn pick_model_path(cfg: &LlmConfig) -> Result<String, String> {
        if let Some(p) = cfg.chat_model.clone() { return Ok(p); }
        let candidates = vec!["./models"]; // search simple default dir
        for dir in candidates {
            if let Ok(rd) = fs::read_dir(dir) {
                for e in rd.flatten() {
                    let p = e.path(); if let Some(ext) = p.extension() { if ext == "gguf" { return Ok(p.to_string_lossy().to_string()); } }
                }
            }
        }
        Err("no GGUF model found; set LLM_CHAT_MODEL".to_string())
    }

    fn load_with_autotune(backend: &LlamaBackend, model_path: &str, hint_gpu_layers: Option<usize>) -> Result<(LlamaModel, i32), String> {
        // Gate local autotune cache behind env; default disabled to avoid local writes
        let cache_path = format!("{}/catalog_cache/llm_tuning.json", Config::get_data_dir());
        let enable_local_tune_cache = Config::truth_value(&Config::getenv("SKIPPR_ENABLE_LOCAL_LLM_TUNE_CACHE", "false"));
        let key = Path::new(model_path).canonicalize().map_err(|_| "model path".to_string())?.to_string_lossy().to_string();
        let mut saved_layers: Option<i32> = None;
        if enable_local_tune_cache { if let Ok(s) = fs::read_to_string(&cache_path) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) { if let Some(n) = v.get(&key).and_then(|x| x.as_i64()) { saved_layers = Some(n as i32); } }
        } }
        let mut start: i32 = saved_layers
            .or_else(|| hint_gpu_layers.map(|x| x as i32))
            .unwrap_or_else(|| if cfg!(target_os = "macos") { 32 } else { 0 });
        let mut attempts = 0;
        loop {
            let params = LlamaModelParams::default().with_n_gpu_layers(start.max(0) as u32);
            match LlamaModel::load_from_file(backend, model_path, &params) {
                Ok(llm) => {
                    // persist only if enabled
                    if enable_local_tune_cache {
                        let mut obj = serde_json::json!({});
                        if let Ok(s) = fs::read_to_string(&cache_path) { let _ = serde_json::from_str::<serde_json::Value>(&s).map(|v| obj = v); }
                        obj.as_object_mut().unwrap().insert(key.clone(), serde_json::json!(start));
                        let _ = fs::create_dir_all(Path::new(&cache_path).parent().unwrap());
                        let _ = fs::write(&cache_path, serde_json::to_string_pretty(&obj).unwrap_or_default());
                    }
                    return Ok((llm, start));
                }
                Err(_) => {
                    attempts += 1; if attempts > 6 { return Err("failed to load model after autotune".to_string()); }
                    start = (start - 8).max(0);
                }
            }
        }
    }

    pub fn chat(cfg: &LlmConfig, messages: &[ChatMessage]) -> Result<String, String> {
        println!("{} LLM(llama.cpp): selecting model...", chrono::Utc::now().to_rfc3339());
        let model_path = match pick_model_path(cfg) { Ok(p) => p, Err(_) => {
            // Fallback stub when no local model is available
            let prompt = messages.iter().map(|m| format!("{}: {}\n", m.role, m.content)).collect::<String>();
            return Ok(prompt);
        }};
        // suppress llama.cpp/ggml logs from stdout unless explicitly enabled later
        send_logs_to_tracing(LogOptions::default().with_logs_enabled(false));
        println!("{} LLM(llama.cpp): initializing backend...", chrono::Utc::now().to_rfc3339());
        let backend = LlamaBackend::init().map_err(|e| e.to_string())?;
        println!("{} LLM(llama.cpp): loading model {}...", chrono::Utc::now().to_rfc3339(), model_path);
        let (model, _gpu) = load_with_autotune(&backend, &model_path, cfg.gpu_layers)?;

        // Build prompt using model chat template if available
        let prompt = {
            let msgs: Vec<model::LlamaChatMessage> = messages
                .iter()
                .map(|m| model::LlamaChatMessage::new(m.role.clone(), m.content.clone()))
                .collect::<Result<_, _>>()
                .map_err(|e| e.to_string())?;
            if let Ok(tmpl) = model.chat_template(None) {
                model.apply_chat_template(&tmpl, &msgs, true).map_err(|e| e.to_string())?
            } else {
                messages.iter().map(|m| format!("{}: {}\n", m.role, m.content)).collect::<String>()
            }
        };

        let ctx_len = cfg.context_length.unwrap_or(2048) as u32;
        let mut ctx_params = LlamaContextParams::default()
            .with_n_ctx(Some(NonZeroU32::new(ctx_len).unwrap_or(NonZeroU32::new(2048).unwrap())));
        println!("{} LLM(llama.cpp): creating context (n_ctx={})...", chrono::Utc::now().to_rfc3339(), ctx_len);
        let mut ctx = model.new_context(&backend, ctx_params).map_err(|e| e.to_string())?;

        // tokenize prompt
        let tokens = model.str_to_token(&prompt, model::AddBos::Always).map_err(|e| e.to_string())?;

        let mut batch = LlamaBatch::new(512, 1);
        let last_index: i32 = (tokens.len() as i32) - 1;
        for (i, token) in (0_i32..).zip(tokens.into_iter()) {
            let is_last = i == last_index;
            batch.add(token, i, &[0], is_last).map_err(|e| e.to_string())?;
        }
        println!("{} LLM(llama.cpp): priming context... ({} tokens)", chrono::Utc::now().to_rfc3339(), last_index + 1);
        ctx.decode(&mut batch).map_err(|e| e.to_string())?;

        // simple greedy decode up to a reasonable limit
        let mut sampler = LlamaSampler::greedy();
        let mut output = String::new();
        // decode bytes to string (lossy fallback for safety)
        let mut n_cur = batch.n_tokens();
        // cap new tokens; if context_length is set, use a conservative portion
        let max_new_tokens: i32 = cfg
            .context_length
            .map(|c| (c as i32 / 4).clamp(64, 512))
            .unwrap_or(256);
        let mut generated: i32 = 0;
        println!("{} LLM(llama.cpp): generating up to {} tokens...", chrono::Utc::now().to_rfc3339(), max_new_tokens);
        while generated < max_new_tokens {
            let token = sampler.sample(&ctx, batch.n_tokens() - 1);
            sampler.accept(token);
            if model.is_eog_token(token) { break; }
            let bytes = model.token_to_bytes(token, model::Special::Tokenize).map_err(|e| e.to_string())?;
            output.push_str(&String::from_utf8_lossy(&bytes));
            batch.clear();
            batch.add(token, n_cur as i32, &[0], true).map_err(|e| e.to_string())?;
            n_cur += 1;
            generated += 1;
            ctx.decode(&mut batch).map_err(|e| e.to_string())?;
        }
        println!("{} LLM(llama.cpp): generation done ({} tokens)", chrono::Utc::now().to_rfc3339(), generated);
        Ok(output.trim().to_string())
    }
    pub fn embed(cfg: &LlmConfig, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        if texts.is_empty() { return Ok(Vec::new()); }
        // suppress logs
        send_logs_to_tracing(LogOptions::default().with_logs_enabled(false));
        let backend = LlamaBackend::init().map_err(|e| e.to_string())?;
        // prefer embed model if provided, otherwise chat model / auto-discover
        let mut cfg2 = cfg.clone();
        if cfg2.chat_model.is_none() { cfg2.chat_model = cfg2.embed_model.clone(); }
        let model_path = match pick_model_path(&cfg2) {
            Ok(p) => p,
            Err(_) => {
                // fallback stub when no model: return zero vectors of common dim 768
                return Ok(texts.iter().map(|_| vec![0.0f32; 768]).collect());
            }
        };
        let (model, _gpu) = match load_with_autotune(&backend, &model_path, cfg.gpu_layers) {
            Ok(m) => m,
            Err(_) => return Ok(texts.iter().map(|_| vec![0.0f32; 768]).collect()),
        };

        // enable embeddings
        let threads = std::thread::available_parallelism().map(|n| n.get() as i32).unwrap_or(4);
        let ctx_params = LlamaContextParams::default()
            .with_n_threads_batch(threads)
            .with_embeddings(true)
            .with_pooling_type(llama_cpp_2::context::params::LlamaPoolingType::Mean);
        let mut ctx = model.new_context(&backend, ctx_params).map_err(|e| e.to_string())?;

        let mut out: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
        for text in texts {
            let tokens = model.str_to_token(text, model::AddBos::Always).map_err(|e| e.to_string())?;
            // ensure fits context
            if (tokens.len() as u32) > ctx.n_ctx() { return Err("prompt exceeds context window".into()); }
            let mut batch = LlamaBatch::new(ctx.n_ctx() as usize, 1);
            batch.add_sequence(&tokens, 0, true).map_err(|e| e.to_string())?;
            // reset cache and encode
            ctx.clear_kv_cache();
            ctx.decode(&mut batch).map_err(|e| e.to_string())?;
            // retrieve embedding for sequence 0; if pooling unsupported, average token embeddings
            match ctx.embeddings_seq_ith(0) {
                Ok(emb) => out.push(emb.to_vec()),
                Err(_) => {
                    let dim = usize::try_from(model.n_embd()).unwrap_or(0);
                    let mut acc = vec![0.0f32; dim];
                    let count = batch.n_tokens().max(1);
                    for i in 0..count { if let Ok(vec) = ctx.embeddings_ith(i) {
                        for (j, v) in vec.iter().enumerate() { acc[j] += *v; }
                    }}
                    if count > 0 { for v in &mut acc { *v /= count as f32; } }
                    out.push(acc);
                }
            }
            batch.clear();
        }
        Ok(out)
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


