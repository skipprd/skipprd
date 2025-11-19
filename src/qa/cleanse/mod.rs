use crate::qa::agent::{Agent, AgentCtx};
use crate::qa::prompts::{system_prompt, tool_card};
use crate::qa::tools::{ToolRegistry};
use datafusion::prelude::SessionContext;
use crate::qa::tools::{sql_run::SqlRunTool, sql_schema::SqlSchemaTool, sql_stats::SqlStatsTool, sql_sample::SqlSampleTool};
use crate::qa::dbt;
use crate::qa::tools::ask_user::AskUserTool;
use crate::qa::tools::vect_query::VectQueryTool;
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
            // Also register deadletters (best-effort)
            let _ = crate::sql::tables::register_deadletters(&ctx, &p).await;
        }
    }
    let mut registry = ToolRegistry::new();
    registry.register(SqlRunTool { ctx: ctx.clone() });
    registry.register(SqlSchemaTool { ctx: ctx.clone() });
    registry.register(SqlStatsTool);
    registry.register(SqlSampleTool { ctx: ctx.clone() });
    registry.register(AskUserTool);
    registry.register(VectQueryTool);
    let actx = AgentCtx {
        top_k: 30,
        per_step_timeout_secs: 10,
        max_steps: 6,
        thread_id: None,
        progress_tx: None,
        pre_step_tx: None,
        agent_name: Some("cleanse".to_string()),
        dataset_candidates: Vec::new(),
    };
    let sys = system_prompt();
    let tools = tool_card();
    // Ask the agent to propose cleansing candidates (cross-namespace), validate via run_sql; use ask_user for clarifications/edits if needed before final.
    let ask = format!(
        "Cleansing goal: {}.\n\
         Identify cleansing candidates across datasets including: coalesce by value/type, deduplicate, normalize_datetime, outliers.\n\
         For the chosen candidate, include fully-qualified target dataset(s) and field(s) (e.g., <pipeline>.<namespace>.<field>), a short rationale, and a preview SELECT SQL showing the transformation.\n\
         If needed, call ask_user to request clarifications or edits before returning final. Validate with run_sql and then return final.\n\
         Return STRICT JSON in final: {{\"sql\": \"SELECT ...\", \"answer\": \"short rationale\"}}.",
        prompt
    );

    // Interactive loop similar to qa/ask: handle AwaitUser interactions
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
                        agent: Some("cleanse".to_string()),
                    }).await;
                }
                // continue loop to let agent resume
            }
            crate::qa::agent::RunOutcome::AwaitApproval { thread_id: tid, prompt } => {
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
                        agent: Some("cleanse".to_string()),
                    }).await;
                }
                // continue loop to let agent resume
            }
        }
    }

    let result = result.ok_or_else(|| "No result".to_string())?;
    // Expect preview_sql in result.sql or in answer; parse from answer if needed
    let mut preview_sql = result.sql.unwrap_or_default();
    if preview_sql.is_empty() {
        preview_sql = result.answer;
    }
    if preview_sql.is_empty() {
        return Err("No preview_sql returned by agent".to_string());
    }
    // Attempt to derive pipeline and namespace from SQL by matching known datasets
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
    let target = derive_fqn(&preview_sql);
    // Validate preview SQL and show before/after samples
    let preview_sql_limited = if preview_sql.to_lowercase().contains(" limit ") { preview_sql.clone() } else { format!("{} LIMIT 20", preview_sql) };
    println!("Validating proposed SQL...");
    let after_rows = match ctx.sql(&preview_sql_limited).await {
        Ok(df) => df.collect().await.unwrap_or_default(),
        Err(e) => return Err(format!("Proposed SQL failed to execute: {}", e)),
    };
    if let Some((p, ns)) = target.as_ref() {
        let baseline_sql = format!("SELECT * FROM {}.{} LIMIT 20", p, ns);
        println!("Before (sample) from {}.{}:", p, ns);
        let before_rows = match ctx.sql(&baseline_sql).await {
            Ok(df) => df.collect().await.unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        if !before_rows.is_empty() {
            match datafusion::arrow::util::pretty::pretty_format_batches(&before_rows) {
                Ok(s) => println!("{}", s),
                Err(_) => println!("<formatting error>"),
            }
        } else {
            println!("<empty or unavailable>");
        }
    }
    println!("After (sample):");
    if !after_rows.is_empty() {
        match datafusion::arrow::util::pretty::pretty_format_batches(&after_rows) {
            Ok(s) => println!("{}", s),
            Err(_) => println!("<formatting error>"),
        }
    } else {
        println!("<no rows>");
    }
    // Write DBT model under dbt/models/<namespace>/<name>.sql (user already approved via ask_user)
    if let Some((p, ns)) = target {
        let name = format!("{}_cleanse_{}", ns, chrono::Utc::now().format("%Y%m%d_%H%M%S"));
        let _key = dbt::write_model_sql(&p, &ns, &name, &preview_sql).await?;
    } else {
        println!("Warning: could not infer target dataset from SQL; skipping DBT write.");
    }
    Ok(())
}


