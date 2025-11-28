use skippr::qa::tools::sql_run::SqlRunTool;
use skippr::qa::agent::AgentCtx;
use serde_json::json;
use datafusion::prelude::SessionContext;

#[tokio::test]
async fn run_sql_allows_cte_and_window() {
	let ctx = SessionContext::new();
	let tool = SqlRunTool { ctx };
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
		agent_name: Some("test".to_string()),
		dataset_candidates: vec![],
	};
	let res = tool.call(args, &actx).await.expect("tool call");
	assert!(res.get("ok").and_then(|x| x.as_bool()).unwrap_or(false), "expected ok response, got {}", res);
	let header = res.get("header").and_then(|x| x.as_array()).cloned().unwrap_or_default();
	let cols: Vec<String> = header.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect();
	assert!(cols.contains(&"a".to_string()), "expected column 'a' in header, got {:?}", cols);
	assert!(cols.contains(&"rn".to_string()), "expected column 'rn' in header, got {:?}", cols);
}



