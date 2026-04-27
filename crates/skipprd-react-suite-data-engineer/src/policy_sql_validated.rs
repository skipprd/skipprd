use async_trait::async_trait;
use serde_json::Value;

use crate::thread_cache::ThreadCacheStore;
use react_core::agent::{
    AgentPolicy, CompleteDecision, CompleteEnvelope, InterruptKind, RunOutcome,
};
use react_core::session::{Observation, ThreadResult, ThreadStep, ThreadStore, ToolStepMeta};
use react_core::tools::ToolRegistry;

#[derive(Clone)]
pub struct DatasetCandidate {
    pub dataset_id: String,
    pub score: f32,
}

/// Policy for analytics-style suites: accept a model-emitted complete step only after validating that
/// Ask-mode `complete.payload.sql` runs successfully (via the `run_sql` tool) and returns at least one row.
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
        _ctx: &react_core::agent::AgentCtx,
        _store: Option<&ThreadStore>,
        thread_id: &str,
    ) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if let Some(cache) = ThreadCacheStore::get(thread_id) {
            if !cache.published_relations.is_empty() {
                let mut lines: Vec<String> = Vec::new();
                lines.push(
                    "PublishedRelations (prefer these over bronze/raw when possible):".to_string(),
                );
                for r in cache.published_relations.iter().take(10) {
                    lines.push(format!("- {}", r));
                }
                out.push(lines.join("\n"));
            }
        }
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

    fn interrupt_for_action(
        &self,
        action_name: &str,
        args: &Value,
        obs: &Value,
    ) -> Option<(InterruptKind, String)> {
        if action_name == self.user_tool {
            let prompt = args
                .get("prompt")
                .and_then(|x| x.as_str())
                .or_else(|| obs.get("prompt").and_then(|x| x.as_str()))
                .unwrap_or("Please provide additional context.")
                .to_string();
            return Some((InterruptKind::AwaitUser, prompt));
        }
        if action_name == self.approval_tool {
            let prompt = args
                .get("prompt")
                .and_then(|x| x.as_str())
                .or_else(|| obs.get("prompt").and_then(|x| x.as_str()))
                .unwrap_or("Please review and approve/reject.")
                .to_string();
            return Some((InterruptKind::AwaitApproval, prompt));
        }
        if action_name == "publish_dbt_to_provider" {
            let awaiting = obs
                .get("await_approval")
                .and_then(|x| x.as_bool())
                .unwrap_or(false);
            if awaiting {
                let prompt = obs
                    .get("prompt")
                    .and_then(|x| x.as_str())
                    .unwrap_or("Please review and approve/reject.")
                    .to_string();
                return Some((InterruptKind::AwaitApproval, prompt));
            }
        }
        None
    }

    fn clean_tool_name(&self, name: &str, args: &serde_json::Value) -> String {
        crate::control_flow::de_clean_tool_name(name, args)
    }

    async fn handle_complete(
        &self,
        tools: &ToolRegistry,
        ctx: &react_core::agent::AgentCtx,
        transcript: &mut Vec<String>,
        store: Option<&ThreadStore>,
        thread_id: &str,
        complete_env: &CompleteEnvelope,
    ) -> Result<CompleteDecision, String> {
        let is_authoring = matches!(ctx.agent_name().as_deref(), Some("model") | Some("cleanse"));
        let is_ask = matches!(ctx.agent_name().as_deref(), Some("ask"));

        // For DBT-authoring agents, require successful dbt_validate before allowing completion.
        //
        // IMPORTANT:
        // - If artifacts were authored, a compile-only validate is not sufficient to complete; we require
        //   a runtime validate (build/run) to pass (run_ok=true) unless explicitly overridden.
        // - This prevents "compiles cleanly" completions that still have runtime/test failures.
        if is_authoring {
            if let Some(store) = store {
                let log = match store.get(thread_id).await {
                    Ok(l) => l,
                    Err(e) => {
                        let reason = format!(
                            "dbt_validate_required; thread log unavailable so validation status is unknown ({e}). Call dbt_validate before completing."
                        );
                        transcript.push(format!("Observation: {reason}"));
                        return Ok(CompleteDecision::Reject { reason });
                    }
                };
                let mut has_artifacts = false;
                for step in log.steps.iter().rev() {
                    match step {
                        ThreadStep::ArtifactSaved { .. } => {
                            has_artifacts = true;
                            break;
                        }
                        ThreadStep::ToolEnd { name, .. }
                            if name == "approve_and_save_artifact_batch" =>
                        {
                            has_artifacts = true;
                            break;
                        }
                        _ => {}
                    }
                }
                if has_artifacts {
                    let mut last_validate: Option<&ThreadStep> = None;
                    for step in log.steps.iter().rev() {
                        match step {
                            ThreadStep::ToolEnd { name, .. } if name == "dbt_validate" => {
                                last_validate = Some(step);
                                break;
                            }
                            _ => {}
                        }
                    }
                    // If artifacts were written, we require an explicit dbt_validate step.
                    // (Suite-level post-run validation is not sufficient for this policy gate.)
                    let Some(v) = last_validate else {
                        let reason =
                            "dbt_validate_required; you must run dbt_validate after writing artifacts before completing.".to_string();
                        transcript.push(format!("Observation: {reason}"));
                        return Ok(CompleteDecision::Reject { reason });
                    };

                    let (ok, compile_ok, run_ok, build, run) = match v {
                        ThreadStep::ToolEnd {
                            args, observation, ..
                        } => {
                            let ok = observation.ok;
                            let compile_ok = observation
                                .extra
                                .get("compile_ok")
                                .and_then(|x| x.as_bool())
                                .unwrap_or(false);
                            let run_ok = observation.extra.get("run_ok").and_then(|x| x.as_bool());
                            let build =
                                args.get("build").and_then(|x| x.as_bool()).unwrap_or(false);
                            let run = args.get("run").and_then(|x| x.as_bool()).unwrap_or(false);
                            (ok, compile_ok, run_ok, build, run)
                        }
                        _ => (false, false, None, false, false),
                    };
                    let runtime_validate = build || run || run_ok.is_some();
                    let allow_compile_only = crate::env_util::dbt_allow_compile_only_complete();

                    if !(ok && compile_ok) {
                        let reason =
                            "dbt_validate_failed; you must fix DBT artifacts and re-run dbt_validate until compile_ok=true before completing."
                                .to_string();
                        transcript.push(format!("Observation: {reason}"));
                        return Ok(CompleteDecision::Reject { reason });
                    }

                    if !runtime_validate && !allow_compile_only {
                        let reason =
                            "dbt_validate_incomplete; compile-only validation is not sufficient after authoring DBT artifacts. Re-run dbt_validate with build=true (or run=true) and fix any runtime/test failures before completing."
                                .to_string();
                        transcript.push(format!("Observation: {reason}"));
                        return Ok(CompleteDecision::Reject { reason });
                    }

                    if runtime_validate && run_ok != Some(true) {
                        let reason =
                            "dbt_validate_run_failed; you must fix runtime/test failures and re-run dbt_validate until run_ok=true before completing."
                                .to_string();
                        transcript.push(format!("Observation: {reason}"));
                        return Ok(CompleteDecision::Reject { reason });
                    }
                }
            } else {
                let reason =
                    "dbt_validate_required; thread_store missing so validation status is unknown. Call dbt_validate before completing."
                        .to_string();
                transcript.push(format!("Observation: {reason}"));
                return Ok(CompleteDecision::Reject { reason });
            }

            // Authoring agents complete without SQL validation (Ask-only concern).
            let result = ThreadResult {
                kind: complete_env.kind.clone(),
                payload: complete_env.payload.clone(),
                display: complete_env.display.clone(),
            };
            if let Some(store) = store {
                let agent = ctx
                    .agent_name()
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string());
                let _ = store
                    .append_step(
                        thread_id,
                        ThreadStep::Complete {
                            kind: result.kind.clone(),
                            payload: result.payload.clone(),
                            display: result.display.clone(),
                            observation: Observation::ok(),
                            ts: chrono::Utc::now().to_rfc3339(),
                            agent,
                        },
                    )
                    .await;
            }
            return Ok(CompleteDecision::Accept { result });
        }

        // Ask mode: require and validate SQL+data before completing.
        if !is_ask {
            let reason =
                "invalid_complete_kind; only ask/model/cleanse agents may complete in this suite."
                    .to_string();
            transcript.push(format!("Observation: {reason}"));
            return Ok(CompleteDecision::Reject { reason });
        }
        if complete_env.kind != "ask" {
            let reason = format!(
                "invalid_complete_kind; ask agent must complete with complete.kind='ask' (got '{}').",
                complete_env.kind
            );
            transcript.push(format!("Observation: {reason}"));
            return Ok(CompleteDecision::Reject { reason });
        }

        let sql_opt = complete_env
            .payload
            .get("sql")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        if let Some(sql_str) = sql_opt.as_ref() {
            let sql_lower = sql_str.to_lowercase();
            if sql_lower.contains(" default.") {
                let reason = "invalid complete SQL - 'default.*' schema is forbidden. Use fully-qualified <catalog>.<database>.<table>.".to_string();
                transcript.push(format!("Observation: {reason}"));
                return Ok(CompleteDecision::Reject { reason });
            }
        }
        let sql_for_run = match sql_opt.as_ref() {
            Some(s) if !s.trim().is_empty() => s.clone(),
            _ => {
                let reason = "complete requires a valid SQL and data; please provide SQL and call run_sql before completing.".to_string();
                transcript.push(format!("Observation: {reason}"));
                return Ok(CompleteDecision::Reject { reason });
            }
        };
        let obs = if let Some(store) = store {
            let meta = ToolStepMeta {
                agent: ctx
                    .agent_name()
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
                phase: "sql_validation".to_string(),
                name: "run_sql".to_string(),
                clean_name: "Run SQL".to_string(),
                args: serde_json::json!({"sql": sql_for_run}),
                ctx: None,
            };
            let sql = sql_for_run.clone();
            store
                .run_observed(
                    thread_id,
                    meta,
                    || async {
                        Ok(
                            match tools
                                .call("run_sql", serde_json::json!({"sql": sql}), ctx)
                                .await
                            {
                                Ok(o) => o,
                                Err(e) => serde_json::json!({"ok": false, "errors": [e]}),
                            },
                        )
                    },
                    |raw: &Value| Ok(raw.clone()),
                )
                .await
                .unwrap_or_else(|e| serde_json::json!({"ok": false, "errors": [e]}))
        } else {
            match tools
                .call("run_sql", serde_json::json!({"sql": sql_for_run}), ctx)
                .await
            {
                Ok(o) => o,
                Err(e) => serde_json::json!({"ok": false, "errors": [e]}),
            }
        };
        let ok = obs.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        let rows_non_empty = obs
            .get("rows")
            .and_then(|r| serde_json::from_value::<Vec<Vec<String>>>(r.clone()).ok())
            .map(|r| !r.is_empty())
            .unwrap_or(false);
        if !(ok && rows_non_empty) {
            let err_text = obs
                .get("errors")
                .and_then(|x| x.as_array())
                .and_then(|a| a.get(0))
                .and_then(|v| v.as_str())
                .unwrap_or("no data");
            let reason = format!(
                "data_validation_failed reason='{}'; fix SQL and try again.",
                err_text
            );
            transcript.push(format!("Observation: {reason}"));
            return Ok(CompleteDecision::Reject { reason });
        }
        let result = ThreadResult {
            kind: complete_env.kind.clone(),
            payload: complete_env.payload.clone(),
            display: complete_env.display.clone(),
        };
        if let Some(store) = store {
            let agent = ctx
                .agent_name()
                .clone()
                .unwrap_or_else(|| "unknown".to_string());
            let _ = store
                .append_step(
                    thread_id,
                    ThreadStep::Complete {
                        kind: result.kind.clone(),
                        payload: result.payload.clone(),
                        display: result.display.clone(),
                        observation: Observation::ok(),
                        ts: chrono::Utc::now().to_rfc3339(),
                        agent,
                    },
                )
                .await;
        }
        Ok(CompleteDecision::Accept { result })
    }

    async fn fallback(
        &self,
        _tools: &ToolRegistry,
        _ctx: &react_core::agent::AgentCtx,
        _transcript: &mut Vec<String>,
        store: Option<&ThreadStore>,
        thread_id: &str,
    ) -> Result<RunOutcome, String> {
        // Preserve previous behavior: if artifacts were saved, summarize them.
        let mut summary = String::from("No result.");
        if let Some(store) = store {
            if let Ok(log) = store.get(thread_id).await {
                let mut keys: Vec<String> = Vec::new();
                for step in log.steps.iter().rev() {
                    match step {
                        ThreadStep::ToolEnd {
                            name, observation, ..
                        } if name == "approve_and_save_artifact_batch" => {
                            if let Some(arr) =
                                observation.extra.get("keys").and_then(|x| x.as_array())
                            {
                                for v in arr {
                                    if let Some(s) = v.as_str() {
                                        keys.push(s.to_string());
                                    }
                                }
                            }
                            break;
                        }
                        ThreadStep::ArtifactSaved { key, .. } => {
                            keys.push(key.to_string());
                        }
                        _ => {}
                    }
                    if keys.len() >= 12 {
                        break;
                    }
                }
                if !keys.is_empty() {
                    let shown: Vec<String> = keys.iter().take(6).cloned().collect();
                    let extra = if keys.len() > 6 {
                        format!(" (+{} more)", keys.len() - 6)
                    } else {
                        String::new()
                    };
                    summary = format!(
                        "Saved {} artifact(s): {}{}",
                        keys.len(),
                        shown.join(", "),
                        extra
                    );
                }
            }
        }
        Ok(RunOutcome::Complete {
            thread_id: thread_id.to_string(),
            result: ThreadResult {
                kind: "generic".to_string(),
                payload: serde_json::json!({ "text": summary.clone() }),
                display: Some(summary),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use react_core::keyspace::DefaultKeyspace;
    use react_core::scope::RequestScope;
    use react_core::session::ToolObservation;
    use react_module_storage_memory::InMemoryStorageAdapter;
    use std::sync::Arc;

    #[tokio::test]
    async fn model_final_is_rejected_after_failed_dbt_validate() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let store = ThreadStore::new(storage.clone(), scope.clone(), keyspace.clone());
        // Use a unique thread id to avoid cross-test cache collisions.
        let tid = format!(
            "tid-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );

        // Simulate an artifact save, then a failed dbt_validate.
        let _ = store
            .append_step(
                &tid,
                ThreadStep::ArtifactSaved {
                    kind: react_core::session::ArtifactKind("model".to_string()),
                    name: "m".to_string(),
                    entity_id: Some("d".to_string()),
                    key: "dbt/models/d/m.sql".to_string(),
                    status: react_core::session::ArtifactSaveStatus::Added,
                    lines_added: 1,
                    lines_removed: 0,
                    observation: Observation::ok(),
                    ts: chrono::Utc::now().to_rfc3339(),
                    agent: "model".to_string(),
                },
            )
            .await;
        let _ = store
            .append_step(
                &tid,
                ThreadStep::ToolEnd {
                    tool_id: "t-save".to_string(),
                    name: "approve_and_save_artifact_batch".to_string(),
                    clean_name: "Save artifacts".to_string(),
                    args: serde_json::json!({}),
                    status: react_core::session::ToolStepStatus::Ok,
                    payload: None,
                    ctx: None,
                    observation: ToolObservation::normalize(serde_json::json!({"ok": true})),
                    ts: chrono::Utc::now().to_rfc3339(),
                    agent: "model".to_string(),
                },
            )
            .await;
        let _ = store
            .append_step(
                &tid,
                ThreadStep::ToolEnd {
                    tool_id: "t1".to_string(),
                    name: "dbt_validate".to_string(),
                    clean_name: "Validate DBT".to_string(),
                    args: serde_json::json!({"build": true}),
                    status: react_core::session::ToolStepStatus::Failed,
                    payload: None,
                    ctx: None,
                    observation: ToolObservation::normalize(
                        serde_json::json!({"ok": false, "compile_ok": false, "errors": ["fail"]}),
                    ),
                    ts: chrono::Utc::now().to_rfc3339(),
                    agent: "model".to_string(),
                },
            )
            .await;

        let ctx = react_core::agent::AgentCtxBuilder::new(
            Arc::new(react_core::llm::NullModel::new()),
            storage,
            scope,
            keyspace,
            Arc::new(react_core::agent::DefaultPolicy),
        )
        .top_k(1)
        .per_step_timeout_secs(1)
        .max_steps(1)
        .thread_id(tid.clone())
        .agent_name("model".to_string())
        .thread_store(store.clone())
        .build();

        let policy = SqlValidatedPolicy::default();
        let mut transcript: Vec<String> = Vec::new();
        let tools = ToolRegistry::new();
        let complete_env = CompleteEnvelope {
            kind: "ask".to_string(),
            payload: serde_json::json!({"answer":"x","sql":"SELECT 1 AS ok"}),
            display: None,
        };

        let out = policy
            .handle_complete(
                &tools,
                &ctx,
                &mut transcript,
                Some(&store),
                tid.as_str(),
                &complete_env,
            )
            .await
            .unwrap();
        assert!(matches!(out, CompleteDecision::Reject { .. }));
        assert!(
            transcript.iter().any(|l| l.contains("dbt_validate_failed")),
            "expected dbt_validate_failed in transcript; got: {transcript:?}"
        );
    }
}
