use react::suites::shared::tools::sql_run::SqlRunTool;
use react::tools::Tool;
use react::agent::AgentCtx;
use react::agent::DefaultPolicy;
use serde_json::json;
use std::sync::Arc;
use react::providers::{QueryProvider, QueryResult};
use react::adapters::storage::InMemoryStorageAdapter;
use react::llm::NullModel;

#[derive(Clone)]
struct DummyQueryProvider;

#[async_trait::async_trait]
impl QueryProvider for DummyQueryProvider {
    async fn query(&self, sql: &str) -> Result<QueryResult, String> {
        // Minimal “smoke” execution: just return header consistent with the test SQL.
        // We validate the tool’s plumbing (limit behavior + provider delegation), not engine semantics here.
        let _ = sql;
        Ok(QueryResult {
            header: vec!["a".to_string(), "rn".to_string()],
            rows: vec![vec!["1".to_string(), "1".to_string()]],
            meta: None,
        })
    }
    async fn schema(&self, _dataset_fqn: &str) -> Result<Vec<(String, String)>, String> {
        Ok(vec![])
    }
    async fn sample(&self, _dataset_fqn: &str, _limit: usize) -> Result<Vec<Vec<String>>, String> {
        Ok(vec![])
    }
}

#[tokio::test]
async fn run_sql_allows_cte_and_window() {
	let tool = SqlRunTool { query: Arc::new(DummyQueryProvider) };
	let sql = r#"
WITH x AS (SELECT 1 AS a)
SELECT a, ROW_NUMBER() OVER () AS rn
FROM x
LIMIT 1
"#;
	let args = json!({ "sql": sql });
	let actx = AgentCtx {
		top_k: 10,
		per_step_timeout_secs: 5,
		max_steps: 1,
		thread_id: None,
		progress_tx: None,
		pre_step_tx: None,
		trace_tx: None,
		agent_name: Some("test".to_string()),
		policy: std::sync::Arc::new(DefaultPolicy),
		llm: std::sync::Arc::new(NullModel::new()),
		storage: std::sync::Arc::new(InMemoryStorageAdapter::default()),
		scope: react::providers::RequestScope { tenant: "t".into(), workspace: "w".into(), project_id: "p".into() },
		keyspace: std::sync::Arc::new(react::providers::DefaultKeyspace::new("b".into())),
		query: None,
		dbt: None,
		vector: None,
		thread_store: None,
		resolved_config: None,
	};
	let res = tool.call(args, &actx).await.expect("tool call");
	assert!(res.get("ok").and_then(|x| x.as_bool()).unwrap_or(false), "expected ok response, got {}", res);
	let header = res.get("header").and_then(|x| x.as_array()).cloned().unwrap_or_default();
	let cols: Vec<String> = header.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect();
	assert!(cols.contains(&"a".to_string()), "expected column 'a' in header, got {:?}", cols);
	assert!(cols.contains(&"rn".to_string()), "expected column 'rn' in header, got {:?}", cols);
}



