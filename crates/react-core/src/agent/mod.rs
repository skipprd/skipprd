use serde_json::Value;
use tracing::{info, warn};
use std::sync::Arc;
use std::any::Any;

use crate::tools::ToolRegistry;
use crate::llm::ChatMessage;
use crate::session::{ThreadStore, ThreadStep, ThreadResult};
use crate::keyspace::Keyspace;
use crate::providers::{QueryProvider, DbtProvider, VectorStore};
use crate::storage::StorageAdapter;
use crate::scope::RequestScope;
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
    /// Authoritative request scope (tenant/workspace/project_id).
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

    /// Optional runtime-specific context/configuration blob (type-erased).
    ///
    /// Suites/tools may downcast this to access runtime wiring/config without
    /// coupling the core runner to any particular config type.
    pub runtime: Option<Arc<dyn Any + Send + Sync>>,
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

    /// Optional per-tool timeout override (in seconds).
    ///
    /// By default, tool calls are bounded by `AgentCtx.per_step_timeout_secs`. Suites can raise the
    /// timeout for known-slow tools (e.g. warehouse queries or LLM-backed generators) without
    /// globally increasing the timeout for every tool.
    fn timeout_for_tool(&self, _action_name: &str) -> Option<u64> {
        None
    }

    /// Handle a model-emitted `{ \"final\": {...} }`. Return:
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

    /// Some model backends emit "JSON-like" text with literal control characters (e.g. raw newlines)
    /// inside string values. That is invalid JSON and `serde_json` will reject it.
    ///
    /// This function repairs ONLY those invalid characters inside string literals by escaping them.
    /// It is intentionally conservative: it does not try to fix other kinds of malformed JSON.
    fn escape_control_chars_in_json_strings(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 8);
        let mut in_str = false;
        let mut esc = false;
        for ch in s.chars() {
            if in_str {
                if esc {
                    // Preserve whatever was escaped (including escaped newlines like \n).
                    out.push(ch);
                    esc = false;
                    continue;
                }
                if ch == '\\' {
                    out.push(ch);
                    esc = true;
                    continue;
                }
                match ch {
                    // Escape raw control characters that are illegal in JSON strings.
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    '\u{08}' => out.push_str("\\b"),
                    '\u{0C}' => out.push_str("\\f"),
                    '"' => {
                        out.push(ch);
                        in_str = false;
                    }
                    c if (c as u32) < 0x20 => {
                        // Any remaining control chars -> \u00XX
                        out.push_str(&format!("\\u{:04x}", c as u32));
                    }
                    _ => out.push(ch),
                }
                continue;
            }

            // Not in string
            if esc {
                out.push(ch);
                esc = false;
                continue;
            }
            match ch {
                '"' => {
                    out.push(ch);
                    in_str = true;
                }
                '\\' => {
                    // Outside strings this is still meaningful JSON (e.g. escapes in whitespace-less JSON5-ish),
                    // but we preserve it.
                    out.push(ch);
                    esc = true;
                }
                _ => out.push(ch),
            }
        }
        out
    }

    fn minimal_base_for_chunking(transcript: &[String]) -> String {
        // Keep only the last ~50 transcript lines to keep prompt size bounded.
        let keep = 50usize.min(transcript.len());
        let tail = &transcript[transcript.len().saturating_sub(keep)..];
        let mut out = String::new();
        for line in tail {
            out.push_str(line);
            out.push('\n');
        }
        out
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

        let prompt = format!(
            "{base}\n\n\
             DiagnosisStateJSON:\n{state}\n\n\
             Step {step}: Decide next action.\n\
             Respond with STRICT JSON only.\n",
            base = base,
            state = state_json,
            step = step_idx + 1
        );
        Self::llm_chat_once(ctx, prompt).await
    }

    fn parse_action(raw: &str) -> Result<Value, String> {
        // Be forgiving: some model backends may emit multiple JSON objects in one response
        // (e.g. a tool action followed by a final). We accept the FIRST valid top-level JSON
        // object and ignore trailing content.
        match serde_json::from_str::<Value>(raw) {
            Ok(v) => Ok(v),
            Err(e) => {
                let trimmed = raw.trim();
                // Attempt to extract the first {...} or [...] JSON value by brace matching.
                if let Some(first) = Self::extract_first_json_value(trimmed) {
                    // First try strict parse of the extracted value.
                    if let Ok(v) = serde_json::from_str::<Value>(&first) {
                        return Ok(v);
                    }
                    // Then try a conservative repair: escape control chars inside strings (raw newlines, etc).
                    let repaired = Self::escape_control_chars_in_json_strings(&first);
                    serde_json::from_str::<Value>(&repaired)
                        .map_err(|e2| format!("invalid JSON from model: {} (original error: {})", e2, e))
                } else {
                    Err(format!("invalid JSON from model: {}", e))
                }
            }
        }
    }

    fn extract_first_json_value(s: &str) -> Option<String> {
        let mut start: Option<usize> = None;
        let mut stack: Vec<char> = Vec::new();
        let mut in_str = false;
        let mut esc = false;
        for (i, ch) in s.char_indices() {
            if start.is_none() {
                if ch == '{' || ch == '[' {
                    start = Some(i);
                    stack.push(ch);
                }
                continue;
            }

            if in_str {
                if esc {
                    esc = false;
                    continue;
                }
                if ch == '\\' {
                    esc = true;
                    continue;
                }
                if ch == '"' {
                    in_str = false;
                }
                continue;
            }

            match ch {
                '"' => in_str = true,
                '{' | '[' => stack.push(ch),
                '}' => {
                    if matches!(stack.pop(), Some('{')) && stack.is_empty() {
                        let st = start?;
                        return Some(s[st..=i].to_string());
                    }
                }
                ']' => {
                    if matches!(stack.pop(), Some('[')) && stack.is_empty() {
                        let st = start?;
                        return Some(s[st..=i].to_string());
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn transcript_add(transcript: &mut Vec<String>, line: String, tx: &Option<tokio::sync::mpsc::UnboundedSender<String>>) {
        if let Some(t) = tx.as_ref() {
            let _ = t.send(line.clone());
        }
        transcript.push(line);
    }

    pub async fn run_until_block(
        tools: &ToolRegistry,
        ctx: &AgentCtx,
        system_prompt: &str,
        tools_card: &str,
        question: &str,
    ) -> Result<RunOutcome, String> {
        let tid = ctx.thread_id.clone().unwrap_or_else(Self::gen_uuid);
        let store = ctx.thread_store.as_ref();

        // Transcript is plain-text lines the model sees.
        let mut transcript: Vec<String> = Vec::new();
        Self::transcript_add(&mut transcript, format!("System: {}", system_prompt), &ctx.trace_tx);
        Self::transcript_add(&mut transcript, format!("Tools: {}", tools_card), &ctx.trace_tx);

        // Suite/policy may inject extra context.
        for l in ctx.policy.prelude_lines(ctx, store, &tid) {
            Self::transcript_add(&mut transcript, l, &ctx.trace_tx);
        }

        Self::transcript_add(&mut transcript, format!("User: {}", question), &ctx.trace_tx);

        for step_idx in 0..ctx.max_steps {
            if let Some(tx) = ctx.progress_tx.as_ref() {
                let _ = tx.send(step_idx);
            }
            if let Some(tx) = ctx.pre_step_tx.as_ref() {
                let _ = tx.send(format!("step {}", step_idx + 1));
            }

            // Ask model for next action.
            let prompt = transcript.join("\n");
            let raw = Self::llm_chat_once(ctx, prompt).await?;
            let action = Self::parse_action(&raw)?;

            if let Some(final_obj) = action.get("final") {
                if let Some(outcome) = ctx.policy.handle_final(tools, ctx, &mut transcript, store, &tid, final_obj).await? {
                    return Ok(outcome);
                }
                // Policy rejected final; continue.
                continue;
            }

            let Some(action_name) = action.get("action").and_then(|x| x.as_str()) else {
                warn!("model output missing action/final");
                Self::transcript_add(&mut transcript, "Observation: {\"ok\":false,\"error\":\"missing action\"}".to_string(), &ctx.trace_tx);
                continue;
            };
            let args = action.get("args").cloned().unwrap_or_else(|| serde_json::json!({}));

            info!("agent action: {}", action_name);
            let timeout_secs = ctx
                .policy
                .timeout_for_tool(action_name)
                .unwrap_or(ctx.per_step_timeout_secs)
                .max(1);
            let obs = match tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs),
                tools.call(action_name, args.clone(), ctx),
            )
            .await
            {
                Ok(r) => r.unwrap_or_else(|e| serde_json::json!({"ok": false, "error": e})),
                Err(_) => serde_json::json!({"ok": false, "error": "tool timeout"}),
            };

            // Persist step if store exists.
            if let Some(store) = store {
                let _ = store
                    .append_step(
                        &tid,
                        ThreadStep {
                            action: action_name.to_string(),
                            args: args.clone(),
                            observation: obs.clone(),
                            ts: chrono::Utc::now().to_rfc3339(),
                            agent: ctx.agent_name.clone(),
                        },
                    )
                    .await;
            }

            // Policy may turn this tool into an interrupt.
            if let Some(int) = ctx.policy.interrupt_for_action(action_name, &args, &obs) {
                match int {
                    Interrupt::AwaitUser { prompt } => return Ok(RunOutcome::AwaitUser { thread_id: tid, prompt }),
                    Interrupt::AwaitApproval { prompt } => return Ok(RunOutcome::AwaitApproval { thread_id: tid, prompt }),
                }
            }

            Self::transcript_add(&mut transcript, format!("Assistant: {}", raw), &ctx.trace_tx);

            // Special case: if tool produced a *very large* error blob, do a chunked follow-up action.
            if let Some(err) = obs.get("error").and_then(|v| v.as_str()) {
                if err.len() >= 10_000 {
                    if let Ok(raw2) = Self::llm_action_via_chunked_errors(ctx, &transcript, step_idx, err).await {
                        Self::transcript_add(&mut transcript, format!("Assistant: {}", raw2), &ctx.trace_tx);
                    }
                }
            }

            Self::transcript_add(&mut transcript, format!("Observation: {}", obs), &ctx.trace_tx);
        }

        ctx.policy.fallback(tools, ctx, &mut transcript, store, &tid).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyspace::DefaultKeyspace;
    use crate::scope::RequestScope;
    use crate::storage::InMemoryStorageAdapter;
    use crate::tools::ToolRegistry;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    struct ScriptedModel {
        replies: Arc<Mutex<Vec<String>>>,
    }

    impl crate::llm::LargeLanguageModel for ScriptedModel {
        fn chat(&self, _messages: &[crate::llm::ChatMessage]) -> Result<String, String> {
            let mut g = self.replies.lock().map_err(|_| "mutex poisoned".to_string())?;
            if g.is_empty() {
                return Err("no more replies".to_string());
            }
            Ok(g.remove(0))
        }

        fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            Ok(vec![])
        }
    }

    struct TimeoutPolicy {
        inner: DefaultPolicy,
        secs: u64,
    }

    #[async_trait]
    impl AgentPolicy for TimeoutPolicy {
        fn timeout_for_tool(&self, _action_name: &str) -> Option<u64> {
            Some(self.secs)
        }

        async fn handle_final(
            &self,
            tools: &ToolRegistry,
            ctx: &AgentCtx,
            transcript: &mut Vec<String>,
            store: Option<&crate::session::ThreadStore>,
            thread_id: &str,
            final_obj: &serde_json::Value,
        ) -> Result<Option<RunOutcome>, String> {
            self.inner
                .handle_final(tools, ctx, transcript, store, thread_id, final_obj)
                .await
        }
    }

    struct SlowTool;

    #[async_trait]
    impl crate::tools::Tool for SlowTool {
        fn name(&self) -> &'static str {
            "slow_tool"
        }

        async fn call(&self, _args: Value, _ctx: &AgentCtx) -> Result<Value, String> {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            Ok(serde_json::json!({"ok": true}))
        }
    }

    #[tokio::test]
    async fn per_tool_timeout_override_is_used() {
        let llm = Arc::new(ScriptedModel {
            replies: Arc::new(Mutex::new(vec![
                "{\"action\":\"slow_tool\",\"args\":{}}".to_string(),
                "{\"final\":{\"answer\":\"ok\",\"sql\":\"SELECT 1 AS ok\"}}".to_string(),
            ])),
        });
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let keyspace = Arc::new(DefaultKeyspace::new("bucket".to_string()));
        let scope = RequestScope { tenant: "t".into(), workspace: "w".into(), project_id: "p".into() };

        let mut reg = ToolRegistry::new();
        reg.register(SlowTool);

        let ctx = AgentCtx {
            top_k: 1,
            per_step_timeout_secs: 1,
            max_steps: 4,
            thread_id: Some("tid".to_string()),
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: None,
            agent_name: Some("test".to_string()),
            policy: Arc::new(TimeoutPolicy { inner: DefaultPolicy, secs: 3 }),
            llm,
            storage,
            scope,
            keyspace,
            query: None,
            dbt: None,
            vector: None,
            thread_store: None,
            runtime: None,
        };

        let out = Agent::run_until_block(&reg, &ctx, "sys", "tools", "q")
            .await
            .expect("ok");
        match out {
            RunOutcome::Final { result, .. } => assert_eq!(result.answer, "ok"),
            _ => panic!("expected final outcome"),
        }
    }

    #[test]
    fn parse_action_repairs_raw_newlines_inside_json_strings() {
        // NOTE: This is intentionally invalid JSON: literal newline in the string value.
        let raw = "{\"final\":{\"answer\":\"line1\nline2\",\"sql\":\"SELECT 1 AS ok\"}}";
        let v = Agent::parse_action(raw).expect("should repair and parse");
        let final_obj = v.get("final").expect("final");
        assert_eq!(
            final_obj.get("answer").and_then(|x| x.as_str()).unwrap(),
            "line1\nline2"
        );
        assert_eq!(
            final_obj.get("sql").and_then(|x| x.as_str()).unwrap(),
            "SELECT 1 AS ok"
        );
    }
}

