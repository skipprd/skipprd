use std::sync::Arc;

use react::agent::{Agent, AgentCtx, DefaultPolicy, RunOutcome};
use react::adapters::storage::InMemoryStorageAdapter;
use react::llm::{ChatMessage, LargeLanguageModel};
use react::tools::ToolRegistry;

struct FixedJsonModel {
    out: String,
}

impl LargeLanguageModel for FixedJsonModel {
    fn chat(&self, _messages: &[ChatMessage]) -> Result<String, String> {
        Ok(self.out.clone())
    }

    fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        Ok(vec![vec![0.0; 8]])
    }
}

#[tokio::test]
async fn agent_default_policy_accepts_final_without_sql() {
    let llm = Arc::new(FixedJsonModel {
        out: r#"{"final":{"answer":"hello"}} "#.to_string(),
    });
    let ctx = AgentCtx {
        top_k: 1,
        per_step_timeout_secs: 1,
        max_steps: 1,
        thread_id: None,
        progress_tx: None,
        pre_step_tx: None,
        agent_name: Some("test".to_string()),
        policy: Arc::new(DefaultPolicy),
        llm,
        storage: Arc::new(InMemoryStorageAdapter::default()),
        scope: react::providers::RequestScope { tenant: "t".into(), workspace: "w".into(), project_id: "p".into() },
        keyspace: Arc::new(react::providers::DefaultKeyspace::new("b".into())),
        dbt: None,
        vector: None,
        thread_store: None,
        dataset_candidates: vec![],
    };
    let reg = ToolRegistry::new();
    let out = Agent::run_until_block(&reg, &ctx, "sys", "tools", "q").await.expect("run");
    match out {
        RunOutcome::Final { result, .. } => {
            assert_eq!(result.answer, "hello");
            assert!(result.sql.is_none());
        }
        _ => panic!("expected final"),
    }
}

#[test]
fn default_registry_includes_kb_suite() {
    let reg = react::suites::registry::default_registry();
    let ids = reg.list_ids();
    assert!(ids.contains(&"kb"), "expected 'kb' in suite registry, got {:?}", ids);
}

