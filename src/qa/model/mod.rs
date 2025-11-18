use crate::qa::agent::{Agent, AgentCtx};
use crate::qa::prompts::{system_prompt, tool_card};
use crate::qa::tools::{ToolRegistry};
use datafusion::prelude::SessionContext;
use crate::qa::tools::{sql_schema::SqlSchemaTool, sql_stats::SqlStatsTool};
use crate::qa::tools::sql_run::SqlRunTool;
use crate::qa::tools::ask_user::AskUserTool;
use crate::qa::tools::vect_query::VectQueryTool;
use crate::qa::dbt;
use uuid::Uuid;
use crate::qa::session::ThreadResult;

pub async fn run(pipeline: &str, prompt: &str) -> Result<(), String> {
    let ctx = SessionContext::new();
    // Auto-register all pipelines/namespaces for cross-namespace context
    {
        let pipelines = crate::sql::registry::list_pipelines().await;
        for p in pipelines {
            let mut namespaces = crate::sql::registry::list_namespaces(&p).await;
            namespaces.sort();
            for ns in namespaces {
                let _ = crate::sql::tables::register_namespace_view(&ctx, &p, &ns).await;
            }
            let _ = crate::sql::tables::register_deadletters(&ctx, &p).await;
        }
    }
    let mut registry = ToolRegistry::new();
    registry.register(SqlSchemaTool { ctx: ctx.clone() });
    registry.register(SqlStatsTool);
    registry.register(SqlRunTool { ctx: ctx.clone() });
    registry.register(AskUserTool);
    registry.register(VectQueryTool);
    let actx = AgentCtx {
        pipeline: pipeline.to_string(),
        namespace: None,
        top_k: 30,
        per_step_timeout_secs: 10,
        max_steps: 6,
        thread_id: None,
        progress_tx: None,
        pre_step_tx: None,
    };
    let sys = system_prompt();
    let tools = tool_card();
    // Ask for DBT model SQL (returned in final.sql) and MetricFlow YAML (returned in final.answer).
    // Require ask_user approval before finalizing and allow validation via run_sql.
    let ask = format!(
        "Modeling goal: {}.\n\
         Propose both:\n\
         - A DBT model SQL for a chosen dataset (SELECT ... with fully-qualified dataset <pipeline>.<namespace>)\n\
         - A MetricFlow YAML snippet (one measure or dimension, spec-compliant)\n\
         You MUST call ask_user to request approval or edits before returning final. Validate any SQL via run_sql before final.\n\
         Return STRICT JSON in final: {{\"sql\": \"<DBT model SQL>\", \"answer\": \"<MetricFlow YAML>\"}}.",
        prompt
    );
    // Interactive loop: mirror qa/ask behavior (AwaitUser / Final)
    let thread_id0 = Uuid::new_v4().to_string();
    let actx = AgentCtx { thread_id: Some(thread_id0.clone()), ..actx };
    let mut thread_id = thread_id0;
    let mut result: Option<ThreadResult> = None;
    loop {
        let mut actx2 = actx.clone();
        actx2.thread_id = Some(thread_id.clone());
        match Agent::run_until_block(&registry, &actx2, &sys, &tools, &ask).await.map_err(|e| e.to_string())? {
            crate::qa::agent::RunOutcome::Final { thread_id: tid, result: r } => {
                thread_id = tid;
                result = Some(r);
                break;
            }
            crate::qa::agent::RunOutcome::AwaitUser { thread_id: tid, prompt } => {
                thread_id = tid;
                println!("{}", prompt);
                let mut buf = String::new();
                let _ = std::io::stdin().read_line(&mut buf);
                let text = buf.trim().to_string();
                if !text.is_empty() {
                    let store = crate::qa::session::ThreadStore::new();
                    let _ = store.append_step(&thread_id, crate::qa::session::ThreadStep {
                        action: "user".to_string(),
                        args: serde_json::json!({ "text": text }),
                        observation: serde_json::json!({ "ok": true }),
                        ts: chrono::Utc::now().to_rfc3339(),
                    }).await;
                }
                // continue
            }
        }
    }
    let result = result.ok_or_else(|| "No result".to_string())?;
    // Extract outputs: DBT model SQL from result.sql; MetricFlow YAML from result.answer
    let model_sql = result.sql.unwrap_or_default();
    let mut yaml_text = result.answer;
    if yaml_text.is_empty() && !model_sql.is_empty() {
        yaml_text = String::new();
    }
    if yaml_text.is_empty() {
        return Err("No MetricFlow YAML returned by agent".to_string());
    }
    // Attempt to derive pipeline and namespace from model SQL
    fn derive_fqn(sql: &str) -> Option<(String, String)> {
        let pipes = futures::executor::block_on(crate::sql::registry::list_pipelines());
        for p in pipes {
            let nss = futures::executor::block_on(crate::sql::registry::list_namespaces(&p));
            for ns in nss {
                let fqn = format!("{}.{}", p, ns);
                if sql.contains(&fqn) {
                    return Some((p, ns));
                }
            }
        }
        None
    }
    let target = derive_fqn(&model_sql);
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
    // Also validate the DBT model SQL if provided (preview with LIMIT)
    if !model_sql.trim().is_empty() {
        let sl = model_sql.trim().to_lowercase();
        if sl.starts_with("select ") {
            let limited = if sl.contains(" limit ") { model_sql.clone() } else { format!("{} LIMIT 10", model_sql) };
            if let Err(e) = ctx.sql(&limited).await {
                return Err(format!("DBT model SQL failed: {}", e));
            }
        }
    }
    // Write artifacts (approval was collected via ask_user earlier)
    if !model_sql.trim().is_empty() {
        if let Some((p, ns)) = target.as_ref() {
            let name_sql = format!("{}_model_{}", ns, chrono::Utc::now().format("%Y%m%d_%H%M%S"));
            let _key_sql = dbt::write_model_sql(p, ns, &name_sql, &model_sql).await?;
        } else {
            println!("Warning: could not infer target dataset from model SQL; skipping DBT model write.");
        }
    }
    if let Some((p, ns)) = target {
        let name_yaml = format!("{}_metric_{}", ns, chrono::Utc::now().format("%Y%m%d_%H%M%S"));
        let _key_yaml = dbt::write_metricflow_yaml(&p, &ns, &name_yaml, &yaml_text).await?;
    } else {
        println!("Warning: could not infer target dataset from model SQL; skipping MetricFlow YAML write.");
    }
    Ok(())
}


