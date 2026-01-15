use async_trait::async_trait;
use serde_json::Value;

use crate::agent::{AgentPolicy, Interrupt, RunOutcome};
use crate::session::{ThreadResult, ThreadStep, ThreadStore};
use crate::tools::ToolRegistry;

use super::types::DatasetCandidate;

/// Policy for analytics-style suites: accept a model-emitted `final` only after validating that
/// `final.sql` runs successfully (via the `run_sql` tool) and returns at least one row.
///
/// This policy also supports user/approval interrupts by configured tool names.
pub struct SqlValidatedPolicy {
    pub dataset_candidates: Vec<DatasetCandidate>,
    pub user_tool: &'static str,
    pub approval_tool: &'static str,
}

impl Default for SqlValidatedPolicy {
    fn default() -> Self {
        Self {
            dataset_candidates: Vec::new(),
            user_tool: "ask_user",
            approval_tool: "ask_approval",
        }
    }
}

#[async_trait]
impl AgentPolicy for SqlValidatedPolicy {
    fn prelude_lines(
        &self,
        _ctx: &crate::agent::AgentCtx,
        _store: Option<&ThreadStore>,
        _thread_id: &str,
    ) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if !self.dataset_candidates.is_empty() {
            let mut lines: Vec<String> = Vec::new();
            lines.push("ResolvedDatasets:".to_string());
            for c in self.dataset_candidates.iter().take(6) {
                lines.push(format!("- {} (score={:.4})", c.dataset_id, c.score));
            }
            out.push(lines.join("\n"));
        }
        out
    }

    fn interrupt_for_action(&self, action_name: &str, args: &Value, obs: &Value) -> Option<Interrupt> {
        if action_name == self.user_tool {
            let prompt = args
                .get("prompt")
                .and_then(|x| x.as_str())
                .or_else(|| obs.get("prompt").and_then(|x| x.as_str()))
                .unwrap_or("Please provide additional context.")
                .to_string();
            return Some(Interrupt::AwaitUser { prompt });
        }
        if action_name == self.approval_tool {
            let prompt = args
                .get("prompt")
                .and_then(|x| x.as_str())
                .or_else(|| obs.get("prompt").and_then(|x| x.as_str()))
                .unwrap_or("Please review and approve/reject.")
                .to_string();
            return Some(Interrupt::AwaitApproval { prompt });
        }
        None
    }

    async fn handle_final(
        &self,
        tools: &ToolRegistry,
        ctx: &crate::agent::AgentCtx,
        transcript: &mut Vec<String>,
        store: Option<&ThreadStore>,
        thread_id: &str,
        final_obj: &Value,
    ) -> Result<Option<RunOutcome>, String> {
        let sql_opt = final_obj.get("sql").and_then(|x| x.as_str()).map(|s| s.to_string());
        if let Some(sql_str) = sql_opt.as_ref() {
            let sql_lower = sql_str.to_lowercase();
            if sql_lower.contains(" default.") {
                transcript.push("Observation: invalid final SQL - 'default.*' schema is forbidden. Use fully-qualified <catalog>.<database>.<table>.".to_string());
                return Ok(None);
            }
        }
        let sql_for_run = match sql_opt.as_ref() {
            Some(s) if !s.trim().is_empty() => s.clone(),
            _ => {
                transcript.push("Observation: final requires a valid SQL and data; please provide SQL and call run_sql before finalizing.".to_string());
                return Ok(None);
            }
        };
        let obs = match tools.call("run_sql", serde_json::json!({"sql": sql_for_run}), ctx).await {
            Ok(o) => o,
            Err(e) => serde_json::json!({"ok": false, "error": e}),
        };
        let ok = obs.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        let rows_non_empty = obs
            .get("rows")
            .and_then(|r| serde_json::from_value::<Vec<Vec<String>>>(r.clone()).ok())
            .map(|r| !r.is_empty())
            .unwrap_or(false);
        if let Some(store) = store {
            let _ = store
                .append_step(
                    thread_id,
                    ThreadStep {
                        action: "run_sql".to_string(),
                        args: serde_json::json!({"sql": sql_for_run}),
                        observation: obs.clone(),
                        ts: chrono::Utc::now().to_rfc3339(),
                        agent: ctx.agent_name.clone(),
                    },
                )
                .await;
        }
        if !(ok && rows_non_empty) {
            let err_text = obs.get("error").and_then(|x| x.as_str()).unwrap_or("no data");
            transcript.push(format!("Observation: data_validation_failed reason='{}'; fix SQL and try again.", err_text));
            return Ok(None);
        }
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

    async fn fallback(
        &self,
        _tools: &ToolRegistry,
        _ctx: &crate::agent::AgentCtx,
        _transcript: &mut Vec<String>,
        store: Option<&ThreadStore>,
        thread_id: &str,
    ) -> Result<RunOutcome, String> {
        // Preserve previous behavior: if artifacts were saved, summarize them.
        let mut summary = String::from("No result.");
        if let Some(store) = store {
            if let Some(log) = store.get(thread_id).await {
                let mut keys: Vec<String> = Vec::new();
                for step in log.steps.iter().rev() {
                    if step.action == "approve_and_save_artifact_batch" {
                        if let Some(arr) = step.observation.get("keys").and_then(|x| x.as_array()) {
                            for v in arr {
                                if let Some(s) = v.as_str() {
                                    keys.push(s.to_string());
                                }
                            }
                        }
                        break;
                    }
                    if step.action == "artifact_saved" {
                        if let Some(k) = step.observation.get("key").and_then(|x| x.as_str()) {
                            keys.push(k.to_string());
                        }
                    }
                    if keys.len() >= 12 {
                        break;
                    }
                }
                if !keys.is_empty() {
                    let shown: Vec<String> = keys.iter().take(6).cloned().collect();
                    let extra = if keys.len() > 6 { format!(" (+{} more)", keys.len() - 6) } else { String::new() };
                    summary = format!("Saved {} artifact(s): {}{}", keys.len(), shown.join(", "), extra);
                }
            }
        }
        Ok(RunOutcome::Final {
            thread_id: thread_id.to_string(),
            result: ThreadResult { sql: None, answer: summary },
        })
    }
}

