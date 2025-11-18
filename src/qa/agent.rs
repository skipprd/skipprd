use serde_json::Value;
use tracing::{info, warn};

use crate::qa::tools::{ToolRegistry};
use crate::llm::ChatMessage;
use crate::qa::session::{ThreadStore, ThreadStep, ThreadResult, ThreadLog};
use uuid::Uuid;

#[derive(Clone)]
pub struct AgentCtx {
    pub pipeline: String,
    pub namespace: Option<String>,
    pub top_k: usize,
    pub per_step_timeout_secs: u64,
    pub max_steps: usize,
    pub thread_id: Option<String>,
    pub progress_tx: Option<tokio::sync::mpsc::UnboundedSender<usize>>,
    pub pre_step_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    pub agent_name: Option<String>,
}

pub struct Agent;

pub enum RunOutcome {
    Final { thread_id: String, result: ThreadResult },
    AwaitUser { thread_id: String, prompt: String },
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
        let mut transcript: Vec<String> = Vec::new();
        transcript.push(system_prompt.to_string());
        transcript.push(tool_card.to_string());
        transcript.push(format!("Context: pipeline={} namespace={}", ctx.pipeline, ctx.namespace.clone().unwrap_or_default()));
        transcript.push(format!("Question: {}", user_prompt));

        let store = ThreadStore::new();
        if let Some(prev) = store.get(&thread_id).await {
            for step in prev.steps {
                transcript.push(format!("Action: {} Args: {}", step.action, step.args));
                transcript.push(format!("Observation: {}", step.observation));
            }
        }

        for step_idx in 0..ctx.max_steps {
            info!("ReAct step {}", step_idx + 1);
            let prompt = transcript.join("\n\n") + &format!("\n\nStep {}: Decide next action.", step_idx + 1);
            let cfg = crate::llm::config_from_env();
            let model = crate::llm::create_llm(&cfg);
            let prompt_clone = prompt.clone();
            let model_for_first = model.clone();
            let act_json = match tokio::task::spawn_blocking(move || {
                model_for_first.chat(&[ChatMessage { role: "user".into(), content: prompt_clone }])
            }).await {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    return Err(format!("LLM not configured: {}", e));
                }
                Err(e) => {
                    return Err(format!("LLM execution failed: {}", e));
                }
            };
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
                let model_for_second = model.clone();
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
                if parsed.is_none() {
                    // Record the failure as an observation and continue loop
                    transcript.push(format!("Observation: parser_error invalid JSON twice; raw='{}'", act_json2));
                    continue;
                }
            }
            let parsed = parsed.unwrap();
            if let Some(final_obj) = parsed.get("final") {
                let sql_opt = final_obj.get("sql").and_then(|x| x.as_str()).map(|s| s.to_string());
                // Enforce FQN: reject if sql references 'default.' or lacks any allowed <pipeline>.<namespace>
                if let Some(sql_str) = sql_opt.as_ref() {
                    let sql_lower = sql_str.to_lowercase();
                    if sql_lower.contains(" default.") {
                        transcript.push("Observation: invalid final SQL - 'default.*' schema is forbidden. Use fully-qualified <pipeline>.<namespace>.".to_string());
                        // Provide allowed datasets
                        let mut allowed: Vec<String> = Vec::new();
                        let pipes = crate::sql::registry::list_pipelines().await;
                        for p in pipes {
                            let nss = crate::sql::registry::list_namespaces(&p).await;
                            for ns in nss { allowed.push(format!("{}.{}", p, ns)); }
                        }
                        if !allowed.is_empty() {
                            transcript.push(format!("AllowedDatasets: {}", allowed.join(", ")));
                        }
                        continue;
                    }
                    // Must contain at least one allowed dataset token
                    let mut ok_fqn = false;
                    let pipes2 = crate::sql::registry::list_pipelines().await;
                    'outer: for p in pipes2 {
                        let nss = crate::sql::registry::list_namespaces(&p).await;
                        for ns in nss {
                            let fqn = format!("{}.{}", p, ns);
                            if sql_str.contains(&fqn) { ok_fqn = true; break 'outer; }
                        }
                    }
                    if !ok_fqn {
                        transcript.push("Observation: invalid final SQL - no fully-qualified dataset found. Use <pipeline>.<namespace> and try again.".to_string());
                        continue;
                    }
                }
                // Require SQL and successful data before accepting final
                let sql_for_run = match sql_opt.as_ref() {
                    Some(s) if !s.trim().is_empty() => s.clone(),
                    _ => {
                        transcript.push("Observation: final requires a valid SQL and data; please provide SQL and call run_sql before finalizing.".to_string());
                        continue;
                    }
                };
                let obs = match tools.call("run_sql", serde_json::json!({"sql": sql_for_run}), ctx).await {
                    Ok(o) => o,
                    Err(e) => serde_json::json!({"ok": false, "error": e}),
                };
                let ok = obs.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                let rows_non_empty = obs.get("rows")
                    .and_then(|r| serde_json::from_value::<Vec<Vec<String>>>(r.clone()).ok())
                    .map(|r| !r.is_empty())
                    .unwrap_or(false);
                // Record the run_sql attempt
                let _ = store.append_step(&thread_id, ThreadStep {
                    action: "run_sql".to_string(),
                    args: serde_json::json!({"sql": sql_for_run}),
                    observation: obs.clone(),
                    ts: chrono::Utc::now().to_rfc3339(),
                    agent: ctx.agent_name.clone(),
                }).await;
                if !(ok && rows_non_empty) {
                    let err_text = obs.get("error").and_then(|x| x.as_str()).unwrap_or("no data");
                    transcript.push(format!("Observation: data_validation_failed reason='{}'; fix SQL and try again.", err_text));
                    continue;
                }
                let answer = final_obj.get("answer").and_then(|x| x.as_str()).map(|s| s.to_string()).unwrap_or_default();
                let result = ThreadResult { sql: sql_opt, answer };
                let _ = store.append_step(&thread_id, ThreadStep {
                    action: "final".to_string(),
                    args: final_obj.clone(),
                    observation: serde_json::json!({"ok": true}),
                    ts: chrono::Utc::now().to_rfc3339(),
                    agent: ctx.agent_name.clone(),
                }).await;
                return Ok(RunOutcome::Final { thread_id, result });
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
            let obs = match tools.call(&action_name, args.clone(), ctx).await {
                Ok(o) => o,
                Err(e) => serde_json::json!({"ok": false, "error": e}),
            };
            transcript.push(format!("Action: {} Args: {}", action_name, args));
            transcript.push(format!("Observation: {}", obs));
            let _ = store.append_step(&thread_id, ThreadStep {
                action: action_name.clone(),
                args,
                observation: obs.clone(),
                ts: chrono::Utc::now().to_rfc3339(),
                agent: ctx.agent_name.clone(),
            }).await;
            if let Some(tx) = ctx.progress_tx.as_ref() {
                let _ = tx.send(step_idx + 1);
            }
            if action_name == "ask_user" {
                let prompt = obs.get("prompt").and_then(|x| x.as_str()).unwrap_or("Please provide additional context.").to_string();
                return Ok(RunOutcome::AwaitUser { thread_id, prompt });
            }
        }
        // If we get here, no final or ask_user; return a minimal result to avoid blocking.
        Ok(RunOutcome::Final { thread_id, result: ThreadResult { sql: None, answer: "No result".to_string() } })
    }

    pub async fn run(
        tools: &ToolRegistry,
        ctx: &AgentCtx,
        system_prompt: &str,
        tool_card: &str,
        user_prompt: &str,
    ) -> Result<ThreadResult, String> {
        let thread_id = ctx.thread_id.clone().unwrap_or_else(|| Agent::gen_uuid());
        let mut transcript: Vec<String> = Vec::new();
        transcript.push(system_prompt.to_string());
        transcript.push(tool_card.to_string());
        transcript.push(format!("Context: pipeline={} namespace={}", ctx.pipeline, ctx.namespace.clone().unwrap_or_default()));
        transcript.push(format!("Question: {}", user_prompt));

        let store = ThreadStore::new();
        let mut final_result: Option<ThreadResult> = None;

        for step in 0..ctx.max_steps {
            info!("ReAct step {}", step + 1);
            // Build LLM prompt from transcript
            let prompt = transcript.join("\n\n") + &format!("\n\nStep {}: Decide next action.", step + 1);
            // Use our LLM interface (OpenAI-compatible or local) via existing llm module
            let cfg = crate::llm::config_from_env();
            let model = crate::llm::create_llm(&cfg);
            let prompt_clone = prompt.clone();
            let model_for_first = model.clone();
            let act_json = match tokio::task::spawn_blocking(move || {
                model_for_first.chat(&[ChatMessage { role: "user".into(), content: prompt_clone }])
            }).await {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    return Err(format!("LLM not configured: {}", e));
                }
                Err(e) => {
                    return Err(format!("LLM execution failed: {}", e));
                }
            };

            // Parse action or final
            let mut parsed: Option<Value> = serde_json::from_str(&act_json).ok();
            if parsed.is_none() {
                warn!("Invalid JSON action from LLM; requesting strict JSON re-emission.");
                let repair_prompt = format!(
                    "{}\n\nPrevious output was not valid JSON:\n{}\n\nRe-emit STRICT JSON ONLY per the schemas. No prose.",
                    transcript.join("\n\n"),
                    act_json
                );
                let prompt_clone2 = repair_prompt.clone();
                let model_for_second = model.clone();
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
                if parsed.is_none() {
                    transcript.push(format!("Observation: parser_error invalid JSON twice; raw='{}'", act_json2));
                    continue;
                }
            }
            let parsed = parsed.unwrap();
            if let Some(final_obj) = parsed.get("final") {
                // End
                let sql_opt = final_obj.get("sql").and_then(|x| x.as_str()).map(|s| s.to_string());
                if let Some(sql_str) = sql_opt.as_ref() {
                    let sql_lower = sql_str.to_lowercase();
                    if sql_lower.contains(" default.") {
                        transcript.push("Observation: invalid final SQL - 'default.*' schema is forbidden. Use fully-qualified <pipeline>.<namespace>.".to_string());
                        let mut allowed: Vec<String> = Vec::new();
                        let pipes = crate::sql::registry::list_pipelines().await;
                        for p in pipes {
                            let nss = crate::sql::registry::list_namespaces(&p).await;
                            for ns in nss { allowed.push(format!("{}.{}", p, ns)); }
                        }
                        if !allowed.is_empty() {
                            transcript.push(format!("AllowedDatasets: {}", allowed.join(", ")));
                        }
                        continue;
                    }
                    let mut ok_fqn = false;
                    let pipes2 = crate::sql::registry::list_pipelines().await;
                    'outer: for p in pipes2 {
                        let nss = crate::sql::registry::list_namespaces(&p).await;
                        for ns in nss {
                            let fqn = format!("{}.{}", p, ns);
                            if sql_str.contains(&fqn) { ok_fqn = true; break 'outer; }
                        }
                    }
                    if !ok_fqn {
                        transcript.push("Observation: invalid final SQL - no fully-qualified dataset found. Use <pipeline>.<namespace> and try again.".to_string());
                        continue;
                    }
                }
                let sql_for_run = match sql_opt.as_ref() {
                    Some(s) if !s.trim().is_empty() => s.clone(),
                    _ => {
                        transcript.push("Observation: final requires a valid SQL and data; please provide SQL and call run_sql before finalizing.".to_string());
                        continue;
                    }
                };
                let obs = match tools.call("run_sql", serde_json::json!({"sql": sql_for_run}), ctx).await {
                    Ok(o) => o,
                    Err(e) => serde_json::json!({"ok": false, "error": e}),
                };
                let ok = obs.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                let rows_non_empty = obs.get("rows")
                    .and_then(|r| serde_json::from_value::<Vec<Vec<String>>>(r.clone()).ok())
                    .map(|r| !r.is_empty())
                    .unwrap_or(false);
                let _ = store.append_step(&thread_id, ThreadStep {
                    action: "run_sql".to_string(),
                    args: serde_json::json!({"sql": sql_for_run}),
                    observation: obs.clone(),
                    ts: chrono::Utc::now().to_rfc3339(),
                    agent: ctx.agent_name.clone(),
                }).await;
                if !(ok && rows_non_empty) {
                    let err_text = obs.get("error").and_then(|x| x.as_str()).unwrap_or("no data");
                    transcript.push(format!("Observation: data_validation_failed reason='{}'; fix SQL and try again.", err_text));
                    continue;
                }
                let answer = final_obj.get("answer").and_then(|x| x.as_str()).map(|s| s.to_string()).unwrap_or_default();
                final_result = Some(ThreadResult { sql: sql_opt, answer });
                let _ = store.append_step(&thread_id, ThreadStep {
                    action: "final".to_string(),
                    args: final_obj.clone(),
                    observation: serde_json::json!({"ok": true}),
                    ts: chrono::Utc::now().to_rfc3339(),
                    agent: ctx.agent_name.clone(),
                }).await;
                break;
            }

            let action_name = parsed.get("action").and_then(|x| x.as_str()).unwrap_or_default().to_string();
            if action_name.trim().is_empty() {
                transcript.push("Observation: invalid action - empty; retrying next step.".to_string());
                continue;
            }
            let args = parsed.get("args").cloned().unwrap_or(Value::Null);

            let obs = match tools.call(&action_name, args.clone(), ctx).await {
                Ok(o) => o,
                Err(e) => serde_json::json!({"ok": false, "error": e}),
            };
            transcript.push(format!("Action: {} Args: {}", action_name, args));
            transcript.push(format!("Observation: {}", obs));
            let _ = store.append_step(&thread_id, ThreadStep {
                action: action_name,
                args,
                observation: obs,
                ts: chrono::Utc::now().to_rfc3339(),
                agent: ctx.agent_name.clone(),
            }).await;
        }

        let out = final_result.unwrap_or(ThreadResult { sql: None, answer: "No result".to_string() });
        // Print thread log for easier debugging
        if let Some(log) = store.get(&thread_id).await {
            if let Ok(pretty) = serde_json::to_string_pretty(&log) {
                println!("{}", pretty);
            }
        }
        Ok(out)
    }
}


