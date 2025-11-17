use crate::qa::agent::{Agent, AgentCtx};
use crate::qa::prompts::{system_prompt, tool_card};
use crate::qa::tools::{ToolRegistry};
use datafusion::prelude::SessionContext;
use crate::qa::tools::{sql_schema::SqlSchemaTool, sql_stats::SqlStatsTool};
use crate::qa::dbt;

pub async fn run(namespace: &str, pipeline: &str, prompt: &str) -> Result<(), String> {
    let ctx = SessionContext::new();
    // Ensure namespace context is registered for SQL validation
    let _ = crate::sql::tables::register_namespace_view(&ctx, pipeline, namespace).await;
    let mut registry = ToolRegistry::new();
    registry.register(SqlSchemaTool { ctx: ctx.clone() });
    registry.register(SqlStatsTool);
    let actx = AgentCtx {
        pipeline: pipeline.to_string(),
        namespace: Some(namespace.to_string()),
        top_k: 30,
        per_step_timeout_secs: 10,
        max_steps: 6,
        thread_id: None,
        progress_tx: None,
        pre_step_tx: None,
    };
    let sys = system_prompt();
    let tools = tool_card();
    // Ask for a single MetricFlow YAML suggestion (STRICT YAML in JSON string)
    let ask = format!("Modeling goal: {}.\nReturn STRICT JSON: {{\"metricflow_yaml\": \"<YAML snippet for one measure or dimension, spec-compliant>\"}}", prompt);
    let result = Agent::run(&registry, &actx, &sys, &tools, &ask).await?;
    let mut yaml_text = result.answer;
    if yaml_text.is_empty() {
        yaml_text = result.sql.unwrap_or_default();
    }
    if yaml_text.is_empty() {
        return Err("No MetricFlow YAML returned by agent".to_string());
    }
    // Validate basic YAML shape and attempt to execute any SQL fragments
    let parsed_yaml = match serde_yaml::from_str::<serde_yaml::Value>(&yaml_text) {
        Ok(v) => v,
        Err(e) => return Err(format!("Invalid YAML: {}", e)),
    };
    // Attempt to find SQL strings under keys commonly used (sql, expression)
    fn find_sql_fragments(v: &serde_yaml::Value, out: &mut Vec<String>) {
        match v {
            serde_yaml::Value::Mapping(map) => {
                for (k, val) in map {
                    if let serde_yaml::Value::String(key) = k {
                        let key_l = key.to_lowercase();
                        if key_l == "sql" || key_l.contains("expression") {
                            if let serde_yaml::Value::String(s) = val {
                                out.push(s.clone());
                            }
                        }
                    }
                    find_sql_fragments(val, out);
                }
            }
            serde_yaml::Value::Sequence(arr) => {
                for el in arr { find_sql_fragments(el, out); }
            }
            _ => {}
        }
    }
    let mut sqls: Vec<String> = Vec::new();
    find_sql_fragments(&parsed_yaml, &mut sqls);
    for s in sqls.iter() {
        // Only validate SELECT-like fragments; skip pure expressions
        let sl = s.trim().to_lowercase();
        if sl.starts_with("select ") {
            let limited = if sl.contains(" limit ") { s.clone() } else { format!("{} LIMIT 10", s) };
            if let Err(e) = ctx.sql(&limited).await {
                return Err(format!("Model SQL failed: {}", e));
            }
        }
    }
    println!("Proposed MetricFlow YAML:\n{}\nApprove? (y/N or paste edited YAML)", yaml_text);
    let mut user = String::new();
    let _ = std::io::stdin().read_line(&mut user);
    let edited = user.trim().to_string();
    let to_write = if edited.eq_ignore_ascii_case("y") || edited.eq_ignore_ascii_case("yes") {
        yaml_text
    } else if !edited.is_empty() {
        edited
    } else {
        return Ok(());
    };
    let name = format!("{}_metric_{}", namespace, chrono::Utc::now().format("%Y%m%d_%H%M%S"));
    let _key = dbt::write_metricflow_yaml(pipeline, namespace, &name, &to_write).await?;
    Ok(())
}


