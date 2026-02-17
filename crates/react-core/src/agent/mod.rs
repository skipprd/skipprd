use serde_json::Value;
use std::any::Any;
use std::sync::Arc;
use tracing::info;

use crate::keyspace::Keyspace;
use crate::llm::{ChatMessage, LlmCallOptions, LlmExpectedFormat};
use crate::llm_observability::PartInput;
use crate::providers::{
    DbtProvider, QueryProvider, VectorStore, WarehouseProvider,
};
use crate::schema_registry::{AgentStepTypeV1, AgentStepV1, SchemaId};
use crate::scope::RequestScope;
use crate::session::{ExecutionContext, Observation, ThreadResult, ThreadStep, ThreadStore, ToolObservation};
use crate::storage::StorageAdapter;
use crate::tools::ToolRegistry;
use async_trait::async_trait;
use uuid::Uuid;

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
    /// Warehouse provider (dbt target). Suites should use this as the single source of truth.
    pub warehouse: Arc<dyn WarehouseProvider>,
    /// Optional DBT provider (suite-provided).
    pub dbt: Option<Arc<dyn DbtProvider>>,
    /// Optional vector store provider (suite-provided).
    pub vector: Option<Arc<dyn VectorStore>>,
    /// Optional thread store (for transcript persistence and artifact context).
    pub thread_store: Option<ThreadStore>,

    /// Optional explicit execution context for hierarchical UI rendering.
    pub exec_ctx: Option<ExecutionContext>,

    /// Optional runtime-specific context/configuration blob (type-erased).
    ///
    /// Suites/tools may downcast this to access runtime wiring/config without
    /// coupling the core runner to any particular config type.
    pub runtime: Option<Arc<dyn Any + Send + Sync>>,
}

pub struct Agent;

#[derive(Clone, Debug)]
enum ParsedStep {
    Tool { name: String, args: Value },
    Final { final_env: FinalEnvelope },
}

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
                    let p = args
                        .get("path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim();
                    if !p.is_empty() {
                        return format!("Read {p}");
                    }
                    "Read file".to_string()
                }
                "list" => {
                    let p = args
                        .get("prefix")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim();
                    if !p.is_empty() {
                        return format!("List {p}");
                    }
                    "List files".to_string()
                }
                "get_json" => {
                    let p = args
                        .get("path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim();
                    if !p.is_empty() {
                        return format!("Read JSON {p}");
                    }
                    "Read JSON".to_string()
                }
                "patch" => {
                    let p = args
                        .get("path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim();
                    if !p.is_empty() {
                        return format!("Patch {p}");
                    }
                    "Patch file".to_string()
                }
                _ => {
                    if !op.is_empty() {
                        return format!("dbt_files {op}");
                    }
                    "dbt_files".to_string()
                }
            }
        }
        "run_sql" => "Run SQL".to_string(),
        "sql_schema" => {
            let t = args
                .get("table")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if !t.is_empty() {
                format!("Describe {t}")
            } else {
                "List tables".to_string()
            }
        }
        "sql_stats" => {
            let t = args
                .get("table")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if !t.is_empty() {
                format!("Stats {t}")
            } else {
                "Stats".to_string()
            }
        }
        "sql_sample" => {
            let t = args
                .get("table")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if !t.is_empty() {
                format!("Sample {t}")
            } else {
                "Sample".to_string()
            }
        }
        "vect_query" => "Vector search".to_string(),
        other => title_case_words(&other.replace('_', " ")),
    }
}

pub enum RunOutcome {
    Final {
        thread_id: String,
        result: ThreadResult,
    },
    AwaitUser {
        thread_id: String,
        prompt: String,
    },
    AwaitApproval {
        thread_id: String,
        prompt: String,
    },
}

pub enum Interrupt {
    AwaitUser { prompt: String },
    AwaitApproval { prompt: String },
}

#[async_trait]
pub trait AgentPolicy: Send + Sync {
    /// Extra transcript lines to inject after system/tool-card and before the user question.
    fn prelude_lines(
        &self,
        _ctx: &AgentCtx,
        _store: Option<&ThreadStore>,
        _thread_id: &str,
    ) -> Vec<String> {
        Vec::new()
    }

    /// Optional interrupt hook: after a tool action executes, policy may convert it into a control
    /// flow interrupt (await user / await approval). This keeps the core loop tool-name agnostic.
    fn interrupt_for_action(
        &self,
        _action_name: &str,
        _args: &Value,
        _obs: &Value,
    ) -> Option<Interrupt> {
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
            prompt: "Agent reached step limit without producing a valid final. Please retry."
                .to_string(),
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
                    },
                )
                .await;
        }
        Ok(Some(RunOutcome::Final {
            thread_id: thread_id.to_string(),
            result,
        }))
    }
}

impl Agent {
    fn gen_uuid() -> String {
        Uuid::new_v4().to_string()
    }

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

    async fn llm_chat_once(
        ctx: &AgentCtx,
        prompt: String,
        llm_options: LlmCallOptions,
    ) -> Result<String, String> {
        let model = ctx.llm.clone();

        let thread_id_opt = ctx.thread_id.clone();
        let store_opt = ctx.thread_store.clone();
        let agent = ctx
            .agent_name
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        let ts = chrono::Utc::now().to_rfc3339();

        let messages = vec![ChatMessage {
            role: "user".into(),
            content: prompt.clone(),
        }];

        // LLM call observability (stdout + persisted thread step).
        let obs_enabled = crate::llm_observability::llm_calls_enabled() && thread_id_opt.is_some();
        let (call_id_opt, phase, prompt_hash, parts_built) = if obs_enabled {
            let thread_id = thread_id_opt.as_ref().unwrap();
            let call_id = crate::llm_observability::next_call_id(thread_id);
            let prompt_hash = crate::llm_observability::prompt_hash_for_messages(&messages);

            // Best-effort: derive phase from persisted thread log.
            let phase =
                if let (Some(store), Some(tid)) = (store_opt.as_ref(), thread_id_opt.as_ref()) {
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
                &[PartInput {
                    name: "user".to_string(),
                    text: prompt.clone(),
                }],
            );

            // Stdout debug logs: print each part in full if changed, else "unchanged".
            tracing::debug!(
                "LLM_CALL thread_id={} call_id={} agent={} phase={} model={} prompt_id={} response_pending=1",
                thread_id,
                call_id,
                agent,
                phase,
                "unknown",
                llm_options.prompt_id
            );
            for p in built.parts.iter() {
                let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("-");
                let hash = p.get("hash").and_then(|v| v.as_str()).unwrap_or("-");
                let text = p.get("text").and_then(|v| v.as_str()).unwrap_or("");
                tracing::debug!(
                    "LLM_PART thread_id={} call_id={} name={} hash={} text={}",
                    thread_id,
                    call_id,
                    name,
                    hash,
                    text
                );
            }

            (Some(call_id), phase, prompt_hash, Some(built))
        } else {
            (None, "react_loop".to_string(), String::new(), None)
        };

        // Emit llm_start as soon as we have a call id.
        if let (Some(call_id), Some(thread_id), Some(store)) = (
            call_id_opt,
            thread_id_opt.as_ref().cloned(),
            store_opt.as_ref().cloned(),
        ) {
            let _ = store
                .append_step(
                    &thread_id,
                    ThreadStep::LlmStart {
                        call_id,
                        model: Some("unknown".to_string()),
                        phase: phase.clone(),
                        ctx: ctx.exec_ctx.clone(),
                        ts: chrono::Utc::now().to_rfc3339(),
                        agent: agent.clone(),
                    },
                )
                .await;
        }

        // Ensure the provider/router receives the thread_id even inside spawn_blocking.
        let mut llm_options = llm_options;
        if llm_options.thread_id.is_none() {
            llm_options.thread_id = thread_id_opt.clone();
        }
        let res = tokio::task::spawn_blocking(move || model.chat(&messages, &llm_options))
            .await
            .map_err(|e| format!("LLM execution failed: {}", e))?
            .map_err(|e| format!("LLM request failed: {}", e));

        // Persist `llm_call` step after the response (success or failure), if enabled.
        if let (Some(call_id), Some(thread_id), Some(store), Some(built)) = (
            call_id_opt,
            thread_id_opt.as_ref().cloned(),
            store_opt.as_ref().cloned(),
            parts_built,
        ) {
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
                        status: if ok {
                            "ok".to_string()
                        } else {
                            "failed".to_string()
                        },
                        error: if ok {
                            None
                        } else {
                            Some(response_raw.to_string())
                        },
                        ctx: ctx.exec_ctx.clone(),
                        ts: chrono::Utc::now().to_rfc3339(),
                        agent: agent.clone(),
                    },
                )
                .await;

            let response_hash = crate::llm_observability::sha256_hex_str(response_raw);
            let response_text = if crate::llm_observability::llm_response_text_enabled() {
                Some(crate::llm_observability::redact_common_secrets(
                    response_raw,
                ))
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
                        observation: if ok {
                            Observation::ok()
                        } else {
                            Observation::fail(vec!["llm_call_failed".to_string()])
                        },
                        ts,
                        agent,
                    },
                )
                .await;
        }

        res
    }

    fn strip_markdown_code_fences(raw: &str) -> String {
        let t = raw.trim();
        if !t.starts_with("```") {
            return t.to_string();
        }
        // Handle ```json ... ``` and ``` ... ```
        let t = t
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim();
        if let Some(end) = t.rfind("```") {
            return t[..end].trim().to_string();
        }
        t.to_string()
    }

    fn parse_agent_step(raw: &str) -> Result<ParsedStep, String> {
        let cleaned = Self::strip_markdown_code_fences(raw);
        let trimmed = cleaned.trim();

        fn parse_json_from_model_string_field(
            raw_json: &str,
            what: &str,
        ) -> Result<Value, String> {
            match serde_json::from_str::<Value>(raw_json) {
                Ok(v) => Ok(v),
                Err(e) => {
                    // Repair raw control chars inside JSON strings (literal newlines, etc).
                    let repaired = Agent::escape_control_chars_in_json_strings(raw_json);
                    serde_json::from_str::<Value>(&repaired).map_err(|_| {
                        format!("{what} is not valid JSON string: {e}")
                    })
                }
            }
        }

        let v = match serde_json::from_str::<Value>(trimmed) {
            Ok(v) => v,
            Err(e) => {
                // Conservative repair: escape control chars inside strings (raw newlines, etc).
                let repaired = Self::escape_control_chars_in_json_strings(trimmed);
                serde_json::from_str::<Value>(&repaired)
                    .map_err(|_| format!("invalid JSON from model: {}", e))?
            }
        };

        crate::schema_registry::validate(SchemaId::AgentStepV1, &v)?;
        let step: AgentStepV1 = serde_json::from_value::<AgentStepV1>(v)
            .map_err(|e| format!("failed to deserialize {}: {}", SchemaId::AgentStepV1.name(), e))?;

        match step.type_ {
            AgentStepTypeV1::Tool => {
                let Some(name) = step.name else {
                    return Err("agent.step.v1 validation error: missing tool name".to_string());
                };
                let Some(args_json) = step.args else {
                    return Err("agent.step.v1 validation error: missing tool args".to_string());
                };
                if step.final_.is_some() {
                    return Err(
                        "agent.step.v1 validation error: tool step must not include final".to_string(),
                    );
                }
                let args: Value = parse_json_from_model_string_field(
                    &args_json,
                    "agent.step.v1 validation error: args",
                )?;
                Ok(ParsedStep::Tool { name, args })
            }
            AgentStepTypeV1::Final => {
                if step.name.is_some() || step.args.is_some() {
                    return Err(
                        "agent.step.v1 validation error: final step must not include name/args"
                            .to_string(),
                    );
                }
                let Some(fin) = step.final_ else {
                    return Err("agent.step.v1 validation error: missing final".to_string());
                };
                let payload: Value = parse_json_from_model_string_field(
                    &fin.payload,
                    "agent.step.v1 validation error: final.payload",
                )?;
                Ok(ParsedStep::Final {
                    final_env: FinalEnvelope {
                        kind: fin.kind,
                        payload,
                        display: fin.display,
                    },
                })
            }
        }
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

    fn transcript_add(
        transcript: &mut Vec<String>,
        line: String,
        tx: &Option<tokio::sync::mpsc::UnboundedSender<String>>,
    ) {
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
        llm_options: LlmCallOptions,
    ) -> Result<RunOutcome, String> {
        // Agent steps are a strict JSON Schema contract: enforce provider JSON mode and validate.
        let mut llm_options = llm_options;
        llm_options.expected_format = LlmExpectedFormat::JsonSchema(SchemaId::AgentStepV1);

        let tid = ctx.thread_id.clone().unwrap_or_else(Self::gen_uuid);
        let store = ctx.thread_store.as_ref();

        let step_schema = crate::schema_registry::json_schema(SchemaId::AgentStepV1);
        let step_schema_txt =
            serde_json::to_string(&step_schema).unwrap_or_else(|_| "{\"error\":\"schema\"}".into());
        let output_contract_line = format!(
            "System: OUTPUT_CONTRACT schema_id={} schema={}. Return ONLY one JSON object matching this schema.",
            SchemaId::AgentStepV1.name(),
            step_schema_txt
        );

        // Transcript is plain-text lines the model sees.
        let mut transcript: Vec<String> = Vec::new();
        Self::transcript_add(
            &mut transcript,
            format!("System: {}", system_prompt),
            &ctx.trace_tx,
        );
        Self::transcript_add(
            &mut transcript,
            format!("Tools: {}", tools_card),
            &ctx.trace_tx,
        );

        // Suite/policy may inject extra context.
        for l in ctx.policy.prelude_lines(ctx, store, &tid) {
            Self::transcript_add(&mut transcript, l, &ctx.trace_tx);
        }

        Self::transcript_add(
            &mut transcript,
            format!("User: {}", question),
            &ctx.trace_tx,
        );

        for step_idx in 0..ctx.max_steps {
            if let Some(tx) = ctx.progress_tx.as_ref() {
                let _ = tx.send(step_idx);
            }
            if let Some(tx) = ctx.pre_step_tx.as_ref() {
                let _ = tx.send(format!("step {}", step_idx + 1));
            }

            // Ask model for next action.
            let prompt = format!("{}\n{}", transcript.join("\n"), output_contract_line);
            let mut raw = Self::llm_chat_once(ctx, prompt, llm_options.clone()).await?;
            let step = match Self::parse_agent_step(&raw) {
                Ok(v) => v,
                Err(e) => {
                    // Defense in depth: if the model output is invalid JSON or fails schema validation,
                    // retry with a minimal prompt so we don't amplify prompt bloat.
                    let is_json_err = e.starts_with("invalid JSON from model:");
                    let is_schema_err = e.contains("validation error:");
                    if !is_json_err && !is_schema_err {
                        return Err(e);
                    }

                    let resp_hash = crate::llm_observability::sha256_hex_str(&raw);
                    Self::transcript_add(
                        &mut transcript,
                        format!(
                            "Observation: {}",
                            serde_json::json!({
                                "ok": false,
                                "error": if is_schema_err { "schema_validation_failed" } else { "invalid_json_from_model" },
                                "detail": e,
                                "response_hash": resp_hash,
                                "bytes": raw.as_bytes().len(),
                            })
                        ),
                        &ctx.trace_tx,
                    );

                    // Retry 1: keep only System/Tools/initial User + last few observations, plus a strict instruction.
                    let mut keep: Vec<String> = Vec::new();
                    if let Some(l) = transcript.iter().find(|l| l.starts_with("System:")) {
                        keep.push(l.clone());
                    }
                    if let Some(l) = transcript.iter().find(|l| l.starts_with("Tools:")) {
                        keep.push(l.clone());
                    }
                    if let Some(l) = transcript.iter().find(|l| l.starts_with("User:")) {
                        keep.push(l.clone());
                    }
                    // Keep a small tail of the transcript for local context.
                    let tail_n = 12usize.min(transcript.len());
                    keep.extend(transcript.iter().skip(transcript.len().saturating_sub(tail_n)).cloned());
                    keep.push(format!(
                        "User: IMPORTANT: Your previous response did not match schema {}. Error: {}. Return ONLY one JSON object that matches the schema.",
                        SchemaId::AgentStepV1.name(),
                        e
                    ));
                    let retry_prompt = format!("{}\n{}", keep.join("\n"), output_contract_line);
                    raw = Self::llm_chat_once(ctx, retry_prompt, llm_options.clone()).await?;
                    match Self::parse_agent_step(&raw) {
                        Ok(v) => v,
                        Err(e2) => {
                            let is_json_err2 = e2.starts_with("invalid JSON from model:");
                            let is_schema_err2 = e2.contains("validation error:");
                            if !is_json_err2 && !is_schema_err2 {
                                return Err(e2);
                            }
                            // Retry 2: ultra-minimal.
                            let mut keep2: Vec<String> = Vec::new();
                            if let Some(l) = transcript.iter().find(|l| l.starts_with("System:")) {
                                keep2.push(l.clone());
                            }
                            if let Some(l) = transcript.iter().find(|l| l.starts_with("Tools:")) {
                                keep2.push(l.clone());
                            }
                            keep2.push(format!(
                                "User: Return ONLY one JSON object matching schema {}. Error: {}.",
                                SchemaId::AgentStepV1.name(),
                                e2
                            ));
                            let retry_prompt2 = format!("{}\n{}", keep2.join("\n"), output_contract_line);
                            raw = Self::llm_chat_once(ctx, retry_prompt2, llm_options.clone()).await?;
                            Self::parse_agent_step(&raw)?
                        }
                    }
                }
            };
            let (action_name, args) = match step {
                ParsedStep::Final { final_env: env } => {
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
                ParsedStep::Tool { name, args } => (name, args),
            };
            let action_name = action_name;
            let action_name_str = action_name.as_str();

            info!("agent action: {}", action_name_str);
            let timeout_secs = ctx
                .policy
                .timeout_for_tool(action_name_str)
                .unwrap_or(ctx.per_step_timeout_secs)
                .max(1);

            // Persist tool_start immediately so UIs can show in-flight tool runtime.
            let tool_id = uuid::Uuid::new_v4().to_string();
            let agent = ctx
                .agent_name
                .clone()
                .unwrap_or_else(|| "unknown".to_string());
            let clean_name = clean_tool_name(action_name_str, &args);
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
                            ctx: ctx.exec_ctx.clone(),
                            ts: chrono::Utc::now().to_rfc3339(),
                            agent: agent.clone(),
                        },
                    )
                    .await;
            }

            let raw_obs = match tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs),
                tools.call(action_name_str, args.clone(), ctx),
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
                let status = if obs_env.ok {
                    "ok".to_string()
                } else {
                    "failed".to_string()
                };
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
                            ctx: ctx.exec_ctx.clone(),
                            observation: obs_env,
                            ts: chrono::Utc::now().to_rfc3339(),
                            agent: agent.clone(),
                        },
                    )
                    .await;
            }

            // Policy may turn this tool into an interrupt.
            if let Some(int) = ctx
                .policy
                .interrupt_for_action(action_name_str, &args, &raw_obs)
            {
                match int {
                    Interrupt::AwaitUser { prompt } => {
                        return Ok(RunOutcome::AwaitUser {
                            thread_id: tid,
                            prompt,
                        })
                    }
                    Interrupt::AwaitApproval { prompt } => {
                        return Ok(RunOutcome::AwaitApproval {
                            thread_id: tid,
                            prompt,
                        })
                    }
                }
            }

            Self::transcript_add(
                &mut transcript,
                format!("Assistant: {}", raw),
                &ctx.trace_tx,
            );

            // Always preserve full tool output in the persisted thread log (ToolObservation).
            // For the model-facing transcript, include the full error output when it fits the prompt budget;
            // otherwise include a deterministic excerpt so we don't miss the critical lines while staying in-bounds.
            if obs_env_for_transcript.ok {
                Self::transcript_add(
                    &mut transcript,
                    format!("Observation: {}", raw_obs),
                    &ctx.trace_tx,
                );
            } else {
                let max_prompt_chars = crate::error_context::estimate_max_prompt_chars(ctx);
                // Best-effort remaining budget: current transcript size + the new line overhead.
                let used_chars: usize = transcript.iter().map(|l| l.chars().count() + 1).sum();
                let remaining = max_prompt_chars.saturating_sub(used_chars).max(256);
                let rendered = crate::error_context::render_failure_context(
                    &obs_env_for_transcript,
                    remaining,
                );
                Self::transcript_add(
                    &mut transcript,
                    format!(
                        "Observation: {}",
                        serde_json::json!({ "ok": false, "error_context": rendered })
                    ),
                    &ctx.trace_tx,
                );
            }
        }

        ctx.policy
            .fallback(tools, ctx, &mut transcript, store, &tid)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyspace::DefaultKeyspace;
    use crate::providers::NullWarehouseProvider;
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
        fn chat(
            &self,
            _messages: &[crate::llm::ChatMessage],
            _options: &crate::llm::LlmCallOptions,
        ) -> Result<String, String> {
            let mut g = self
                .replies
                .lock()
                .map_err(|_| "mutex poisoned".to_string())?;
            if g.is_empty() {
                return Err("no more replies".to_string());
            }
            Ok(g.remove(0))
        }

        fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            Ok(vec![])
        }
    }

    struct CapturingOptionsModel {
        replies: Arc<Mutex<Vec<String>>>,
        last_opts: Arc<Mutex<Option<crate::llm::LlmCallOptions>>>,
    }

    impl crate::llm::LargeLanguageModel for CapturingOptionsModel {
        fn chat(
            &self,
            _messages: &[crate::llm::ChatMessage],
            options: &crate::llm::LlmCallOptions,
        ) -> Result<String, String> {
            if let Ok(mut g) = self.last_opts.lock() {
                *g = Some(options.clone());
            }
            let mut g = self
                .replies
                .lock()
                .map_err(|_| "mutex poisoned".to_string())?;
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
                "{\"type\":\"tool\",\"name\":\"slow_tool\",\"args\":\"{}\",\"final\":null}".to_string(),
                "{\"type\":\"final\",\"name\":null,\"args\":null,\"final\":{\"kind\":\"generic\",\"payload\":\"{\\\"text\\\":\\\"ok\\\"}\",\"display\":null}}".to_string(),
            ])),
        });
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let keyspace = Arc::new(DefaultKeyspace::new("bucket".to_string()));
        let scope = RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        };

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
            policy: Arc::new(TimeoutPolicy {
                inner: DefaultPolicy,
                secs: 3,
            }),
            llm,
            storage,
            scope,
            keyspace,
            query: None,
            warehouse: Arc::new(NullWarehouseProvider::default()),
            dbt: None,
            vector: None,
            thread_store: None,
            exec_ctx: None,
            runtime: None,
        };

        let out = Agent::run_until_block(
            &reg,
            &ctx,
            "sys",
            "tools",
            "q",
            crate::llm::LlmCallOptions {
                prompt_id: "react_core.agent.tests.run_until_block_basic",
                thread_id: None,
                expected_format: crate::llm::LlmExpectedFormat::JsonObject,
                max_output_tokens: None,
                temperature: None,
                top_p: None,
                reasoning_effort: None,
            },
        )
            .await
            .expect("ok");
        match out {
            RunOutcome::Final { result, .. } => {
                assert_eq!(result.kind, "generic");
                assert_eq!(
                    result.payload.get("text").and_then(|x| x.as_str()),
                    Some("ok")
                );
            }
            _ => panic!("expected final outcome"),
        }
    }

    #[tokio::test]
    async fn run_until_block_passes_llm_call_options_through() {
        let last_opts: Arc<Mutex<Option<crate::llm::LlmCallOptions>>> =
            Arc::new(Mutex::new(None));
        let llm = Arc::new(CapturingOptionsModel {
            replies: Arc::new(Mutex::new(vec![
                "{\"type\":\"final\",\"name\":null,\"args\":null,\"final\":{\"kind\":\"generic\",\"payload\":\"{\\\"text\\\":\\\"ok\\\"}\",\"display\":null}}".to_string(),
            ])),
            last_opts: last_opts.clone(),
        });
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let keyspace = Arc::new(DefaultKeyspace::new("bucket".to_string()));
        let scope = RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        };
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
            warehouse: Arc::new(NullWarehouseProvider::default()),
            dbt: None,
            vector: None,
            thread_store: None,
            exec_ctx: None,
            runtime: None,
        };
        let opts = crate::llm::LlmCallOptions {
            prompt_id: "react_core.agent.tests.capture_options",
            thread_id: None,
            expected_format: crate::llm::LlmExpectedFormat::JsonObject,
            temperature: Some(0.9),
            top_p: Some(0.8),
            max_output_tokens: Some(1234),
            reasoning_effort: None,
        };
        let _out = Agent::run_until_block(&reg, &ctx, "sys", "tools", "q", opts)
            .await
            .expect("ok");
        let got = last_opts
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .expect("opts");
        assert_eq!(got.temperature, Some(0.9));
        assert_eq!(got.top_p, Some(0.8));
        assert_eq!(got.max_output_tokens, Some(1234));
    }

    #[tokio::test]
    async fn final_payload_round_trips_as_json_value() {
        let llm = Arc::new(ScriptedModel {
            replies: Arc::new(Mutex::new(vec![
                "{\"type\":\"final\",\"name\":null,\"args\":null,\"final\":{\"kind\":\"generic\",\"payload\":\"{\\\"obj\\\":{\\\"hello\\\":\\\"world\\\",\\\"n\\\":1}}\",\"display\":null}}".to_string(),
            ])),
        });
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let keyspace = Arc::new(DefaultKeyspace::new("bucket".to_string()));
        let scope = RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        };

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
            warehouse: Arc::new(NullWarehouseProvider::default()),
            dbt: None,
            vector: None,
            thread_store: None,
            exec_ctx: None,
            runtime: None,
        };

        let out = Agent::run_until_block(
            &reg,
            &ctx,
            "sys",
            "tools",
            "q",
            crate::llm::LlmCallOptions {
                prompt_id: "react_core.agent.tests.final_payload_round_trip",
                thread_id: None,
                expected_format: crate::llm::LlmExpectedFormat::JsonObject,
                max_output_tokens: None,
                temperature: None,
                top_p: None,
                reasoning_effort: None,
            },
        )
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
    fn parse_agent_step_repairs_raw_newlines_inside_json_strings() {
        // NOTE: This is intentionally invalid JSON: literal newline in the string value.
        let raw =
            "{\"type\":\"final\",\"name\":null,\"args\":null,\"final\":{\"kind\":\"generic\",\"payload\":\"{\\\"text\\\":\\\"line1\nline2\\\"}\",\"display\":null}}";
        let step = Agent::parse_agent_step(raw).expect("should repair and parse");
        match step {
            ParsedStep::Final { final_env } => {
                assert_eq!(final_env.kind, "generic");
                assert_eq!(
                    final_env.payload.get("text").and_then(|x| x.as_str()).unwrap(),
                    "line1\nline2"
                );
            }
            _ => panic!("expected final step"),
        }
    }

    #[test]
    fn parse_agent_step_rejects_concatenated_multiple_json_objects() {
        let raw = concat!(
            "{\"type\":\"tool\",\"name\":\"noop\",\"args\":\"{}\",\"final\":null}",
            "{\"type\":\"final\",\"name\":null,\"args\":null,\"final\":{\"kind\":\"generic\",\"payload\":\"{}\",\"display\":null}}"
        );
        let err = Agent::parse_agent_step(raw).expect_err("should reject concatenation");
        assert!(
            err.starts_with("invalid JSON from model:"),
            "unexpected err: {err}"
        );
    }

    struct NoopTool;
    #[async_trait]
    impl crate::tools::Tool for NoopTool {
        fn name(&self) -> &'static str {
            "noop"
        }
        async fn call(&self, _args: Value, _ctx: &AgentCtx) -> Result<Value, String> {
            Ok(serde_json::json!({"ok": true}))
        }
    }

    #[tokio::test]
    async fn invalid_json_from_model_is_retried_with_minimal_prompt() {
        let llm = Arc::new(ScriptedModel {
            replies: Arc::new(Mutex::new(vec![
                // Truncated/invalid JSON (EOF mid-string).
                "{\"type\":\"tool\",\"name\":\"noop\",\"args\":\"{".to_string(),
                // Retry succeeds.
                "{\"type\":\"tool\",\"name\":\"noop\",\"args\":\"{}\",\"final\":null}".to_string(),
                // Then final.
                "{\"type\":\"final\",\"name\":null,\"args\":null,\"final\":{\"kind\":\"generic\",\"payload\":\"{\\\"text\\\":\\\"ok\\\"}\",\"display\":null}}".to_string(),
            ])),
        });
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let keyspace = Arc::new(DefaultKeyspace::new("bucket".to_string()));
        let scope = RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        };

        let mut reg = ToolRegistry::new();
        reg.register(NoopTool);

        let ctx = AgentCtx {
            top_k: 1,
            per_step_timeout_secs: 1,
            max_steps: 8,
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
            warehouse: Arc::new(crate::providers::NullWarehouseProvider::default()),
            dbt: None,
            vector: None,
            thread_store: None,
            exec_ctx: None,
            runtime: None,
        };

        let out = Agent::run_until_block(
            &reg,
            &ctx,
            "sys",
            "tools",
            "q",
            crate::llm::LlmCallOptions {
                prompt_id: "react_core.agent.tests.retry_loop",
                thread_id: None,
                expected_format: crate::llm::LlmExpectedFormat::JsonObject,
                max_output_tokens: None,
                temperature: None,
                top_p: None,
                reasoning_effort: None,
            },
        )
        .await
        .expect("run should succeed after retry");

        match out {
            RunOutcome::Final { thread_id, .. } => {
                assert_eq!(thread_id, "tid".to_string());
            }
            _ => panic!("expected final outcome"),
        }
    }

    struct CapturingDbtFilesPatchTool {
        saw_patch: Arc<Mutex<bool>>,
    }

    #[async_trait]
    impl crate::tools::Tool for CapturingDbtFilesPatchTool {
        fn name(&self) -> &'static str {
            "dbt_files"
        }

        async fn call(&self, args: Value, _ctx: &AgentCtx) -> Result<Value, String> {
            let op = args.get("op").and_then(|v| v.as_str()).unwrap_or("");
            if op != "patch" {
                return Err(format!("expected op=patch, got op={}", op));
            }
            let has_rf = args.get("replace_file").is_some();
            let has_rr = args.get("replace_range").is_some();
            let has_rl = args.get("replace_list").is_some();
            let provided = (has_rf as usize) + (has_rr as usize) + (has_rl as usize);
            if provided != 1 {
                return Err("expected exactly one patch primitive".to_string());
            }
            if let Ok(mut g) = self.saw_patch.lock() {
                *g = true;
            }
            Ok(serde_json::json!({"ok": true}))
        }
    }

    #[tokio::test]
    async fn patch_protocol_response_is_wrapped_as_dbt_files_patch_action() {
        let llm = Arc::new(ScriptedModel {
            replies: Arc::new(Mutex::new(vec![
                // Model mistakenly emits patch-protocol response object (no action/final envelope).
                serde_json::json!({
                    "notes": ["example"],
                    "replace_range": {
                        "path": "models/staging/stg_x.yml",
                        "start_line": 1,
                        "end_line": 10,
                        "new_text": "version: 2\n"
                    }
                })
                .to_string(),
                // Retry: model emits a correct tool step.
                serde_json::json!({
                    "type": "tool",
                    "name": "dbt_files",
                    "args": "{\"op\":\"patch\",\"replace_range\":{\"path\":\"models/staging/stg_x.yml\",\"start_line\":1,\"end_line\":10,\"new_text\":\"version: 2\\n\"}}",
                    "final": null
                })
                .to_string(),
                // Then finish.
                "{\"type\":\"final\",\"name\":null,\"args\":null,\"final\":{\"kind\":\"generic\",\"payload\":\"{\\\"text\\\":\\\"ok\\\"}\",\"display\":null}}".to_string(),
            ])),
        });

        let saw_patch: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));

        let storage = Arc::new(InMemoryStorageAdapter::default());
        let keyspace = Arc::new(DefaultKeyspace::new("bucket".to_string()));
        let scope = RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        };

        let mut reg = ToolRegistry::new();
        reg.register(CapturingDbtFilesPatchTool {
            saw_patch: saw_patch.clone(),
        });

        let ctx = AgentCtx {
            top_k: 1,
            per_step_timeout_secs: 1,
            max_steps: 4,
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
            warehouse: Arc::new(NullWarehouseProvider::default()),
            dbt: None,
            vector: None,
            thread_store: None,
            // Critical: only wrap patch-protocol objects when exec_ctx exists.
            exec_ctx: Some(ExecutionContext {
                plan_kind: Some("cleanse".to_string()),
                plan_key: Some("k".to_string()),
                workgroup_id: Some("wg".to_string()),
                task_id: Some("t".to_string()),
                checklist_item_id: Some("schema_contract".to_string()),
            }),
            runtime: None,
        };

        let out = Agent::run_until_block(
            &reg,
            &ctx,
            "sys",
            "tools",
            "q",
            crate::llm::LlmCallOptions {
                prompt_id: "react_core.agent.tests.dbt_files_patch_invoked",
                thread_id: None,
                expected_format: crate::llm::LlmExpectedFormat::JsonObject,
                max_output_tokens: None,
                temperature: None,
                top_p: None,
                reasoning_effort: None,
            },
        )
        .await
        .expect("ok");

        let saw = match saw_patch.lock() {
            Ok(g) => *g,
            Err(_) => false,
        };
        assert!(saw, "expected dbt_files patch tool to be invoked");

        match out {
            RunOutcome::Final { result, .. } => {
                assert_eq!(result.kind, "generic");
            }
            _ => panic!("expected final outcome"),
        }
    }
}
