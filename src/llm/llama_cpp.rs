use super::{ChatMessage, LargeLanguageModel, LlmConfig};
use crate::helpers::configuration::Config;
use std::fs;
use std::path::Path;

#[cfg(feature = "llama_cpp")]
mod inner {
    use super::*;
    use std::num::NonZeroU32;
    use std::sync::Arc;
    use llama_cpp_2::context::params::LlamaContextParams;
    use llama_cpp_2::llama_backend::LlamaBackend;
    use llama_cpp_2::llama_batch::LlamaBatch;
    use llama_cpp_2::model::{self, LlamaModel};
    use llama_cpp_2::model::params::LlamaModelParams;
    use llama_cpp_2::sampling::LlamaSampler;
    use llama_cpp_2::{send_logs_to_tracing, LogOptions};
    use once_cell::sync::OnceCell;
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
        // Remote S3 autotune cache
        fn s3_key() -> String {
            let tenant = Config::get_tenant();
            let workspace = Config::get_workspace_name();
            let pipeline = Config::get_pipeline_name();
            format!("{}/{}/{}/llm/llm_tuning.json", tenant, workspace, pipeline)
        }
        fn load_map() -> serde_json::Value {
            let key = s3_key();
            match tokio::runtime::Handle::try_current() {
                // Avoid blocking inside an active runtime; skip remote cache in this case
                Ok(_h) => serde_json::json!({}),
                Err(_) => {
                    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                    rt.block_on(async { crate::helpers::s3::get_json(&key).await }).ok().unwrap_or(serde_json::json!({}))
                }
            }
        }
        fn save_map(obj: &serde_json::Value) {
            let key = s3_key();
            let val = obj.clone();
            match tokio::runtime::Handle::try_current() {
                // Avoid blocking inside an active runtime; skip remote cache in this case
                Ok(_h) => { let _ = &val; /* no-op */ }
                Err(_) => {
                    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                    let _ = rt.block_on(async { crate::helpers::s3::put_json(&key, &val).await });
                }
            }
        }
        let key_model = Path::new(model_path).canonicalize().map_err(|_| "model path".to_string())?.to_string_lossy().to_string();
        let mut saved_layers: Option<i32> = load_map().get(&key_model).and_then(|x| x.as_i64()).map(|n| n as i32);
        let mut start: i32 = saved_layers
            .or_else(|| hint_gpu_layers.map(|x| x as i32))
            .unwrap_or_else(|| if cfg!(target_os = "macos") { 32 } else { 0 });
        let mut attempts = 0;
        loop {
            let params = LlamaModelParams::default().with_n_gpu_layers(start.max(0) as u32);
            match LlamaModel::load_from_file(backend, model_path, &params) {
                Ok(llm) => {
                    let mut obj = load_map();
                    obj.as_object_mut().unwrap().insert(key_model.clone(), serde_json::json!(start));
                    save_map(&obj);
                    return Ok((llm, start));
                }
                Err(_) => {
                    attempts += 1; if attempts > 6 { return Err("failed to load model after autotune".to_string()); }
                    start = (start - 8).max(0);
                }
            }
        }
    }

    struct SharedLocalModel {
        backend: LlamaBackend,
        model: LlamaModel,
        model_path: String,
        gpu_layers: i32,
    }

    static SHARED_LOCAL: OnceCell<Arc<SharedLocalModel>> = OnceCell::new();

    fn get_or_load_shared(cfg: &LlmConfig) -> Result<Arc<SharedLocalModel>, String> {
        if let Some(shared) = SHARED_LOCAL.get() { return Ok(shared.clone()); }
        // suppress logs to stdout
        send_logs_to_tracing(LogOptions::default().with_logs_enabled(false));
        println!("{} LLM(llama.cpp): selecting model...", chrono::Utc::now().to_rfc3339());
        let model_path = pick_model_path(cfg)?;
        println!("{} LLM(llama.cpp): initializing backend...", chrono::Utc::now().to_rfc3339());
        let backend = LlamaBackend::init().map_err(|e| e.to_string())?;
        println!("{} LLM(llama.cpp): loading model {}...", chrono::Utc::now().to_rfc3339(), model_path);
        let (model, gpu_layers) = load_with_autotune(&backend, &model_path, cfg.gpu_layers)?;
        let shared = Arc::new(SharedLocalModel { backend, model, model_path, gpu_layers });
        let _ = SHARED_LOCAL.set(shared.clone());
        Ok(shared)
    }

    fn load_context_with_autotune<'a>(
        model: &'a LlamaModel,
        backend: &'a LlamaBackend,
        model_path: &'a str,
        hint_ctx: Option<usize>,
    ) -> Result<(llama_cpp_2::context::LlamaContext<'a>, u32), String> {
        // Use S3-based tuning map
        fn s3_key() -> String {
            let tenant = Config::get_tenant();
            let workspace = Config::get_workspace_name();
            let pipeline = Config::get_pipeline_name();
            format!("{}/{}/{}/llm/llm_tuning.json", tenant, workspace, pipeline)
        }
        fn load_map() -> serde_json::Value {
            let key = s3_key();
            match tokio::runtime::Handle::try_current() {
                // Avoid blocking inside an active runtime; skip remote cache
                Ok(_h) => serde_json::json!({}),
                Err(_) => {
                    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                    rt.block_on(async { crate::helpers::s3::get_json(&key).await }).ok().unwrap_or(serde_json::json!({}))
                }
            }
        }
        fn save_map(obj: &serde_json::Value) {
            let key = s3_key();
            let val = obj.clone();
            match tokio::runtime::Handle::try_current() {
                // Avoid blocking inside an active runtime; skip remote cache
                Ok(_h) => { let _ = &val; /* no-op */ }
                Err(_) => {
                    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                    let _ = rt.block_on(async { crate::helpers::s3::put_json(&key, &val).await });
                }
            }
        }
        let key = Path::new(model_path)
            .canonicalize()
            .map_err(|_| "model path".to_string())?
            .to_string_lossy()
            .to_string() + "#ctx";
        let mut saved_ctx: Option<u32> = load_map().get(&key).and_then(|x| x.as_u64()).map(|n| n as u32);
        let mut start: u32 = saved_ctx
            .or_else(|| hint_ctx.map(|x| x as u32))
            .unwrap_or(4096);
        let mut attempts = 0;
        loop {
            let params = LlamaContextParams::default()
                .with_n_ctx(Some(NonZeroU32::new(start).unwrap_or(NonZeroU32::new(2048).unwrap())));
            match model.new_context(backend, params) {
                Ok(ctx) => {
                    let mut obj = load_map();
                    obj.as_object_mut().unwrap().insert(key.clone(), serde_json::json!(start));
                    save_map(&obj);
                    return Ok((ctx, start));
                }
                Err(_) => {
                    attempts += 1; if attempts > 6 { return Err("failed to create context after autotune".to_string()); }
                    // back off conservatively
                    start = start.saturating_sub(1024).max(1024);
                }
            }
        }
    }

    pub fn chat(cfg: &LlmConfig, messages: &[ChatMessage]) -> Result<String, String> {
        // Try to get or load the shared model once; if no local model available, fall back to echo
        let shared = match get_or_load_shared(cfg) { Ok(s) => s, Err(_) => {
            // Fallback stub when no local model is available
            let prompt = messages.iter().map(|m| format!("{}: {}\n", m.role, m.content)).collect::<String>();
            return Ok(prompt);
        }};
        let backend = &shared.backend;
        let model = &shared.model;

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

        println!("{} LLM(llama.cpp): creating context (auto-tune n_ctx)...", chrono::Utc::now().to_rfc3339());
        let (mut ctx, tuned_ctx_len) = load_context_with_autotune(&model, &backend, &shared.model_path, cfg.context_length)?;
        println!("{} LLM(llama.cpp): context ready (n_ctx={})", chrono::Utc::now().to_rfc3339(), tuned_ctx_len);

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
        // Strict JSON early-stop collector
        let mut json_started = false;
        let mut json_depth: i32 = 0;
        let mut json_buf = String::new();
        let mut n_cur = batch.n_tokens();
        // cap new tokens; use smaller cap for STRICT JSON prompts
        let is_strict_json = prompt.contains("STRICT JSON") || prompt.contains("Output JSON:");
        let max_new_tokens: i32 = if is_strict_json {
            // Allow larger responses for STRICT JSON (batched outputs). Scale with context.
            ((tuned_ctx_len as i32) / 2).clamp(256, 2048)
        } else {
            ((tuned_ctx_len as i32) / 4).clamp(64, 1024)
        };
        let mut generated: i32 = 0;
        println!("{} LLM(llama.cpp): generating up to {} tokens...", chrono::Utc::now().to_rfc3339(), max_new_tokens);
        while generated < max_new_tokens {
            let token = sampler.sample(&ctx, batch.n_tokens() - 1);
            sampler.accept(token);
            if model.is_eog_token(token) { break; }
            let bytes = model.token_to_bytes(token, model::Special::Tokenize).map_err(|e| e.to_string())?;
            // Prefer strict UTF-8; skip invalid fragments instead of lossy decode to avoid garbage
            if let Ok(piece) = std::str::from_utf8(&bytes) {
                if is_strict_json {
                    for ch in piece.chars() {
                        if !json_started {
                            if ch == '{' { json_started = true; json_depth = 1; json_buf.push('{'); }
                            // ignore any preface before first '{'
                        } else {
                            json_buf.push(ch);
                            if ch == '{' { json_depth += 1; }
                            else if ch == '}' { json_depth -= 1; if json_depth == 0 { output = json_buf.clone(); break; } }
                        }
                    }
                } else {
                    output.push_str(piece);
                }
            } else {
                // Skip non-UTF8 token to avoid injecting replacement chars
            }
            batch.clear();
            batch.add(token, n_cur as i32, &[0], true).map_err(|e| e.to_string())?;
            n_cur += 1;
            generated += 1;
            ctx.decode(&mut batch).map_err(|e| e.to_string())?;
            // Early stop if strict JSON object was completed
            if is_strict_json && json_started && json_depth == 0 && !output.is_empty() { break; }
        }
        println!("{} LLM(llama.cpp): generation done ({} tokens)", chrono::Utc::now().to_rfc3339(), generated);
        Ok(output.trim().to_string())
    }
    pub fn embed(cfg: &LlmConfig, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        if texts.is_empty() { return Ok(Vec::new()); }
        // suppress logs
        send_logs_to_tracing(LogOptions::default().with_logs_enabled(false));
        let shared = match get_or_load_shared(cfg) {
            Ok(s) => s,
            Err(_) => {
                // fallback when no model: zeros
                return Ok(texts.iter().map(|_| vec![0.0f32; 768]).collect());
            }
        };
        let backend = &shared.backend;
        // prefer embed model if provided, otherwise chat model / auto-discover
        let mut cfg2 = cfg.clone();
        if cfg2.chat_model.is_none() { cfg2.chat_model = cfg2.embed_model.clone(); }
        let model = &shared.model;

        // enable embeddings with context auto-tune similar to chat
        let threads = std::thread::available_parallelism().map(|n| n.get() as i32).unwrap_or(4);
        // try cached/suggested context length and back off if needed
        fn s3_key() -> String {
            let tenant = Config::get_tenant();
            let workspace = Config::get_workspace_name();
            let pipeline = Config::get_pipeline_name();
            format!("{}/{}/{}/llm/llm_tuning.json", tenant, workspace, pipeline)
        }
        fn load_map() -> serde_json::Value {
            let key = s3_key();
            match tokio::runtime::Handle::try_current() {
                Ok(h) => h.block_on(async { crate::helpers::s3::get_json(&key).await }).ok().unwrap_or(serde_json::json!({})),
                Err(_) => {
                    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                    rt.block_on(async { crate::helpers::s3::get_json(&key).await }).ok().unwrap_or(serde_json::json!({}))
                }
            }
        }
        fn save_map(obj: &serde_json::Value) {
            let key = s3_key();
            let val = obj.clone();
            match tokio::runtime::Handle::try_current() {
                Ok(h) => { let _ = h.block_on(async { crate::helpers::s3::put_json(&key, &val).await }); }
                Err(_) => {
                    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                    let _ = rt.block_on(async { crate::helpers::s3::put_json(&key, &val).await });
                }
            }
        }
        let key_ctx = Path::new(&shared.model_path).canonicalize().map_err(|_| "model path".to_string())?.to_string_lossy().to_string() + "#ctx";
        let mut saved_ctx: Option<u32> = load_map().get(&key_ctx).and_then(|x| x.as_u64()).map(|n| n as u32);
        let mut start_ctx: u32 = saved_ctx.or_else(|| cfg.context_length.map(|x| x as u32)).unwrap_or(4096);
        let mut attempts = 0;
        let mut ctx = loop {
            let params = LlamaContextParams::default()
                .with_n_threads_batch(threads)
                .with_embeddings(true)
                .with_n_ctx(Some(NonZeroU32::new(start_ctx).unwrap_or(NonZeroU32::new(2048).unwrap())))
                .with_pooling_type(llama_cpp_2::context::params::LlamaPoolingType::Mean);
            match model.new_context(&backend, params) {
                Ok(ctx) => {
                    let mut obj = load_map();
                    obj.as_object_mut().unwrap().insert(key_ctx.clone(), serde_json::json!(start_ctx));
                    save_map(&obj);
                    break ctx;
                }
                Err(_) => {
                    attempts += 1; if attempts > 6 { return Err("failed to create embedding context after autotune".to_string()); }
                    start_ctx = start_ctx.saturating_sub(1024).max(1024);
                }
            }
        };

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


