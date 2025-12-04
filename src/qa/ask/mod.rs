use datafusion::prelude::SessionContext;
use crate::qa::agent::{Agent, AgentCtx};
use crate::qa::prompts::{system_prompt, tool_card};
use crate::qa::tools::{ToolRegistry};
use crate::qa::tools::{sql_run::SqlRunTool, sql_schema::SqlSchemaTool, sql_stats::SqlStatsTool, sql_sample::SqlSampleTool, vect_query::VectQueryTool};
use std::cmp::Ordering;
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub async fn run(question: &str, _pipeline: &str, _namespace: Option<&str>) -> Result<String, String> {
    use uuid::Uuid;
    let thread_id = Uuid::new_v4().to_string();
    let frames = crate::flows::ask::run(&thread_id, question).await?;
    for f in frames {
        match f {
            crate::flows::adapter::FlowFrame::Final { answer, .. } => return Ok(answer),
            crate::flows::adapter::FlowFrame::AwaitUser { prompt } => return Ok(prompt),
            crate::flows::adapter::FlowFrame::AwaitApproval { prompt } => return Ok(prompt),
            _ => {}
        }
    }
    Err("No output".to_string())
}


