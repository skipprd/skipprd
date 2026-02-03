use serde_json::Value;
use tracing::{info, warn};
use std::sync::Arc;
use std::any::Any;

use crate::tools::ToolRegistry;
use crate::llm::ChatMessage;
use crate::session::{Observation, ThreadStore, ThreadStep, ThreadResult, ToolObservation};
use crate::llm_observability::{PartInput};
use crate::keyspace::Keyspace;
use crate::providers::{QueryProvider, DbtProvider, VectorStore};
use crate::storage::StorageAdapter;
use crate::scope::RequestScope;
use uuid::Uuid;
use async_trait::async_trait;

#[derive(serde::Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct FinalEnvelope {
    pub kind: String,
    pub payload: Value,
    #[serde(default)]
    pub display: Option<String>,
}

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

fn title_case_words(s: &str) -> String {
    let mut out = String::new();
    for (i, w) in s.split_whitespace().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let mut chars = w.chars();
        match chars.next() {
            Some(c) => {
                out.extend(c.to_uppercase());
                out.push_str(chars.as_str());
            }
            None => {}
        }
    }
    out
}

fn clean_tool_name(name: &str, args: &Value) -> String {
    match name {
        "dbt_files" => {
            let op = args.get("op").and_then(|v| v.as_str()).unwrap_or("");
            match op {
                "get" => {
                    let p = args.get("path").and_then(|v| v.as_str()).unwrap_or("").trim();
                    if !p.is_empty() { return format!("Read {p}"); }
                    "Read file".to_string()
                }
                "list" => {
                    let p = args.get("prefix").and_then(|v| v.as_str()).unwrap_or("").trim();
                    if !p.is_empty() { return format!("List {p}"); }
                    "List files".to_string()
                }
                "get_json" => {
                    let p = args.get("path").and_then(|v| v.as_str()).unwrap_or("").trim();
                    if !p.is_empty() { return format!("Read JSON {p}"); }
                    "Read JSON".to_string()
                }
                "patch" => {
                    let p = args.get("path").and_then(|v| v.as_str()).unwrap_or("").trim();
                    if !p.is_empty() { return format!("Patch {p}"); }
                    "Patch file".to_string()
                }
                _ => {
                    if !op.is_empty() { return format!("dbt_files {op}"); }
                    "dbt_files".to_string()
                }
            }
        }
        "run_sql" => "Run SQL".to_string(),
        "sql_schema" => {
            let t = args.get("table").and_then(|v| v.as_str()).unwrap_or("").trim();
            if !t.is_empty() { format!("Describe {t}") } else { "List tables".to_string() }
        }
        "sql_stats" => {
            let t = args.get("table").and_then(|v| v.as_str()).unwrap_or("").trim();
            if !t.is_empty() { format!("Stats {t}") } else { "Stats".to_string() }
        }
        "sql_sample" => {
            let t = args.get("table").and_then(|v| v.as_str()).unwrap_or("").trim();
            if !t.is_empty() { format!("Sample {t}") } else { "Sample".to_string() }
        }
        "vect_query" => "Vector search".to_string(),
        other => title_case_words(&other.replace('_', " ")),
    }
}

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
        final_env: &FinalEnvelope,
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
        Ok(RunOutcome::AwaitUser {
            thread_id: thread_id.to_string(),
            prompt: "Agent reached step limit without producing a valid final. Please retry.".to_string(),
        })
    }
}

/// Default policy: accept any well-formed typed final envelope.
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
        final_env: &FinalEnvelope,
    ) -> Result<Option<RunOutcome>, String> {
        let result = ThreadResult {
            kind: final_env.kind.clone(),
            payload: final_env.payload.clone(),
            display: final_env.display.clone(),
        };
        if let Some(store) = store {
            let agent = ctx
                .agent_name
                .clone()
                .unwrap_or_else(|| "unknown".to_string());
            let ts = chrono::Utc::now().to_rfc3339();
            let _ = store
                .append_step(
                    thread_id,
                    ThreadStep::Final {
                        kind: result.kind.clone(),
                        payload: result.payload.clone(),
                        display: result.display.clone(),
                        observation: Observation::ok(),
                        ts,
                        agent,
                    }
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

    async fn llm_chat_once(ctx: &AgentCtx, prompt: String) -> Result<String, String> {
        let model = ctx.llm.clone();

        let thread_id_opt = ctx.thread_id.clone();
        let store_opt = ctx.thread_store.clone();
        let agent = ctx
            .agent_name
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        let ts = chrono::Utc::now().to_rfc3339();

        let messages = vec![ChatMessage { role: "user".into(), content: prompt.clone() }];

        // LLM call observability (stdout + persisted thread step).
        let obs_enabled = crate::llm_observability::llm_calls_enabled() && thread_id_opt.is_some();
        let (call_id_opt, phase, prompt_hash, parts_built) = if obs_enabled {
            let thread_id = thread_id_opt.as_ref().unwrap();
            let call_id = crate::llm_observability::next_call_id(thread_id);
            let prompt_hash = crate::llm_observability::prompt_hash_for_messages(&messages);

            // Best-effort: derive phase from persisted thread log.
            let phase = if let (Some(store), Some(tid)) = (store_opt.as_ref(), thread_id_opt.as_ref()) {
                match store.get(tid).await {
                    Ok(log) => {
                        let mut found = "react_loop".to_string();
                        for step in log.steps.iter().rev() {
                            if let ThreadStep::Phase { phase, .. } = step {
                                let t = phase.trim();
                                if !t.is_empty() {
                                    found = t.to_string();
                                    break;
                                }
                            }
                        }
                        found
                    }
                    Err(_) => "react_loop".to_string(),
                }
            } else {
                "react_loop".to_string()
            };

            let built = crate::llm_observability::build_parts_for_thread(
                thread_id,
                &[PartInput { name: "user".to_string(), text: prompt.clone() }],
            );

            // Stdout debug logs: print each part in full if changed, else "unchanged".
            tracing::debug!(
                "LLM_CALL thread_id={} call_id={} agent={} phase={} model={} prompt_hash={} response_pending=1",
                thread_id,
                call_id,
                agent,
                phase,
                "unknown",
                prompt_hash
            );
            for p in built.parts.iter() {
                let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("-");
                let hash = p.get("hash").and_then(|v| v.as_str()).unwrap_or("-");
                let text = p.get("text").and_then(|v| v.as_str()).unwrap_or("");
                tracing::debug!("LLM_PART thread_id={} call_id={} name={} hash={} text={}", thread_id, call_id, name, hash, text);
            }

            (Some(call_id), phase, prompt_hash, Some(built))
        } else {
            (None, "react_loop".to_string(), String::new(), None)
        };

        // Emit llm_start as soon as we have a call id.
        if let (Some(call_id), Some(thread_id), Some(store)) =
            (call_id_opt, thread_id_opt.as_ref().cloned(), store_opt.as_ref().cloned())
        {
            let _ = store
                .append_step(
                    &thread_id,
                    ThreadStep::LlmStart {
                        call_id,
                        model: Some("unknown".to_string()),
                        phase: phase.clone(),
                        ts: chrono::Utc::now().to_rfc3339(),
                        agent: agent.clone(),
                    },
                )
                .await;
        }

        let res = tokio::task::spawn_blocking(move || model.chat(&messages))
            .await
            .map_err(|e| format!("LLM execution failed: {}", e))?
            .map_err(|e| format!("LLM request failed: {}", e));

        // Persist `llm_call` step after the response (success or failure), if enabled.
        if let (Some(call_id), Some(thread_id), Some(store), Some(built)) =
            (call_id_opt, thread_id_opt.as_ref().cloned(), store_opt.as_ref().cloned(), parts_built)
        {
            let (ok, response_raw) = match res.as_ref() {
                Ok(txt) => (true, txt.as_str()),
                Err(e) => (false, e.as_str()),
            };

            // Emit llm_end before the full llm_call record.
            let _ = store
                .append_step(
                    &thread_id,
                    ThreadStep::LlmEnd {
                        call_id,
                        model: Some("unknown".to_string()),
                        phase: phase.clone(),
                        status: if ok { "ok".to_string() } else { "failed".to_string() },
                        error: if ok { None } else { Some(response_raw.to_string()) },
                        ts: chrono::Utc::now().to_rfc3339(),
                        agent: agent.clone(),
                    },
                )
                .await;

            let response_hash = crate::llm_observability::sha256_hex_str(response_raw);
            let response_text = if crate::llm_observability::llm_response_text_enabled() {
                Some(crate::llm_observability::redact_common_secrets(response_raw))
            } else {
                None
            };

            if crate::llm_observability::llm_response_text_enabled() {
                tracing::debug!(
                    "LLM_RESPONSE thread_id={} call_id={} response_hash={} response_text={}",
                    thread_id,
                    call_id,
                    response_hash,
                    response_text.as_deref().unwrap_or("")
                );
            } else {
                tracing::debug!(
                    "LLM_RESPONSE thread_id={} call_id={} response_hash={} response_text=disabled",
                    thread_id,
                    call_id,
                    response_hash
                );
            }

            let _ = store
                .append_step(
                    &thread_id,
                    ThreadStep::LlmCall {
                        call_id,
                        model: "unknown".to_string(),
                        phase,
                        prompt_hash,
                        parts: built.parts,
                        part_hashes: built.part_hashes,
                        response_hash,
                        response_text,
                        observation: if ok { Observation::ok() } else { Observation::fail(vec!["llm_call_failed".to_string()]) },
                        ts,
                        agent,
                    },
                )
                .await;
        }

        res
    }

    fn parse_action(raw: &str) -> Result<Value, String> {
        // Be forgiving: some model backends may emit multiple JSON objects in one response
        // (e.g. a tool action followed by a final). Prefer an object that contains "action"
        // (tool call) over "final" and over args-only objects.
        match serde_json::from_str::<Value>(raw) {
            Ok(v) => Ok(v),
            Err(e) => {
                let trimmed = raw.trim();
                let extracted = Self::extract_all_json_values(trimmed, 8);
                if extracted.is_empty() {
                    return Err(format!("invalid JSON from model: {}", e));
                }

                let mut parsed: Vec<Value> = Vec::new();
                for txt in extracted.iter() {
                    if let Ok(v) = serde_json::from_str::<Value>(txt) {
                        parsed.push(v);
                        continue;
                    }
                    // Conservative repair: escape control chars inside strings (raw newlines, etc).
                    let repaired = Self::escape_control_chars_in_json_strings(txt);
                    if let Ok(v) = serde_json::from_str::<Value>(&repaired) {
                        parsed.push(v);
                    }
                }

                if parsed.is_empty() {
                    return Err(format!("invalid JSON from model: {}", e));
                }

                // Prefer the *last* action (tool call); otherwise the *last* final; otherwise the last JSON value.
                let mut best_action: Option<Value> = None;
                let mut best_final: Option<Value> = None;
                for v in parsed.iter() {
                    if let Some(obj) = v.as_object() {
                        if obj.contains_key("action") {
                            best_action = Some(v.clone());
                        } else if obj.contains_key("final") {
                            best_final = Some(v.clone());
                        }
                    }
                }
                Ok(best_action.or(best_final).unwrap_or_else(|| parsed.last().cloned().unwrap()))
            }
        }
    }

    fn coerce_args_only_action(v: Value) -> Value {
        // Deterministic coercion for common failure mode: model emits tool args without
        // the required {"action": "...", "args": {...}} envelope.
        if let Some(obj) = v.as_object() {
            if obj.contains_key("action") || obj.contains_key("final") {
                return v;
            }
            // dbt_files tool: always has an "op" discriminator.
            if obj.get("op").and_then(|x| x.as_str()).is_some() {
                return serde_json::json!({ "action": "dbt_files", "args": v });
            }
            // run_sql tool: args are commonly just {"sql": "..."}.
            if obj.get("sql").and_then(|x| x.as_str()).is_some() {
                return serde_json::json!({ "action": "run_sql", "args": v });
            }
            // vect_query tool: args include scope + query_text.
            if obj.get("scope").and_then(|x| x.as_str()).is_some() && obj.get("query_text").and_then(|x| x.as_str()).is_some() {
                return serde_json::json!({ "action": "vect_query", "args": v });
            }
        }
        v
    }

    fn extract_all_json_values(s: &str, max: usize) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if max == 0 {
            return out;
        }

        let mut i = 0usize;
        while i < s.len() && out.len() < max {
            // Find the next start.
            let mut start: Option<usize> = None;
            for (off, ch) in s[i..].char_indices() {
                if ch == '{' || ch == '[' {
                    start = Some(i + off);
                    break;
                }
            }
            let Some(st) = start else { break };

            // Walk forward from start until we close the top-level value.
            let mut stack: Vec<char> = Vec::new();
            let mut in_str = false;
            let mut esc = false;
            let mut end: Option<usize> = None;
            for (pos, ch) in s[st..].char_indices() {
                let abs = st + pos;

                if stack.is_empty() {
                    stack.push(ch);
                } else if in_str {
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
                } else {
                    match ch {
                        '"' => in_str = true,
                        '{' | '[' => stack.push(ch),
                        '}' => {
                            if matches!(stack.pop(), Some('{')) && stack.is_empty() {
                                end = Some(abs);
                                break;
                            }
                        }
                        ']' => {
                            if matches!(stack.pop(), Some('[')) && stack.is_empty() {
                                end = Some(abs);
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }

            let Some(en) = end else { break };
            out.push(s[st..=en].to_string());
            i = en + 1;
        }

        out
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
            let action = Self::coerce_args_only_action(Self::parse_action(&raw)?);

            if let Some(final_obj) = action.get("final") {
                let env = match serde_json::from_value::<FinalEnvelope>(final_obj.clone()) {
                    Ok(env) => env,
                    Err(e) => {
                        warn!("model emitted invalid final envelope: {}", e);
                        Self::transcript_add(
                            &mut transcript,
                            format!(
                                "Observation: {}",
                                serde_json::json!({
                                    "ok": false,
                                    "errors": [format!("invalid final envelope: {e}")]
                                })
                            ),
                            &ctx.trace_tx,
                        );
                        continue;
                    }
                };

                if let Some(outcome) = ctx
                    .policy
                    .handle_final(tools, ctx, &mut transcript, store, &tid, &env)
                    .await?
                {
                    return Ok(outcome);
                }
                // Policy rejected final; continue.
                continue;
            }

            let Some(action_name) = action.get("action").and_then(|x| x.as_str()) else {
                warn!("model output missing action/final");
                Self::transcript_add(&mut transcript, "Observation: {\"ok\":false,\"errors\":[\"missing action\"]}".to_string(), &ctx.trace_tx);
                continue;
            };
            let args = action.get("args").cloned().unwrap_or_else(|| serde_json::json!({}));

            info!("agent action: {}", action_name);
            let timeout_secs = ctx
                .policy
                .timeout_for_tool(action_name)
                .unwrap_or(ctx.per_step_timeout_secs)
                .max(1);

            // Persist tool_start immediately so UIs can show in-flight tool runtime.
            let tool_id = uuid::Uuid::new_v4().to_string();
            let agent = ctx
                .agent_name
                .clone()
                .unwrap_or_else(|| "unknown".to_string());
            let clean_name = clean_tool_name(action_name, &args);
            if let Some(store) = store {
                let _ = store
                    .append_step(
                        &tid,
                        ThreadStep::ToolStart {
                            tool_id: tool_id.clone(),
                            name: action_name.to_string(),
                            clean_name: clean_name.clone(),
                            args: args.clone(),
                            status: "running".to_string(),
                            payload: None,
                            ts: chrono::Utc::now().to_rfc3339(),
                            agent: agent.clone(),
                        },
                    )
                    .await;
            }

            let raw_obs = match tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs),
                tools.call(action_name, args.clone(), ctx),
            )
            .await
            {
                Ok(r) => r.unwrap_or_else(|e| serde_json::json!({"ok": false, "errors": [e]})),
                Err(_) => serde_json::json!({"ok": false, "errors": ["tool timeout"]}),
            };
            let obs_env = ToolObservation::normalize(raw_obs.clone());
            let obs_env_for_transcript = obs_env.clone();

            // Persist tool_end if store exists.
            if let Some(store) = store {
                let status = if obs_env.ok { "ok".to_string() } else { "failed".to_string() };
                let payload = obs_env
                    .extra
                    .get("payload")
                    .cloned()
                    .or_else(|| obs_env.extra.get("ui_payload").cloned());
                let _ = store
                    .append_step(
                        &tid,
                        ThreadStep::ToolEnd {
                            tool_id: tool_id.clone(),
                            name: action_name.to_string(),
                            clean_name: clean_name.clone(),
                            args: args.clone(),
                            status,
                            payload,
                            observation: obs_env,
                            ts: chrono::Utc::now().to_rfc3339(),
                            agent: agent.clone(),
                        }
                    )
                    .await;
            }

            // Policy may turn this tool into an interrupt.
            if let Some(int) = ctx.policy.interrupt_for_action(action_name, &args, &raw_obs) {
                match int {
                    Interrupt::AwaitUser { prompt } => return Ok(RunOutcome::AwaitUser { thread_id: tid, prompt }),
                    Interrupt::AwaitApproval { prompt } => return Ok(RunOutcome::AwaitApproval { thread_id: tid, prompt }),
                }
            }

            Self::transcript_add(&mut transcript, format!("Assistant: {}", raw), &ctx.trace_tx);

            // Always preserve full tool output in the persisted thread log (ToolObservation).
            // For the model-facing transcript, include the full error output when it fits the prompt budget;
            // otherwise include a deterministic excerpt so we don't miss the critical lines while staying in-bounds.
            if obs_env_for_transcript.ok {
                Self::transcript_add(&mut transcript, format!("Observation: {}", raw_obs), &ctx.trace_tx);
            } else {
                let max_prompt_chars = crate::error_context::estimate_max_prompt_chars(ctx);
                // Best-effort remaining budget: current transcript size + the new line overhead.
                let used_chars: usize = transcript.iter().map(|l| l.chars().count() + 1).sum();
                let remaining = max_prompt_chars.saturating_sub(used_chars).max(256);
                let rendered = crate::error_context::render_failure_context(&obs_env_for_transcript, remaining);
                Self::transcript_add(
                    &mut transcript,
                    format!("Observation: {}", serde_json::json!({ "ok": false, "error_context": rendered })),
                    &ctx.trace_tx,
                );
            }
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
            final_env: &FinalEnvelope,
        ) -> Result<Option<RunOutcome>, String> {
            self.inner
                .handle_final(tools, ctx, transcript, store, thread_id, final_env)
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
                "{\"final\":{\"kind\":\"generic\",\"payload\":{\"text\":\"ok\"}}}".to_string(),
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
            RunOutcome::Final { result, .. } => {
                assert_eq!(result.kind, "generic");
                assert_eq!(result.payload.get("text").and_then(|x| x.as_str()), Some("ok"));
            }
            _ => panic!("expected final outcome"),
        }
    }

    #[tokio::test]
    async fn final_payload_round_trips_as_json_value() {
        let llm = Arc::new(ScriptedModel {
            replies: Arc::new(Mutex::new(vec![
                "{\"final\":{\"kind\":\"generic\",\"payload\":{\"obj\":{\"hello\":\"world\",\"n\":1}}}}".to_string(),
            ])),
        });
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let keyspace = Arc::new(DefaultKeyspace::new("bucket".to_string()));
        let scope = RequestScope { tenant: "t".into(), workspace: "w".into(), project_id: "p".into() };

        let reg = ToolRegistry::new();
        let ctx = AgentCtx {
            top_k: 1,
            per_step_timeout_secs: 1,
            max_steps: 2,
            thread_id: Some("tid".to_string()),
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: None,
            agent_name: Some("test".to_string()),
            policy: Arc::new(DefaultPolicy),
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
            RunOutcome::Final { result, .. } => {
                assert_eq!(result.kind, "generic");
                let obj = result.payload.get("obj").expect("obj");
                assert_eq!(obj.get("hello").and_then(|x| x.as_str()), Some("world"));
                assert_eq!(obj.get("n").and_then(|x| x.as_i64()), Some(1));
            }
            _ => panic!("expected final outcome"),
        }
    }

    #[test]
    fn parse_action_repairs_raw_newlines_inside_json_strings() {
        // NOTE: This is intentionally invalid JSON: literal newline in the string value.
        let raw = "{\"final\":{\"kind\":\"generic\",\"payload\":{\"text\":\"line1\nline2\"}}}";
        let v = Agent::parse_action(raw).expect("should repair and parse");
        let final_obj = v.get("final").expect("final");
        let env: FinalEnvelope = serde_json::from_value(final_obj.clone()).expect("env");
        assert_eq!(env.kind, "generic");
        assert_eq!(
            env.payload.get("text").and_then(|x| x.as_str()).unwrap(),
            "line1\nline2"
        );
    }
}

