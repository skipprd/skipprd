use crate::qa::agent::{Agent, AgentCtx};
use crate::qa::prompts_model::{model_system_prompt, model_tool_card};
use crate::qa::tools::{ToolRegistry};
use datafusion::prelude::SessionContext;
use crate::qa::tools::sql_run::SqlRunTool;
use crate::qa::tools::ask_approval::AskApprovalTool;
use crate::qa::tools::ask_user::AskUserTool;
use crate::qa::tools::vect_query::VectQueryTool;
use crate::qa::dbt;
use uuid::Uuid;
use crate::qa::session::ThreadResult;

pub async fn run(pipeline: &str, prompt: &str) -> Result<(), String> {
    // Create thread id first to reuse a thread-scoped context
    let thread_id0 = Uuid::new_v4().to_string();
    let ctx = crate::ws::agent_runner::get_or_create_thread_ctx(&thread_id0);
    // Greedy, checksum-aware register of all namespaces (once per process/thread)
    {
        let pipelines = crate::sql::registry::list_pipelines().await;
        for p in pipelines {
            let mut namespaces = crate::sql::registry::list_namespaces(&p).await;
            namespaces.sort();
            let mut pairs: Vec<(String, String)> = Vec::new();
            for ns in namespaces { pairs.push((p.clone(), ns)); }
            crate::ws::agent_runner::pre_register_selected_namespaces(&ctx, &pairs).await;
            let _ = crate::sql::tables::register_deadletters(&ctx, &p).await;
        }
    }
    let mut registry = ToolRegistry::new();
    registry.register(SqlRunTool { ctx: ctx.clone() });
    registry.register(AskUserTool);
    registry.register(AskApprovalTool);
    registry.register(VectQueryTool);
    registry.register(crate::qa::tools::dbt_examples::SearchDbtExamplesTool);
    registry.register(crate::qa::tools::dbt_validate::DbtValidateTool);
    registry.register(crate::qa::tools::sql_register::SqlRegisterTool);
    registry.register(crate::qa::tools::catalog_note::CatalogNoteTool);
    registry.register(crate::qa::tools::approve_save::ApproveAndSaveArtifactTool);
    registry.register(crate::qa::tools::artifacts::ArtifactsTool);
    let actx = AgentCtx {
        top_k: 30,
        per_step_timeout_secs: 10,
        max_steps: 6,
        thread_id: None,
        progress_tx: None,
        pre_step_tx: None,
        agent_name: Some("model".to_string()),
        dataset_candidates: Vec::new(),
    };
    let sys = model_system_prompt();
    let tools = model_tool_card();
    // Propose exactly one artifact at a time (model or MetricFlow) with a stable logical name.
    // Use ask_approval for approvals; on update, first show a diff; upon approval, call approve_and_save_artifact.
    let ask = format!(
        "Modeling goal: {}.\n\
         Work on ONE artifact at a time:\n\
         - Either a DBT model (SQL starting with SELECT ...) OR a MetricFlow YAML (one measure/dimension).\n\
         - Use a stable logical name `name` that will never change.\n\
         - Prefer existing MetricFlow or models if relevant; otherwise propose a new one.\n\
         Procedure:\n\
         1) Propose the artifact and call ask_approval for approval (use ask_user for clarifications/edits).\n\
         2) For updates, call approve_and_save_artifact with preview_diff=true to produce a unified diff; show it via ask_approval for approval.\n\
         3) On approval, call approve_and_save_artifact with {{kind, name, content}} to save.\n\
         Return a compact summary only in final.",
        prompt
    );
    // Interactive loop: mirror qa/ask behavior (AwaitUser / Final)
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
                        agent: Some("model".to_string()),
                    }).await;
                }
                // continue
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
                        agent: Some("model".to_string()),
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
    // Saving is performed by the approve_and_save_artifact tool after user approval.
    Ok(())
}


