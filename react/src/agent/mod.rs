use serde_json::Value;
use tracing::{info, warn};
use std::sync::Arc;

use crate::tools::ToolRegistry;
use crate::llm::ChatMessage;
use crate::session::{ThreadStore, ThreadStep, ThreadResult};
use crate::providers::{Keyspace, QueryProvider, RequestScope};
use crate::providers::DbtProvider;
use crate::adapters::storage::StorageAdapter;
use crate::providers::VectorStore;
use uuid::Uuid;
use async_trait::async_trait;

#[derive(Clone)]
pub struct AgentCtx {
    pub top_k: usize,
    pub per_step_timeout_secs: u64,
    pub max_steps: usize,
    pub thread_id: Option<String>,
    pub progress_tx: Option<tokio::sync::mpsc::UnboundedSender<usize>>,
    pub pre_step_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// Optional trace channel for streaming prompt/response summaries and tool actions.
    pub trace_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    pub agent_name: Option<String>,
    /// Suite-provided policy that defines what \"final\" means, what context to inject, and any
    /// required validations. This is the primary extension point that keeps the core loop agnostic.
    pub policy: Arc<dyn AgentPolicy>,
    /// LLM provider to use for the ReAct loop.
    pub llm: Arc<dyn crate::llm::LargeLanguageModel>,
    /// Storage adapter for reads/writes (S3, in-memory, etc.).
    pub storage: Arc<dyn StorageAdapter>,
    /// Authoritative request scope (tenant/workspace/project_id). Today comes from Config bootstrap;
    /// TODO(JWT): derive from WS handshake JWT claims and inject per-connection.
    pub scope: RequestScope,
    /// Canonical key/URI builder for scoped persistence.
    pub keyspace: Arc<dyn Keyspace>,
    /// Optional query provider (suite-provided). Used for existence checks and data access.
    pub query: Option<Arc<dyn QueryProvider>>,
    /// Optional DBT provider (suite-provided).
    pub dbt: Option<Arc<dyn DbtProvider>>,
    /// Optional vector store provider (suite-provided).
    pub vector: Option<Arc<dyn VectorStore>>,
    /// Optional thread store (for transcript persistence and artifact context).
    pub thread_store: Option<ThreadStore>,
    /// Resolved runtime configuration (from YAML + env + CLI), if available.
    pub resolved_config: Option<std::sync::Arc<crate::config::ReactResolvedConfig>>,
}

pub struct Agent;

pub enum RunOutcome {
    Final { thread_id: String, result: ThreadResult },
    AwaitUser { thread_id: String, prompt: String },
    AwaitApproval { thread_id: String, prompt: String },
}

pub enum Interrupt {
    AwaitUser { prompt: String },
    AwaitApproval { prompt: String },
}

#[async_trait]
pub trait AgentPolicy: Send + Sync {
    /// Extra transcript lines to inject after system/tool-card and before the user question.
    fn prelude_lines(&self, _ctx: &AgentCtx, _store: Option<&ThreadStore>, _thread_id: &str) -> Vec<String> {
        Vec::new()
    }

    /// Optional interrupt hook: after a tool action executes, policy may convert it into a control
    /// flow interrupt (await user / await approval). This keeps the core loop tool-name agnostic.
    fn interrupt_for_action(&self, _action_name: &str, _args: &Value, _obs: &Value) -> Option<Interrupt> {
        None
    }

    /// Handle a model-emitted `{ "final": {...} }`. Return:
    /// - `Ok(Some(RunOutcome::Final{..}))` to accept and finish
    /// - `Ok(None)` to reject and continue (policy should append an Observation to transcript)
    async fn handle_final(
        &self,
        tools: &ToolRegistry,
        ctx: &AgentCtx,
        transcript: &mut Vec<String>,
        store: Option<&ThreadStore>,
        thread_id: &str,
        final_obj: &Value,
    ) -> Result<Option<RunOutcome>, String>;

    /// If we exhaust steps without reaching an accepted final, produce a fallback.
    async fn fallback(
        &self,
        _tools: &ToolRegistry,
        _ctx: &AgentCtx,
        _transcript: &mut Vec<String>,
        _store: Option<&ThreadStore>,
        thread_id: &str,
    ) -> Result<RunOutcome, String> {
        Ok(RunOutcome::Final {
            thread_id: thread_id.to_string(),
            result: ThreadResult { sql: None, answer: "No result".to_string() },
        })
    }
}

/// Default policy: accept `final.answer` without any required validations.
pub struct DefaultPolicy;

#[async_trait]
impl AgentPolicy for DefaultPolicy {
    async fn handle_final(
        &self,
        _tools: &ToolRegistry,
        ctx: &AgentCtx,
        _transcript: &mut Vec<String>,
        store: Option<&ThreadStore>,
        thread_id: &str,
        final_obj: &Value,
    ) -> Result<Option<RunOutcome>, String> {
        let sql_opt = final_obj.get("sql").and_then(|x| x.as_str()).map(|s| s.to_string());
        let answer = final_obj
            .get("answer")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default();
        let result = ThreadResult { sql: sql_opt, answer };
        if let Some(store) = store {
            let _ = store
                .append_step(
                    thread_id,
                    ThreadStep {
                        action: "final".to_string(),
                        args: final_obj.clone(),
                        observation: serde_json::json!({"ok": true}),
                        ts: chrono::Utc::now().to_rfc3339(),
                        agent: ctx.agent_name.clone(),
                    },
                )
                .await;
        }
        Ok(Some(RunOutcome::Final { thread_id: thread_id.to_string(), result }))
    }
}

impl Agent {
    fn gen_uuid() -> String { Uuid::new_v4().to_string() }

    fn is_dbt_validate_blocked_error(s: &str) -> bool {
        // Keep this check stable and intentionally broad, as the error may be wrapped
        // by other layers before reaching the agent loop.
        s.contains("dbt_validate is blocked after a failed validation")
            || s.contains("Next step must be a mutating fix action")
    }

    fn is_mutating_action(action_name: &str, args: &Value) -> bool {
        match action_name {
            "approve_and_save_artifact" => true,
            "approve_and_save_artifact_batch" => true,
            "staging_model" => true,
            "dbt_files" => args
                .get("op")
                .and_then(|v| v.as_str())
                .map(|s| s == "put")
                .unwrap_or(false),
            _ => false,
        }
    }

    fn summarize_args(action_name: &str, args: &Value) -> String {
        match action_name {
            "dbt_validate" => {
                let build = args.get("build").and_then(|v| v.as_bool()).unwrap_or(false);
                let run = args.get("run").and_then(|v| v.as_bool()).unwrap_or(false);
                let select = args.get("select").and_then(|v| v.as_str()).unwrap_or("");
                let mut parts = vec![format!("build={}", build), format!("run={}", run)];
                if !select.is_empty() {
                    parts.push(format!("select='{}'", Self::truncate_line(select, 80)));
                }
                parts.join(" ")
            }
            "dbt_files" => {
                let op = args.get("op").and_then(|v| v.as_str()).unwrap_or("get");
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let pointer = args.get("pointer").and_then(|v| v.as_str()).unwrap_or("");
                let mut parts = vec![format!("op={}", op)];
                if !path.is_empty() {
                    parts.push(format!("path='{}'", Self::truncate_line(path, 120)));
                }
                if !pointer.is_empty() {
                    parts.push(format!("ptr='{}'", Self::truncate_line(pointer, 80)));
                }
                parts.join(" ")
            }
            "run_sql" => {
                let sql = args.get("sql").and_then(|v| v.as_str()).unwrap_or("");
                format!("sql='{}'", Self::truncate_line(&sql.replace('\n', " "), 180))
            }
            "approve_and_save_artifact_batch" => {
                let n = args
                    .get("items")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                format!("items={}", n)
            }
            "approve_and_save_artifact" => {
                let kind = args.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let mut parts: Vec<String> = Vec::new();
                if !kind.is_empty() {
                    parts.push(format!("kind={}", kind));
                }
                if !name.is_empty() {
                    parts.push(format!("name={}", Self::truncate_line(name, 80)));
                }
                if !path.is_empty() {
                    parts.push(format!("path='{}'", Self::truncate_line(path, 120)));
                }
                if parts.is_empty() {
                    Self::truncate_line(&args.to_string(), 220)
                } else {
                    parts.join(" ")
                }
            }
            _ => Self::truncate_line(&args.to_string(), 260),
        }
    }

    fn summarize_observation(action_name: &str, obs: &Value) -> String {
        let ok = obs.get("ok").and_then(|v| v.as_bool()).unwrap_or(true);
        if let Some(err) = obs.get("error").and_then(|v| v.as_str()) {
            return format!("ok={} error='{}'", ok, Self::truncate_line(err, 900));
        }
        match action_name {
            "dbt_validate" => {
                let compile_ok = obs.get("compile_ok").and_then(|v| v.as_bool()).unwrap_or(false);
                let run_ok = obs.get("run_ok").and_then(|v| v.as_bool()).unwrap_or(false);
                let parse_ok = obs.get("parse_ok").and_then(|v| v.as_bool()).unwrap_or(false);
                let deps_ok = obs.get("deps_ok").and_then(|v| v.as_bool()).unwrap_or(false);
                let n_errors = obs.get("errors").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
                let first_err = obs
                    .get("errors")
                    .and_then(|v| v.as_array())
                    .and_then(|a| a.first())
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if n_errors > 0 {
                    format!(
                        "ok={} compile_ok={} run_ok={} parse_ok={} deps_ok={} errors={} first='{}'",
                        ok,
                        compile_ok,
                        run_ok,
                        parse_ok,
                        deps_ok,
                        n_errors,
                        Self::truncate_line(first_err, 500)
                    )
                } else {
                    format!(
                        "ok={} compile_ok={} run_ok={} parse_ok={} deps_ok={}",
                        ok, compile_ok, run_ok, parse_ok, deps_ok
                    )
                }
            }
            "approve_and_save_artifact_batch" => {
                let n_files = obs.get("files").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
                format!("ok={} files={}", ok, n_files)
            }
            "approve_and_save_artifact" => {
                let key = obs.get("key").and_then(|v| v.as_str()).unwrap_or("");
                if !key.is_empty() {
                    format!("ok={} key='{}'", ok, Self::truncate_line(key, 140))
                } else {
                    format!("ok={}", ok)
                }
            }
            "dbt_files" => {
                // IMPORTANT: dbt_files get often returns file contents; keep it, but cap it.
                let path = obs.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let status = obs.get("status").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(content) = obs.get("content").and_then(|v| v.as_str()) {
                    let c = Self::truncate_line(content, 12_000);
                    if !status.is_empty() {
                        format!(
                            "ok={} path='{}' status={} content:\n{}",
                            ok,
                            Self::truncate_line(path, 120),
                            status,
                            c
                        )
                    } else {
                        format!(
                            "ok={} path='{}' content:\n{}",
                            ok,
                            Self::truncate_line(path, 120),
                            c
                        )
                    }
                } else {
                    if !status.is_empty() {
                        format!(
                            "ok={} path='{}' status={}",
                            ok,
                            Self::truncate_line(path, 120),
                            status
                        )
                    } else if !path.is_empty() {
                        format!("ok={} path='{}'", ok, Self::truncate_line(path, 120))
                    } else {
                        format!("ok={}", ok)
                    }
                }
            }
            "run_sql" => {
                let rows = obs.get("rows").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
                format!("ok={} rows={}", ok, rows)
            }
            _ => format!("ok={}", ok),
        }
    }

    fn trim_transcript(transcript: &mut Vec<String>) {
        // Keep the prompt bounded: preserve the initial context up through the Question,
        // then retain only the most recent action/observation lines.
        const MAX_TAIL_LINES: usize = 120;
        const MAX_TOTAL_LINES: usize = 220;
        if transcript.len() <= MAX_TOTAL_LINES {
            return;
        }
        let mut prefix_end = 0usize;
        for (i, line) in transcript.iter().enumerate() {
            if line.starts_with("Question:") {
                prefix_end = i + 1;
                break;
            }
        }
        if prefix_end == 0 {
            // Unexpected; keep last N lines only.
            let start = transcript.len().saturating_sub(MAX_TAIL_LINES);
            *transcript = transcript[start..].to_vec();
            return;
        }
        let tail_start = transcript.len().saturating_sub(MAX_TAIL_LINES);
        let keep_from = tail_start.max(prefix_end);
        let mut out: Vec<String> = Vec::new();
        out.extend_from_slice(&transcript[..prefix_end]);
        out.push("Observation: (context trimmed; older steps omitted)".to_string());
        out.extend_from_slice(&transcript[keep_from..]);
        *transcript = out;
    }

    fn chunk_text(s: &str, max_chars: usize) -> Vec<String> {
        if s.is_empty() {
            return vec![];
        }
        let max_chars = max_chars.max(256);
        let mut out: Vec<String> = Vec::new();
        let mut buf = String::new();
        for line in s.lines() {
            // Keep line breaks to preserve dbt line numbers and context.
            let piece = if buf.is_empty() { line.to_string() } else { format!("\n{}", line) };
            if buf.len() + piece.len() <= max_chars {
                buf.push_str(&piece);
            } else {
                if !buf.is_empty() {
                    out.push(buf);
                    buf = String::new();
                }
                // If a single line is too large, hard-split it.
                if line.len() > max_chars {
                    let mut start = 0usize;
                    let chars: Vec<char> = line.chars().collect();
                    while start < chars.len() {
                        let end = (start + max_chars).min(chars.len());
                        out.push(chars[start..end].iter().collect::<String>());
                        start = end;
                    }
                } else {
                    buf.push_str(line);
                }
            }
            if out.len() >= 40 {
                // Safety cap; we don't want unbounded LLM calls.
                break;
            }
        }
        if !buf.is_empty() && out.len() < 40 {
            out.push(buf);
        }
        out
    }

    fn extract_error_blob_for_chunking(step: &crate::session::ThreadStep) -> Option<String> {
        // Prefer structured dbt errors array, else fallback to a large error string.
        if let Some(arr) = step.observation.get("errors").and_then(|v| v.as_array()) {
            let mut out = String::new();
            for e in arr.iter().filter_map(|v| v.as_str()) {
                out.push_str(e);
                out.push('\n');
            }
            if out.trim().len() >= 4000 {
                return Some(out);
            }
        }
        if let Some(e) = step.observation.get("error").and_then(|v| v.as_str()) {
            if e.trim().len() >= 4000 {
                return Some(e.to_string());
            }
        }
        None
    }

    async fn llm_chat_once(ctx: &AgentCtx, prompt: String) -> Result<String, String> {
        let model = ctx.llm.clone();
        tokio::task::spawn_blocking(move || model.chat(&[ChatMessage { role: "user".into(), content: prompt }]))
            .await
            .map_err(|e| format!("LLM execution failed: {}", e))?
            .map_err(|e| format!("LLM request failed: {}", e))
    }

    async fn llm_action_via_chunked_errors(
        ctx: &AgentCtx,
        transcript: &[String],
        step_idx: usize,
        error_blob: &str,
    ) -> Result<String, String> {
        // We split the error log into chunks and ask the model to build an incremental diagnosis state.
        // Then we ask for the next action using only the compact transcript + the final state.
        let base = Self::minimal_base_for_chunking(transcript);
        let chunk_budget = 40_000usize;
        let chunks = Self::chunk_text(error_blob, chunk_budget);
        if chunks.is_empty() {
            return Self::llm_chat_once(ctx, base + &format!("\n\nStep {}: Decide next action.", step_idx + 1)).await;
        }

        let mut state_json = "{\"summary\":\"\",\"failing_models\":[],\"key_errors\":[],\"next_fix_hint\":\"\"}".to_string();
        for (i, ch) in chunks.iter().enumerate() {
            let prompt = format!(
                "{base}\n\n\
                 You are being shown a LONG error log in chunks so you can see ALL details.\n\
                 Update your internal diagnosis state ONLY.\n\
                 Respond with STRICT JSON only.\n\n\
                 Chunk {i1}/{n}:\n{chunk}\n\n\
                 CurrentStateJSON:\n{state}\n\n\
                 Output JSON schema:\n\
                 {{\"summary\":string,\"failing_models\":[string],\"key_errors\":[string],\"next_fix_hint\":string}}\n",
                base = base,
                i1 = i + 1,
                n = chunks.len(),
                chunk = ch,
                state = state_json
            );
            let raw = Self::llm_chat_once(ctx, prompt).await?;
            // Best-effort parse; if invalid, keep previous state and continue.
            if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                if v.is_object() {
                    state_json = v.to_string();
                }
            }
            // Cap iterations
            if i + 1 >= 20 {
                break;
            }
        }

        let final_prompt = format!(
            "{base}\n\n\
             The full error log was provided in prior chunked requests.\n\
             You MUST now choose ONE next action.\n\
             IMPORTANT: Do NOT run validation/build tools (e.g. dbt_validate / publish) as the very next step.\n\
             First take a FIX step based on the diagnosis state: inspect the relevant model SQL/artifact, then edit/regenerate it.\n\
             Only after applying a fix should you run validation again.\n\
             Use the diagnosis state below to guide your choice.\n\
             Respond with STRICT JSON only, per the tool schemas.\n\n\
             DiagnosisStateJSON:\n{state}\n\n\
             Step {step}: Decide next action.",
            base = base,
            state = state_json,
            step = step_idx + 1
        );
        Self::llm_chat_once(ctx, final_prompt).await
    }

    fn minimal_base_for_chunking(transcript: &[String]) -> String {
        // Crude + simple: keep system prompt + tool card + prelude + question only.
        // Drop all prior Action/Observation lines to avoid context explosion.
        let mut out: Vec<String> = Vec::new();
        for line in transcript.iter() {
            out.push(line.clone());
            if line.starts_with("Question:") {
                break;
            }
        }
        out.join("\n\n")
    }

    fn trace_enabled(ctx: &AgentCtx) -> bool {
        // Simple opt-in switch. Keep defaults safe.
        if let Ok(v) = std::env::var("REACT_TRACE") {
            let vv = v.trim().to_lowercase();
            if vv == "1" || vv == "true" || vv == "yes" { return true; }
        }
        // Future: allow enabling via resolved_config.logging once added.
        let _ = ctx;
        false
    }

    fn persist_parse_errors_enabled() -> bool {
        // Default: on. Allow opt-out via env.
        if let Ok(v) = std::env::var("REACT_PERSIST_PARSE_ERRORS") {
            let vv = v.trim().to_lowercase();
            if vv == "0" || vv == "false" || vv == "no" {
                return false;
            }
        }
        true
    }

    fn trace_send(ctx: &AgentCtx, line: String) {
        if let Some(tx) = ctx.trace_tx.as_ref() {
            let _ = tx.send(line);
        }
    }

    fn truncate_line(s: &str, max: usize) -> String {
        if s.len() <= max { return s.to_string(); }
        match s.char_indices().take_while(|(i, _)| *i < max).last() {
            Some((i, _)) => format!("{}…", &s[..i]),
            None => s.chars().take(max).collect(),
        }
    }

    pub async fn run_until_block(
        tools: &ToolRegistry,
        ctx: &AgentCtx,
        system_prompt: &str,
        tool_card: &str,
        user_prompt: &str,
    ) -> Result<RunOutcome, String> {
        let thread_id = ctx.thread_id.clone().unwrap_or_else(|| Agent::gen_uuid());
        let store_opt = ctx.thread_store.as_ref();
        let mut transcript: Vec<String> = Vec::new();
        transcript.push(system_prompt.to_string());
        transcript.push(tool_card.to_string());
        transcript.extend(ctx.policy.prelude_lines(ctx, store_opt, &thread_id));
        transcript.push(format!("Question: {}", user_prompt));

        // Capture the most recent large error blob from the persisted thread steps, if any.
        // We keep prompts compact, but we can still feed the FULL error log via chunked requests when needed.
        let mut last_error_blob: Option<String> = None;
        if let Some(store) = store_opt {
            if let Some(prev) = store.get(&thread_id).await {
                for step in prev.steps {
                    if let Some(blob) = Self::extract_error_blob_for_chunking(&step) {
                        last_error_blob = Some(blob);
                    }
                    // Rehydrate a compact transcript for the LLM (avoid replaying giant logs/diffs forever).
                    transcript.push(format!(
                        "Action: {} Args: {}",
                        step.action,
                        Self::summarize_args(&step.action, &step.args)
                    ));
                    transcript.push(format!(
                        "Observation: {}",
                        Self::summarize_observation(&step.action, &step.observation)
                    ));
                    Self::trim_transcript(&mut transcript);
                }
            }
        }

        // Hard control flow: if dbt_validate is blocked, we MUST apply a mutating fix next.
        // If we fail to perform a mutation 3 times, we stop and ask the user.
        let mut must_apply_mutation: bool = false;
        let mut mutation_failures: u8 = 0;

        // Simple loop-guard memory: last few short, repeating failures
        let mut recent: std::collections::VecDeque<(String, String, u128)> = std::collections::VecDeque::with_capacity(8);
        for step_idx in 0..ctx.max_steps {
            info!("ReAct step {}", step_idx + 1);
            if Self::trace_enabled(ctx) {
                let a = ctx.agent_name.clone().unwrap_or_else(|| "agent".to_string());
                tracing::info!("TRACE({}): step {}", a, step_idx + 1);
                Self::trace_send(ctx, format!("step {}: thinking…", step_idx + 1));
            }
            let mut prompt = transcript.join("\n\n");
            if must_apply_mutation {
                prompt.push_str(
                    "\n\nCONSTRAINT (hard): dbt_validate is currently blocked until you APPLY A MUTATING FIX.\n\
                     Your next action MUST be one of:\n\
                     - approve_and_save_artifact\n\
                     - approve_and_save_artifact_batch\n\
                     - staging_model\n\
                     - dbt_files with op='put'\n\
                     Do NOT call dbt_validate again until after a successful mutation.\n\
                     If you cannot produce a valid mutation after a few attempts, ask_user for guidance."
                );
            }
            prompt.push_str(&format!("\n\nStep {}: Decide next action.", step_idx + 1));
            // Track LLM expense (chat) for this decision
            let mut chat_chars_in: usize = prompt.len();
            let mut chat_chars_out: usize = 0;
            if Self::trace_enabled(ctx) {
                let p = Self::truncate_line(&prompt, 240);
                tracing::info!("TRACE: llm_prompt_chars={} prompt='{}'", prompt.len(), p);
                Self::trace_send(ctx, format!("llm prompt ({} chars): {}", prompt.len(), p));
            }
            // If the prompt is still very large and we have a large error blob, prefer chunked error flow
            // instead of risking context window issues or ballooning costs.
            let use_chunked = prompt.len() > 120_000 && last_error_blob.as_deref().map(|s| !s.is_empty()).unwrap_or(false);
            let mut act_json = match if use_chunked {
                Self::llm_action_via_chunked_errors(ctx, &transcript, step_idx, last_error_blob.as_deref().unwrap_or("")).await
            } else {
                Self::llm_chat_once(ctx, prompt.clone()).await
            } {
                Ok(s) => s,
                Err(e) => {
                    let el = e.to_lowercase();
                    if el.contains("http 429") || el.contains("status code 429") || el.contains("rate limit") {
                        return Err(format!("LLM rate limited (429): {}", e));
                    }
                    if el.contains("context_length_exceeded") || el.contains("exceeds the context window") {
                        // Instead of shrinking away critical errors, feed the full error log in multiple prompts.
                        if let Some(blob) = last_error_blob.as_deref() {
                            let s2 = Self::llm_action_via_chunked_errors(ctx, &transcript, step_idx, blob).await?;
                            chat_chars_in += s2.len().min(0); // keep existing accounting; we attach detailed expense per tool anyway
                            chat_chars_out += s2.len();
                            s2
                        } else {
                            return Err(format!("LLM request failed: {}", e));
                        }
                    } else {
                        return Err(format!("LLM request failed: {}", e));
                    }
                }
            };
            chat_chars_out += act_json.len();
            if Self::trace_enabled(ctx) {
                let r = Self::truncate_line(&act_json, 240);
                tracing::info!("TRACE: llm_reply_chars={} reply='{}'", act_json.len(), r);
                Self::trace_send(ctx, format!("llm reply ({} chars): {}", act_json.len(), r));
            }
            // Parse JSON, with one repair attempt if invalid
            let mut parsed: Option<Value> = serde_json::from_str(&act_json).ok();
            if parsed.is_none() {
                warn!("Invalid JSON action from LLM; requesting strict JSON re-emission.");
                let base = Self::minimal_base_for_chunking(&transcript);
                let bad = Self::truncate_line(&act_json, 2000);
                let repair_prompt = format!(
                    "{}\n\nPrevious output was not valid JSON (truncated):\n{}\n\nRe-emit STRICT JSON ONLY per the schemas. No prose.",
                    base,
                    bad
                );
                let prompt_clone2 = repair_prompt.clone();
                let model_for_second = ctx.llm.clone();
                let act_json2 = match tokio::task::spawn_blocking(move || {
                    model_for_second.chat(&[ChatMessage { role: "user".into(), content: prompt_clone2 }])
                }).await {
                    Ok(Ok(s)) => s,
                    Ok(Err(e)) => {
                        return Err(format!("LLM not configured: {}", e));
                    }
                    Err(e) => {
                        return Err(format!("LLM execution failed: {}", e));
                    }
                };
                parsed = serde_json::from_str(&act_json2).ok();
                chat_chars_in += repair_prompt.len();
                chat_chars_out += act_json2.len();
                if Self::trace_enabled(ctx) {
                    let r = Self::truncate_line(&act_json2, 240);
                    tracing::info!("TRACE: llm_repair_reply_chars={} reply='{}'", act_json2.len(), r);
                    Self::trace_send(ctx, format!("llm repair reply ({} chars): {}", act_json2.len(), r));
                }
                if parsed.is_none() {
                    // Record the failure (truncated) and continue loop.
                    let raw1_len = act_json.len();
                    let raw2_len = act_json2.len();
                    let raw1_trunc = Self::truncate_line(&act_json, 800);
                    let raw2_trunc = Self::truncate_line(&act_json2, 800);
                    transcript.push(format!(
                        "Observation: parser_error invalid JSON twice; raw_len={} raw='{}'",
                        raw2_len, raw2_trunc
                    ));

                    // Persist parse failures into thread history so they appear in `thread.json`.
                    if Self::persist_parse_errors_enabled() {
                        if let Some(store) = store_opt {
                            let _ = store
                                .append_step(
                                    &thread_id,
                                    ThreadStep {
                                        action: "parser_error".to_string(),
                                        args: serde_json::json!({
                                            "kind": "invalid_json_twice",
                                            "raw1_len": raw1_len,
                                            "raw2_len": raw2_len,
                                            "raw1_trunc": raw1_trunc,
                                            "raw2_trunc": raw2_trunc
                                        }),
                                        observation: serde_json::json!({"ok": false, "error": "invalid_json"}),
                                        ts: chrono::Utc::now().to_rfc3339(),
                                        agent: ctx.agent_name.clone(),
                                    },
                                )
                                .await;
                        }
                    }
                    continue;
                }
            }
            let parsed = parsed.unwrap();
            if let Some(final_obj) = parsed.get("final") {
                if must_apply_mutation {
                    mutation_failures = mutation_failures.saturating_add(1);
                    transcript.push("Observation: policy_error - cannot finalize while dbt_validate is blocked; you MUST apply a mutating fix next.".to_string());
                    if mutation_failures >= 3 {
                        return Ok(RunOutcome::AwaitUser {
                            thread_id,
                            prompt: "Auto-remediation is stuck: dbt_validate is blocked until a mutating fix is applied, but the agent failed to produce a valid mutation three times.\n\nPlease tell me what to change (e.g., which model/file to edit), or apply the fix manually and re-run.".to_string(),
                        });
                    }
                    continue;
                }
                if let Some(outcome) = ctx
                    .policy
                    .handle_final(tools, ctx, &mut transcript, store_opt, &thread_id, final_obj)
                    .await?
                {
                    return Ok(outcome);
                }
                    continue;
            }
            let action_name = parsed.get("action").and_then(|x| x.as_str()).unwrap_or_default().to_string();
            // Skip empty/invalid action names to avoid logging noisy empty steps
            if action_name.trim().is_empty() {
                transcript.push("Observation: invalid action - empty; retrying next step.".to_string());
                continue;
            }

            let args = parsed.get("args").cloned().unwrap_or(Value::Null);

            // Hard control flow gate: when dbt_validate is blocked, refuse non-mutating actions.
            if must_apply_mutation && !Self::is_mutating_action(&action_name, &args) {
                mutation_failures = mutation_failures.saturating_add(1);
                transcript.push(format!(
                    "Observation: policy_error - action '{}' is not allowed right now. You MUST apply a mutating fix (approve_and_save_artifact(_batch), staging_model, or dbt_files op=put).",
                    action_name
                ));
                if mutation_failures >= 3 {
                    return Ok(RunOutcome::AwaitUser {
                        thread_id,
                        prompt: "Auto-remediation is stuck: dbt_validate is blocked until a mutating fix is applied, but the agent failed to select/perform an allowed mutation three times.\n\nPlease specify what to change (file/model), or apply the fix manually and re-run.".to_string(),
                    });
                }
                continue;
            }
            if let Some(tx) = ctx.pre_step_tx.as_ref() {
                let _ = tx.send(action_name.clone());
            }
            if Self::trace_enabled(ctx) {
                tracing::info!("TRACE: tool_call {}", action_name);
                Self::trace_send(ctx, format!("tool: {}", action_name));
            }
            // Guardrail: prevent oversized batch tool calls from creating huge JSON payloads
            // that are likely to be truncated by LLM output limits.
            if action_name == "approve_and_save_artifact_batch" {
                let max_items = 20usize;
                let n_items = args
                    .get("items")
                    .and_then(|x| x.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                if n_items > max_items {
                    transcript.push(format!(
                        "Observation: approve_and_save_artifact_batch too_large items={} max={}; split into multiple calls (<= {} items) and retry.",
                        n_items, max_items, max_items
                    ));
                    continue;
                }
            }
            let start = std::time::Instant::now();
            let mut obs = match tools.call(&action_name, args.clone(), ctx).await {
                Ok(o) => o,
                Err(e) => serde_json::json!({"ok": false, "error": e}),
            };
            if Self::trace_enabled(ctx) {
                let ok = obs.get("ok").and_then(|x| x.as_bool()).unwrap_or(true);
                let dur_ms = start.elapsed().as_millis();
                let summ = Self::truncate_line(&obs.to_string(), 240);
                tracing::info!("TRACE: tool_result {} ok={} dur_ms={} obs={}", action_name, ok, dur_ms, summ);
                Self::trace_send(ctx, format!("tool result: {} ({}ms) {}", action_name, dur_ms, if ok { "ok" } else { "error" }));
            }
            // Attach LLM expense estimate (tokens only) for this step
            let est_tokens = ((chat_chars_in + chat_chars_out) as f32 / 4.0).round() as i64;
            let expense = serde_json::json!({
                "chat_chars_in": chat_chars_in,
                "chat_chars_out": chat_chars_out,
                "est_tokens": est_tokens
            });
            if let Some(map) = obs.as_object_mut() {
                map.insert("llm_expense".to_string(), expense);
            }
            let dur_ms = start.elapsed().as_millis();

            // Update hard control-flow state based on tool outcomes.
            if action_name == "dbt_validate" {
                if let Some(e) = obs.get("error").and_then(|x| x.as_str()) {
                    if Self::is_dbt_validate_blocked_error(e) {
                        must_apply_mutation = true;
                        mutation_failures = 0;
                    }
                }
            }
            if must_apply_mutation && Self::is_mutating_action(&action_name, &args) {
                let ok = obs.get("ok").and_then(|x| x.as_bool()).unwrap_or(false);
                if ok {
                    must_apply_mutation = false;
                    mutation_failures = 0;
                } else {
                    mutation_failures = mutation_failures.saturating_add(1);
                    if mutation_failures >= 3 {
                        return Ok(RunOutcome::AwaitUser {
                            thread_id,
                            prompt: "Tried to apply a mutating fix three times but the mutation tool calls failed.\n\nPlease review the last error and tell me how you want to proceed (or apply the fix manually and re-run).".to_string(),
                        });
                    }
                }
            }

            // Keep the LLM-facing transcript compact.
            transcript.push(format!(
                "Action: {} Args: {}",
                action_name,
                Self::summarize_args(&action_name, &args)
            ));
            transcript.push(format!(
                "Observation: {}",
                Self::summarize_observation(&action_name, &obs)
            ));
            Self::trim_transcript(&mut transcript);
            if let Some(store) = store_opt {
                let _ = store.append_step(&thread_id, ThreadStep {
                    action: action_name.clone(),
                    args: args.clone(),
                    observation: obs.clone(),
                    ts: chrono::Utc::now().to_rfc3339(),
                    agent: ctx.agent_name.clone(),
                }).await;
            }
            if let Some(interrupt) = ctx.policy.interrupt_for_action(&action_name, &args, &obs) {
                match interrupt {
                    Interrupt::AwaitUser { prompt } => return Ok(RunOutcome::AwaitUser { thread_id, prompt }),
                    Interrupt::AwaitApproval { prompt } => return Ok(RunOutcome::AwaitApproval { thread_id, prompt }),
                }
            }
            // Loop-guard bookkeeping
            let err_snippet = obs.get("error").and_then(|x| x.as_str()).unwrap_or("").chars().take(64).collect::<String>();
            if !err_snippet.is_empty() {
                if recent.len() == 8 { recent.pop_front(); }
                recent.push_back((action_name.clone(), err_snippet.clone(), dur_ms));
                // If 5 most recent are the same action+error and each < 300ms and total < 3000ms → stop
                if recent.len() >= 5 {
                    let n = recent.len();
                    let window = &recent.as_slices().0[(n-5)..];
                    let same = window.iter().all(|(a,e,_)| a == &action_name && e == &err_snippet);
                    let fast = window.iter().all(|(_,_,d)| *d < 300);
                    let total: u128 = window.iter().map(|(_,_,d)| *d).sum();
                    if same && fast && total < 3000 {
                        let result = ThreadResult { sql: None, answer: format!("Halting due to rapid repeated '{}' errors. Last error: {}", action_name, err_snippet) };
                        if let Some(store) = store_opt {
                            let _ = store.append_step(&thread_id, ThreadStep {
                                action: "final".to_string(),
                                args: serde_json::json!({"answer": result.answer, "sql": null}),
                                observation: serde_json::json!({"ok": true}),
                                ts: chrono::Utc::now().to_rfc3339(),
                                agent: ctx.agent_name.clone(),
                            }).await;
                        }
                        return Ok(RunOutcome::Final { thread_id, result });
                    }
                }
            }
            if let Some(tx) = ctx.progress_tx.as_ref() {
                let _ = tx.send(step_idx + 1);
            }
        }
        ctx.policy.fallback(tools, ctx, &mut transcript, store_opt, &thread_id).await
    }

    // NOTE: legacy `run` removed (it duplicated logic). Prefer `run_until_block`.
}


