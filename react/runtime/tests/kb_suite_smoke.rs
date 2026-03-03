use std::sync::Arc;

use async_trait::async_trait;
use react_core::agent::{
    Agent, AgentCtx, AgentPolicy, DefaultPolicy, FinalEnvelope, Interrupt, RunOutcome,
};
use react_core::keyspace::DefaultKeyspace;
use react_core::llm::{ChatMessage, LargeLanguageModel};
use react_core::scope::RequestScope;
use react_core::session::ThreadStore;
use react_core::storage::InMemoryStorageAdapter;
use react_core::tools::{Tool, ToolRegistry};
use serde_json::Value;

struct FixedJsonModel {
    out: String,
}

impl LargeLanguageModel for FixedJsonModel {
    fn chat(
        &self,
        _messages: &[ChatMessage],
        _options: &react_core::llm::LlmCallOptions,
    ) -> Result<String, String> {
        Ok(self.out.clone())
    }

    fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        Ok(vec![vec![0.0; 8]])
    }
}

struct AskUserTool;

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &'static str {
        "ask_user"
    }
    async fn call(
        &self,
        _args: Value,
        _ctx: &react_core::agent::AgentCtx,
    ) -> Result<Value, String> {
        Ok(serde_json::json!({"ok": true, "prompt": "hi"}))
    }
}

struct InterruptOnAskUser;

#[async_trait]
impl AgentPolicy for InterruptOnAskUser {
    fn interrupt_for_action(
        &self,
        action_name: &str,
        _args: &Value,
        obs: &Value,
    ) -> Option<Interrupt> {
        if action_name == "ask_user" {
            let prompt = obs
                .get("prompt")
                .and_then(|x| x.as_str())
                .unwrap_or("x")
                .to_string();
            return Some(Interrupt::AwaitUser { prompt });
        }
        None
    }

    async fn handle_final(
        &self,
        _tools: &ToolRegistry,
        _ctx: &react_core::agent::AgentCtx,
        _transcript: &mut Vec<String>,
        _store: Option<&ThreadStore>,
        _thread_id: &str,
        _final_env: &FinalEnvelope,
    ) -> Result<Option<RunOutcome>, String> {
        Ok(None)
    }
}

#[tokio::test]
async fn agent_default_policy_accepts_typed_final() {
    let llm = Arc::new(FixedJsonModel {
        out: r#"{"type":"final","name":null,"args":null,"final":{"kind":"kb","payload":"{\"answer\":\"hello\"}","display":null}} "#.to_string(),
    });
    let ctx = AgentCtx {
        top_k: 1,
        per_step_timeout_secs: 1,
        max_steps: 1,
        thread_id: None,
        progress_tx: None,
        pre_step_tx: None,
        trace_tx: None,
        agent_name: Some("test".to_string()),
        policy: Arc::new(DefaultPolicy),
        llm,
        storage: Arc::new(InMemoryStorageAdapter::default()),
        scope: RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        },
        keyspace: Arc::new(DefaultKeyspace::new("b".into())),
        query: None,
        warehouse: Arc::new(react_core::providers::NullWarehouseProvider::default()),
        dbt: None,
        vector: None,
        thread_store: None,
        exec_ctx: None,
        resolved_config: None,
    };
    let reg = ToolRegistry::new();
    let out = Agent::run_until_block(
        &reg,
        &ctx,
        "sys",
        "tools",
        "q",
        react_core::llm::LlmCallOptions {
            prompt_id: "react.tests.kb_suite_smoke.basic",
            thread_id: None,
            expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
            max_output_tokens: None,
            temperature: None,
            top_p: None,
            reasoning_effort: None,
        },
    )
    .await
    .expect("run");
    match out {
        RunOutcome::Final { result, .. } => {
            assert_eq!(result.kind.as_str(), "kb");
            assert_eq!(
                result.payload.get("answer").and_then(|x| x.as_str()),
                Some("hello")
            );
        }
        _ => panic!("expected final"),
    }
}

#[tokio::test]
async fn agent_does_not_special_case_ask_user_tool_name() {
    let llm = Arc::new(FixedJsonModel {
        out: r#"{"type":"tool","name":"ask_user","args":"{}","final":null} "#.to_string(),
    });
    let ctx = AgentCtx {
        top_k: 1,
        per_step_timeout_secs: 1,
        max_steps: 1,
        thread_id: None,
        progress_tx: None,
        pre_step_tx: None,
        trace_tx: None,
        agent_name: Some("test".to_string()),
        policy: Arc::new(DefaultPolicy),
        llm,
        storage: Arc::new(InMemoryStorageAdapter::default()),
        scope: RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        },
        keyspace: Arc::new(DefaultKeyspace::new("b".into())),
        query: None,
        warehouse: Arc::new(react_core::providers::NullWarehouseProvider::default()),
        dbt: None,
        vector: None,
        thread_store: None,
        exec_ctx: None,
        resolved_config: None,
    };
    let mut reg = ToolRegistry::new();
    reg.register(AskUserTool);
    let out = Agent::run_until_block(
        &reg,
        &ctx,
        "sys",
        "tools",
        "q",
        react_core::llm::LlmCallOptions {
            prompt_id: "react.tests.kb_suite_smoke.ask_user_name_not_special_cased",
            thread_id: None,
            expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
            max_output_tokens: None,
            temperature: None,
            top_p: None,
            reasoning_effort: None,
        },
    )
    .await
    .expect("run");
    match out {
        // OK: did not become AwaitUser automatically from tool name; it simply hit fallback.
        RunOutcome::AwaitUser { .. } => {}
        other => panic!(
            "expected AwaitUser fallback (no final produced), got {:?}",
            std::mem::discriminant(&other)
        ),
    }
}

#[tokio::test]
async fn agent_interrupts_only_when_policy_requests_it() {
    let llm = Arc::new(FixedJsonModel {
        out: r#"{"type":"tool","name":"ask_user","args":"{}","final":null} "#.to_string(),
    });
    let ctx = AgentCtx {
        top_k: 1,
        per_step_timeout_secs: 1,
        max_steps: 1,
        thread_id: None,
        progress_tx: None,
        pre_step_tx: None,
        trace_tx: None,
        agent_name: Some("test".to_string()),
        policy: Arc::new(InterruptOnAskUser),
        llm,
        storage: Arc::new(InMemoryStorageAdapter::default()),
        scope: RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        },
        keyspace: Arc::new(DefaultKeyspace::new("b".into())),
        query: None,
        warehouse: Arc::new(react_core::providers::NullWarehouseProvider::default()),
        dbt: None,
        vector: None,
        thread_store: None,
        exec_ctx: None,
        resolved_config: None,
    };
    let mut reg = ToolRegistry::new();
    reg.register(AskUserTool);
    let out = Agent::run_until_block(
        &reg,
        &ctx,
        "sys",
        "tools",
        "q",
        react_core::llm::LlmCallOptions {
            prompt_id: "react.tests.kb_suite_smoke.policy_interrupts_only_when_requested",
            thread_id: None,
            expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
            max_output_tokens: None,
            temperature: None,
            top_p: None,
            reasoning_effort: None,
        },
    )
    .await
    .expect("run");
    match out {
        RunOutcome::AwaitUser { prompt, .. } => assert_eq!(prompt, "hi"),
        _ => panic!("expected AwaitUser"),
    }
}

#[test]
fn default_registry_includes_kb_suite() {
    let reg = react_suites::default_registry();
    let ids = reg.list_ids();
    assert!(
        ids.contains(&"kb"),
        "expected 'kb' in suite registry, got {:?}",
        ids
    );
}
