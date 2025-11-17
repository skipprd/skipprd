use crate::qa::agent::{Agent, AgentCtx};
use crate::qa::prompts::{system_prompt, tool_card};
use crate::qa::tools::{ToolRegistry};
use datafusion::prelude::SessionContext;
use crate::qa::tools::{sql_run::SqlRunTool, sql_schema::SqlSchemaTool, sql_stats::SqlStatsTool, sql_sample::SqlSampleTool};
use crate::qa::dbt;

pub async fn run(namespace: &str, pipeline: &str, prompt: &str) -> Result<(), String> {
    let ctx = SessionContext::new();
    // Ensure namespace is registered for validation and previews
    let _ = crate::sql::tables::register_namespace_view(&ctx, pipeline, namespace).await;
    let mut registry = ToolRegistry::new();
    registry.register(SqlRunTool { ctx: ctx.clone() });
    registry.register(SqlSchemaTool { ctx: ctx.clone() });
    registry.register(SqlStatsTool);
    registry.register(SqlSampleTool { ctx: ctx.clone() });
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
    // Ask agent for a single cleansing suggestion (STRICT JSON)
    let ask = format!("Cleansing goal: {}.\nReturn STRICT JSON: {{\"op\": \"dedupe|normalize_datetime|coalesce|outliers\", \"target\": \"<table.field>\", \"preview_sql\": \"SELECT ...\", \"rationale\":\"...\"}}", prompt);
    let result = Agent::run(&registry, &actx, &sys, &tools, &ask).await?;
    // Expect preview_sql in result.sql or in answer; parse from answer if needed
    let mut preview_sql = result.sql.unwrap_or_default();
    if preview_sql.is_empty() {
        // crude parse attempt
        preview_sql = result.answer;
    }
    if preview_sql.is_empty() {
        return Err("No preview_sql returned by agent".to_string());
    }
    // Validate preview SQL and show before/after samples
    let preview_sql_limited = if preview_sql.to_lowercase().contains(" limit ") { preview_sql.clone() } else { format!("{} LIMIT 20", preview_sql) };
    let baseline_sql = format!("SELECT * FROM {} LIMIT 20", namespace);
    println!("Validating proposed SQL...");
    let after_rows = match ctx.sql(&preview_sql_limited).await {
        Ok(df) => df.collect().await.unwrap_or_default(),
        Err(e) => return Err(format!("Proposed SQL failed to execute: {}", e)),
    };
    let before_rows = match ctx.sql(&baseline_sql).await {
        Ok(df) => df.collect().await.unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    println!("Before (sample):");
    if !before_rows.is_empty() {
        match datafusion::arrow::util::pretty::pretty_format_batches(&before_rows) {
            Ok(s) => println!("{}", s),
            Err(_) => println!("<formatting error>"),
        }
    } else {
        println!("<empty or unavailable>");
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
    println!("Proposed cleansing model SQL:\n{}\nApprove? (y/N or edit and enter new SQL)", preview_sql);
    let mut user = String::new();
    let _ = std::io::stdin().read_line(&mut user);
    let edited = user.trim();
    let to_write = if edited.eq_ignore_ascii_case("y") || edited.eq_ignore_ascii_case("yes") {
        preview_sql
    } else if !edited.is_empty() {
        edited.to_string()
    } else {
        return Ok(());
    };
    // Write DBT model under dbt/models/<namespace>/<name>.sql
    let name = format!("{}_cleanse_{}", namespace, chrono::Utc::now().format("%Y%m%d_%H%M%S"));
    let _key = dbt::write_model_sql(pipeline, namespace, &name, &to_write).await?;
    Ok(())
}


