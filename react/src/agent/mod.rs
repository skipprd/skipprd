use serde_json::Value;
use tracing::{info, warn};
use std::sync::Arc;

use crate::tools::ToolRegistry;
use crate::llm::ChatMessage;
use crate::session::{ThreadStore, ThreadStep, ThreadResult};
use crate::providers::{Keyspace, RequestScope};
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
    /// Optional DBT provider (suite-provided).
    pub dbt: Option<Arc<dyn DbtProvider>>,
    /// Optional vector store provider (suite-provided).
    pub vector: Option<Arc<dyn VectorStore>>,
    /// Optional thread store (for transcript persistence and artifact context).
    pub thread_store: Option<ThreadStore>,
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

        if let Some(store) = store_opt {
            if let Some(prev) = store.get(&thread_id).await {
                for step in prev.steps {
                    transcript.push(format!("Action: {} Args: {}", step.action, step.args));
                    transcript.push(format!("Observation: {}", step.observation));
                }
            }
        }

        // Simple loop-guard memory: last few short, repeating failures
        let mut recent: std::collections::VecDeque<(String, String, u128)> = std::collections::VecDeque::with_capacity(8);
        for step_idx in 0..ctx.max_steps {
            info!("ReAct step {}", step_idx + 1);
            let prompt = transcript.join("\n\n") + &format!("\n\nStep {}: Decide next action.", step_idx + 1);
            let prompt_clone = prompt.clone();
            let model_for_first = ctx.llm.clone();
            // Track LLM expense (chat) for this decision
            let mut chat_chars_in: usize = prompt.len();
            let mut chat_chars_out: usize = 0;
            let mut act_json = match tokio::task::spawn_blocking(move || {
                model_for_first.chat(&[ChatMessage { role: "user".into(), content: prompt_clone }])
            }).await {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    let el = e.to_lowercase();
                    if el.contains("http 429") || el.contains("status code 429") || el.contains("rate limit") {
                        return Err(format!("LLM rate limited (429): {}", e));
                    }
                    return Err(format!("LLM request failed: {}", e));
                }
                Err(e) => {
                    return Err(format!("LLM execution failed: {}", e));
                }
            };
            chat_chars_out += act_json.len();
            // Parse JSON, with one repair attempt if invalid
            let mut parsed: Option<Value> = serde_json::from_str(&act_json).ok();
            if parsed.is_none() {
                warn!("Invalid JSON action from LLM; requesting strict JSON re-emission.");
                let repair_prompt = format!(
                    "{}\n\nPrevious output was not valid JSON:\n{}\n\nRe-emit STRICT JSON ONLY per the schemas. No prose.",
                    transcript.join("\n\n"),
                    act_json
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
                if parsed.is_none() {
                    // Record the failure as an observation and continue loop
                    transcript.push(format!("Observation: parser_error invalid JSON twice; raw='{}'", act_json2));
                    continue;
                }
            }
            let parsed = parsed.unwrap();
            if let Some(final_obj) = parsed.get("final") {
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
            if let Some(tx) = ctx.pre_step_tx.as_ref() {
                let _ = tx.send(action_name.clone());
            }
            let args = parsed.get("args").cloned().unwrap_or(Value::Null);
            let start = std::time::Instant::now();
            let mut obs = match tools.call(&action_name, args.clone(), ctx).await {
                Ok(o) => o,
                Err(e) => serde_json::json!({"ok": false, "error": e}),
            };
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
            transcript.push(format!("Action: {} Args: {}", action_name, args));
            transcript.push(format!("Observation: {}", obs));
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


