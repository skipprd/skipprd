use serde_json::Value;
use std::sync::Arc;

use crate::keyspace::Keyspace;
use crate::providers::{DbtProvider, QueryProvider, VectorStore, WarehouseProvider};
use crate::schema_registry::{AgentStepTypeV1, AgentStepV1, SchemaId};
use crate::scope::RequestScope;
use crate::session::{
    ExecutionContext, Observation, ThreadResult, ThreadStep, ThreadStore,
};
use crate::storage::StorageAdapter;
use crate::tools::ToolRegistry;
use async_trait::async_trait;

mod helpers;
mod parsing;
mod run_loop;

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

    /// Resolved runtime configuration (parsed YAML config).
    ///
    /// Injected by the runtime so suites/tools can access warehouse, dbt,
    /// and other provider configuration without coupling to parsing logic.
    pub resolved_config: Option<Arc<crate::resolved_config::ReactResolvedConfig>>,
}

pub struct Agent;

#[derive(Clone, Debug)]
pub(crate) enum ParsedStep {
    Tool { name: String, args: Value },
    Final { final_env: FinalEnvelope },
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

pub enum RunOutcomeNonInteractive {
    Final {
        thread_id: String,
        result: ThreadResult,
    },
    StepBoundary {
        thread_id: String,
        reason: StepBoundaryReason,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepBoundaryReason {
    StepBudgetExhausted,
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
            kind: crate::session::FinalKind::from(final_env.kind.clone()),
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

/// Adapter policy for strict non-interactive runs.
///
/// It preserves all policy behavior except interactive interrupts/fallbacks.
/// This lets suites reuse existing policies while hard-cutting `AwaitUser`/`AwaitApproval`.
struct NonInteractivePolicyAdapter {
    inner: Arc<dyn AgentPolicy>,
}

#[async_trait]
impl AgentPolicy for NonInteractivePolicyAdapter {
    fn prelude_lines(
        &self,
        ctx: &AgentCtx,
        store: Option<&ThreadStore>,
        thread_id: &str,
    ) -> Vec<String> {
        self.inner.prelude_lines(ctx, store, thread_id)
    }

    fn interrupt_for_action(
        &self,
        _action_name: &str,
        _args: &Value,
        _obs: &Value,
    ) -> Option<Interrupt> {
        // Hard cutover: non-interactive runs never emit interactive interrupts.
        None
    }

    fn timeout_for_tool(&self, action_name: &str) -> Option<u64> {
        self.inner.timeout_for_tool(action_name)
    }

    async fn handle_final(
        &self,
        tools: &ToolRegistry,
        ctx: &AgentCtx,
        transcript: &mut Vec<String>,
        store: Option<&ThreadStore>,
        thread_id: &str,
        final_env: &FinalEnvelope,
    ) -> Result<Option<RunOutcome>, String> {
        self.inner
            .handle_final(tools, ctx, transcript, store, thread_id, final_env)
            .await
    }

    async fn fallback(
        &self,
        tools: &ToolRegistry,
        ctx: &AgentCtx,
        transcript: &mut Vec<String>,
        store: Option<&ThreadStore>,
        thread_id: &str,
    ) -> Result<RunOutcome, String> {
        // Keep underlying fallback semantics; run_until_block_non_interactive maps
        // step-budget AwaitUser fallbacks to a typed StepBoundary handoff.
        self.inner
            .fallback(tools, ctx, transcript, store, thread_id)
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
            resolved_config: None,
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
                assert_eq!(result.kind.as_str(), "generic");
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
        let last_opts: Arc<Mutex<Option<crate::llm::LlmCallOptions>>> = Arc::new(Mutex::new(None));
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
            resolved_config: None,
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
        let got = last_opts.lock().ok().and_then(|g| g.clone()).expect("opts");
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
            resolved_config: None,
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
                assert_eq!(result.kind.as_str(), "generic");
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
                    final_env
                        .payload
                        .get("text")
                        .and_then(|x| x.as_str())
                        .unwrap(),
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
            resolved_config: None,
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

    #[tokio::test]
    async fn llm_error_text_does_not_trigger_invalid_json_retries() {
        let replies = Arc::new(Mutex::new(vec![
            // Provider-side failure surfaced as plain text (common in some router layers).
            "LLM_ERROR: You exceeded your current quota".to_string(),
            // If the agent incorrectly retries, it would consume this.
            "{\"type\":\"tool\",\"name\":\"noop\",\"args\":\"{}\",\"final\":null}".to_string(),
        ]));
        let llm = Arc::new(ScriptedModel {
            replies: replies.clone(),
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
            resolved_config: None,
        };

        let res = Agent::run_until_block(
            &reg,
            &ctx,
            "sys",
            "tools",
            "q",
            crate::llm::LlmCallOptions {
                prompt_id: "react_core.agent.tests.llm_error_no_retry",
                thread_id: None,
                expected_format: crate::llm::LlmExpectedFormat::JsonObject,
                max_output_tokens: None,
                temperature: None,
                top_p: None,
                reasoning_effort: None,
            },
        )
        .await;
        assert!(res.is_err(), "expected run to fail fast on LLM_ERROR text");
        let err = res.err().unwrap();

        assert!(
            err.contains("LLM_ERROR:"),
            "expected error to include LLM_ERROR text; got: {err}"
        );
        assert_eq!(
            replies.lock().unwrap().len(),
            1,
            "agent should not have retried after LLM_ERROR text"
        );
    }

    struct CapturingDbtFilesPatchTool {
        saw_patch: Arc<Mutex<bool>>,
    }

    #[async_trait]
    impl crate::tools::Tool for CapturingDbtFilesPatchTool {
        fn name(&self) -> &'static str {
            "file"
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
    async fn patch_protocol_response_is_wrapped_as_file_patch_action() {
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
                    "name": "file",
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
                plan_kind: Some(crate::session::ExecutionPlanKind::new("cleanse")),
                plan_key: Some("k".to_string()),
                workgroup_id: Some("wg".to_string()),
                task_id: Some("t".to_string()),
                checklist_item_id: Some("schema_contract".to_string()),
                data: std::collections::BTreeMap::from([
                    ("suite".to_string(), serde_json::json!("suite_x")),
                ]),
            }),
            resolved_config: None,
        };

        let out = Agent::run_until_block(
            &reg,
            &ctx,
            "sys",
            "tools",
            "q",
            crate::llm::LlmCallOptions {
                prompt_id: "react_core.agent.tests.file_patch_invoked",
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
        assert!(saw, "expected file patch tool to be invoked");

        match out {
            RunOutcome::Final { result, .. } => {
                assert_eq!(result.kind.as_str(), "generic");
            }
            _ => panic!("expected final outcome"),
        }
    }

    struct InterruptOnNoopPolicy;

    #[async_trait]
    impl AgentPolicy for InterruptOnNoopPolicy {
        fn interrupt_for_action(
            &self,
            action_name: &str,
            _args: &Value,
            _obs: &Value,
        ) -> Option<Interrupt> {
            if action_name == "noop" {
                return Some(Interrupt::AwaitUser {
                    prompt: "need input".to_string(),
                });
            }
            None
        }

        async fn handle_final(
            &self,
            tools: &ToolRegistry,
            ctx: &AgentCtx,
            transcript: &mut Vec<String>,
            store: Option<&ThreadStore>,
            thread_id: &str,
            final_env: &FinalEnvelope,
        ) -> Result<Option<RunOutcome>, String> {
            DefaultPolicy
                .handle_final(tools, ctx, transcript, store, thread_id, final_env)
                .await
        }
    }

    #[tokio::test]
    async fn run_until_block_non_interactive_suppresses_policy_interrupts() {
        let llm = Arc::new(ScriptedModel {
            replies: Arc::new(Mutex::new(vec![
                "{\"type\":\"tool\",\"name\":\"noop\",\"args\":\"{}\",\"final\":null}".to_string(),
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
            max_steps: 3,
            thread_id: Some("tid".to_string()),
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: None,
            agent_name: Some("test".to_string()),
            policy: Arc::new(InterruptOnNoopPolicy),
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
            resolved_config: None,
        };

        let out = Agent::run_until_block_non_interactive(
            &reg,
            &ctx,
            "sys",
            "tools",
            "q",
            crate::llm::LlmCallOptions {
                prompt_id: "react_core.agent.tests.non_interactive_interrupt_suppressed",
                thread_id: None,
                expected_format: crate::llm::LlmExpectedFormat::JsonObject,
                max_output_tokens: None,
                temperature: None,
                top_p: None,
                reasoning_effort: None,
            },
        )
        .await
        .expect("non-interactive run should finish without AwaitUser");

        match out {
            RunOutcomeNonInteractive::Final { result, .. } => {
                assert_eq!(result.kind.as_str(), "generic");
            }
            RunOutcomeNonInteractive::StepBoundary { .. } => {
                panic!("unexpected StepBoundary for interrupt-suppression test");
            }
        }
    }

    #[tokio::test]
    async fn run_until_block_non_interactive_returns_step_boundary_on_step_budget_exhaustion() {
        let llm = Arc::new(ScriptedModel {
            replies: Arc::new(Mutex::new(vec![
                "{\"type\":\"tool\",\"name\":\"noop\",\"args\":\"{}\",\"final\":null}".to_string(),
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
            max_steps: 1,
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
            resolved_config: None,
        };

        let out = Agent::run_until_block_non_interactive(
            &reg,
            &ctx,
            "sys",
            "tools",
            "q",
            crate::llm::LlmCallOptions {
                prompt_id: "react_core.agent.tests.non_interactive_budget_exhausted",
                thread_id: None,
                expected_format: crate::llm::LlmExpectedFormat::JsonObject,
                max_output_tokens: None,
                temperature: None,
                top_p: None,
                reasoning_effort: None,
            },
        )
        .await
        .expect("non-interactive runner should return StepBoundary on step cap");
        match out {
            RunOutcomeNonInteractive::StepBoundary { thread_id, reason } => {
                assert_eq!(thread_id, "tid".to_string());
                assert_eq!(reason, StepBoundaryReason::StepBudgetExhausted);
            }
            other => panic!("expected StepBoundary, got unexpected outcome: {:?}", std::mem::discriminant(&other)),
        }
    }
}
