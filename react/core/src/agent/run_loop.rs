use std::sync::Arc;

use tracing::info;

use crate::llm::{ChatMessage, LlmCallOptions, LlmExpectedFormat};
use crate::llm_observability::PartInput;
use crate::schema_registry::SchemaId;
use crate::session::{LlmStepStatus, Observation, ThreadStep, ToolObservation, ToolStepStatus};
use crate::tools::ToolRegistry;

use super::helpers::clean_tool_name;
use super::{
    Agent, AgentCtx, Interrupt, NonInteractivePolicyAdapter, ParsedStep, RunOutcome,
    RunOutcomeNonInteractive, StepBoundaryReason,
};

impl Agent {
    pub(crate) async fn llm_chat_once(
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
                            LlmStepStatus::Ok
                        } else {
                            LlmStepStatus::Failed
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

    pub async fn run_until_block_non_interactive(
        tools: &ToolRegistry,
        ctx: &AgentCtx,
        system_prompt: &str,
        tools_card: &str,
        question: &str,
        llm_options: LlmCallOptions,
    ) -> Result<RunOutcomeNonInteractive, String> {
        let mut non_interactive_ctx = ctx.clone();
        non_interactive_ctx.policy = Arc::new(NonInteractivePolicyAdapter {
            inner: ctx.policy.clone(),
        });
        match Self::run_until_block(
            tools,
            &non_interactive_ctx,
            system_prompt,
            tools_card,
            question,
            llm_options,
        )
        .await?
        {
            RunOutcome::Final { thread_id, result } => {
                Ok(RunOutcomeNonInteractive::Final { thread_id, result })
            }
            // Non-interactive single-step runs suppress tool interrupts; remaining AwaitUser
            // outcomes represent deterministic step boundaries (budget exhaustion).
            RunOutcome::AwaitUser {
                thread_id,
                prompt: _,
            } => Ok(RunOutcomeNonInteractive::StepBoundary {
                thread_id,
                reason: StepBoundaryReason::StepBudgetExhausted,
            }),
            RunOutcome::AwaitApproval { prompt, .. } => Err(format!(
                "non_interactive_contract_violation: received AwaitApproval outcome in non-interactive mode: {}",
                prompt
            )),
        }
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

        let output_contract_line = format!(
            "System: OUTPUT_CONTRACT schema_id={}. Return exactly one JSON object matching this schema_id.",
            SchemaId::AgentStepV1.name()
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
            let prompt = Self::prompt_from_transcript(ctx, &mut transcript, &output_contract_line);
            let mut raw = Self::llm_chat_once(ctx, prompt, llm_options.clone()).await?;
            // If the provider returned a deterministic error payload as "text", do not enter the
            // invalid-JSON repair ladder (it only wastes tokens and repeats the same failure).
            if raw.trim_start().starts_with("LLM_ERROR:") {
                return Err(raw.trim().to_string());
            }
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
                    keep.extend(
                        transcript
                            .iter()
                            .skip(transcript.len().saturating_sub(tail_n))
                            .cloned(),
                    );
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
                            let retry_prompt2 =
                                format!("{}\n{}", keep2.join("\n"), output_contract_line);
                            raw = Self::llm_chat_once(ctx, retry_prompt2, llm_options.clone())
                                .await?;
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
                            status: ToolStepStatus::Running,
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
                    ToolStepStatus::Ok
                } else {
                    ToolStepStatus::Failed
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
                        });
                    }
                    Interrupt::AwaitApproval { prompt } => {
                        return Ok(RunOutcome::AwaitApproval {
                            thread_id: tid,
                            prompt,
                        });
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
                let rendered =
                    crate::error_context::render_failure_context(&obs_env_for_transcript, remaining);
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
