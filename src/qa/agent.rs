use serde_json::Value;
use tracing::{info, warn};

use crate::qa::tools::{ToolRegistry};
use crate::llm::ChatMessage;
use crate::qa::session::{ThreadStore, ThreadStep, ThreadResult};

pub struct AgentCtx {
    pub pipeline: String,
    pub namespace: Option<String>,
    pub top_k: usize,
    pub per_step_timeout_secs: u64,
    pub max_steps: usize,
    pub thread_id: Option<String>,
}

pub struct Agent;

impl Agent {
    pub async fn run(
        tools: &ToolRegistry,
        ctx: &AgentCtx,
        system_prompt: &str,
        tool_card: &str,
        user_prompt: &str,
    ) -> Result<ThreadResult, String> {
        let thread_id = ctx.thread_id.clone().unwrap_or_else(|| {
            format!("thread_{}_{}", ctx.pipeline, chrono::Utc::now().timestamp_millis())
        });
        let mut transcript: Vec<String> = Vec::new();
        transcript.push(system_prompt.to_string());
        transcript.push(tool_card.to_string());
        transcript.push(format!("Context: pipeline={} namespace={}", ctx.pipeline, ctx.namespace.clone().unwrap_or_default()));
        transcript.push(format!("Question: {}", user_prompt));

        let store = ThreadStore::new(&ctx.pipeline);
        let mut final_result: Option<ThreadResult> = None;

        for step in 0..ctx.max_steps {
            info!("ReAct step {}", step + 1);
            // Build LLM prompt from transcript
            let prompt = transcript.join("\n\n") + &format!("\n\nStep {}: Decide next action.", step + 1);
            // Use our LLM interface (OpenAI-compatible or local) via existing llm module
            let cfg = crate::llm::config_from_env();
            let model = crate::llm::create_llm(&cfg);
            let prompt_clone = prompt.clone();
            let act_json = match tokio::task::spawn_blocking(move || {
                model.chat(&[ChatMessage { role: "user".into(), content: prompt_clone }])
            }).await {
                Ok(Ok(s)) => s,
                _ => String::from("{}"),
            };

            // Parse action or final
            let parsed: Value = match serde_json::from_str(&act_json) {
                Ok(v) => v,
                Err(_) => {
                    warn!("Invalid JSON action from LLM; aborting.");
                    break;
                }
            };
            if let Some(final_obj) = parsed.get("final") {
                // End
                let sql = final_obj.get("sql").and_then(|x| x.as_str()).map(|s| s.to_string());
                let answer = final_obj.get("answer").and_then(|x| x.as_str()).map(|s| s.to_string()).unwrap_or_default();
                final_result = Some(ThreadResult { sql, answer });
                let _ = store.append_step(&thread_id, ThreadStep {
                    action: "final".to_string(),
                    args: final_obj.clone(),
                    observation: serde_json::json!({"ok": true}),
                    ts: chrono::Utc::now().to_rfc3339(),
                }).await;
                break;
            }

            let action_name = parsed.get("action").and_then(|x| x.as_str()).unwrap_or_default().to_string();
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


