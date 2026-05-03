use std::sync::Arc;

use react_core::llm::{ChatMessage, ChatRole, LargeLanguageModel, LlmCallOptions};
use react_suite_data_engineer::providers::{QueryProvider, QueryResult};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatsSqlRepairRequest {
    pub provider: String,
    pub dialect: String,
    pub dataset_id: String,
    pub field_name: String,
    pub field_type: String,
    pub expected_shape: Vec<String>,
    pub sql: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatsSqlRepairFailure {
    pub attempts: usize,
    pub last_sql: String,
    pub last_error: String,
}

pub async fn execute_stats_sql_with_repair(
    query: &dyn QueryProvider,
    llm: Arc<dyn LargeLanguageModel>,
    request: StatsSqlRepairRequest,
    max_repair_attempts: usize,
    llm_timeout_secs: u64,
) -> Result<QueryResult, StatsSqlRepairFailure> {
    let mut sql = request.sql.clone();
    let mut last_error = String::new();

    for attempt in 0..=max_repair_attempts {
        match query.query(&sql).await {
            Ok(result) => return Ok(result),
            Err(error) => {
                last_error = error;
                if attempt == max_repair_attempts {
                    break;
                }
                let Some(repaired_sql) =
                    repair_stats_sql(llm.clone(), &request, &sql, &last_error, llm_timeout_secs)
                        .await
                else {
                    break;
                };
                sql = repaired_sql;
            }
        }
    }

    Err(StatsSqlRepairFailure {
        attempts: max_repair_attempts + 1,
        last_sql: sql,
        last_error,
    })
}

async fn repair_stats_sql(
    llm: Arc<dyn LargeLanguageModel>,
    request: &StatsSqlRepairRequest,
    failed_sql: &str,
    warehouse_error: &str,
    llm_timeout_secs: u64,
) -> Option<String> {
    let prompt = format!(
        "Repair this generated catalog statistics SQL for the {provider} provider.\n\
         Dialect: {dialect}\n\
         Dataset: {dataset}\n\
         Field: {field}\n\
         Field type: {field_type}\n\
         Required result columns, in order: {shape}\n\
         Warehouse error:\n{warehouse_error}\n\n\
         SQL:\n{failed_sql}\n\n\
         Return JSON only: {{\"sql\":\"...\"}}. The repaired SQL must return the same stats shape. \
         Do not invent statistics or semantic evidence.",
        provider = request.provider,
        dialect = request.dialect,
        dataset = request.dataset_id,
        field = request.field_name,
        field_type = request.field_type,
        shape = request.expected_shape.join(", "),
    );
    let opts = LlmCallOptions {
        prompt_id: "react.catalog.stats.sql.repair",
        expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
        ..Default::default()
    };
    let call = tokio::task::spawn_blocking(move || {
        llm.chat(
            &[ChatMessage {
                role: ChatRole::User,
                content: prompt,
            }],
            &opts,
        )
    });
    let text = if llm_timeout_secs == 0 {
        call.await.ok().and_then(Result::ok)?
    } else {
        tokio::time::timeout(std::time::Duration::from_secs(llm_timeout_secs), call)
            .await
            .ok()
            .and_then(Result::ok)
            .and_then(Result::ok)?
    };
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value
        .get("sql")
        .and_then(|sql| sql.as_str())
        .map(str::trim)
        .filter(|sql| !sql.is_empty())
        .map(ToString::to_string)
}
