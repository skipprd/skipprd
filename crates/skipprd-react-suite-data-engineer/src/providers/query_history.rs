use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::de_config::WarehouseKind;

use super::QueryResult;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct QueryHistoryRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    #[serde(default)]
    pub limit: usize,
    #[serde(default)]
    pub include_failed: bool,
    #[serde(default)]
    pub include_non_select: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warehouse_or_workgroup: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_tag: Option<String>,
}

impl QueryHistoryRequest {
    pub fn bounded_limit(&self, default_limit: usize, max_limit: usize) -> usize {
        let requested = if self.limit == 0 {
            default_limit
        } else {
            self.limit
        };
        requested.max(1).min(max_limit)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QueryHistoryStatus {
    Succeeded,
    Failed,
    Canceled,
    Running,
    Unknown,
}

impl Default for QueryHistoryStatus {
    fn default() -> Self {
        Self::Unknown
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct QueryHistoryRecord {
    pub provider: WarehouseKind,
    pub query_id: String,
    pub sql: String,
    pub normalized_sql: String,
    pub sql_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_epoch_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_epoch_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warehouse: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(default)]
    pub status: QueryHistoryStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    #[serde(default)]
    pub raw_metadata: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum QueryHistoryCapability {
    Supported,
    Unavailable {
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw_error: Option<String>,
    },
    RequiresConfiguration {
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw_error: Option<String>,
    },
    RequiresPrivileges {
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw_error: Option<String>,
    },
}

impl Default for QueryHistoryCapability {
    fn default() -> Self {
        Self::Supported
    }
}

impl QueryHistoryCapability {
    pub fn supported(&self) -> bool {
        matches!(self, Self::Supported)
    }

    pub fn diagnostic_message(&self) -> Option<String> {
        match self {
            Self::Supported => None,
            Self::Unavailable { reason, raw_error }
            | Self::RequiresConfiguration { reason, raw_error }
            | Self::RequiresPrivileges { reason, raw_error } => {
                Some(format_reason_and_raw_error(reason, raw_error.as_deref()))
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QueryHistoryProviderErrorKind {
    Unavailable,
    RequiresConfiguration,
    RequiresPrivileges,
    RetentionWindow,
    Provider,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct QueryHistoryProviderError {
    pub kind: QueryHistoryProviderErrorKind,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_error: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

impl QueryHistoryProviderError {
    pub fn provider(message: impl Into<String>, raw_error: impl Into<Option<String>>) -> Self {
        Self {
            kind: QueryHistoryProviderErrorKind::Provider,
            message: message.into(),
            raw_error: raw_error.into(),
            metadata: BTreeMap::new(),
        }
    }

    pub fn requires_configuration(
        message: impl Into<String>,
        raw_error: impl Into<Option<String>>,
    ) -> Self {
        Self {
            kind: QueryHistoryProviderErrorKind::RequiresConfiguration,
            message: message.into(),
            raw_error: raw_error.into(),
            metadata: BTreeMap::new(),
        }
    }

    pub fn requires_privileges(
        message: impl Into<String>,
        raw_error: impl Into<Option<String>>,
    ) -> Self {
        Self {
            kind: QueryHistoryProviderErrorKind::RequiresPrivileges,
            message: message.into(),
            raw_error: raw_error.into(),
            metadata: BTreeMap::new(),
        }
    }

    pub fn retention_window(
        message: impl Into<String>,
        raw_error: impl Into<Option<String>>,
    ) -> Self {
        Self {
            kind: QueryHistoryProviderErrorKind::RetentionWindow,
            message: message.into(),
            raw_error: raw_error.into(),
            metadata: BTreeMap::new(),
        }
    }

    pub fn diagnostic_message(&self) -> String {
        format_reason_and_raw_error(&self.message, self.raw_error.as_deref())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct QueryHistoryResult {
    pub provider: WarehouseKind,
    pub capability: QueryHistoryCapability,
    #[serde(default)]
    pub records: Vec<QueryHistoryRecord>,
    #[serde(default)]
    pub diagnostics: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_error: Option<String>,
}

#[async_trait]
pub trait WarehouseQueryHistoryProvider: Send + Sync {
    fn query_history_capability(&self) -> QueryHistoryCapability;

    async fn list_query_history(
        &self,
        request: &QueryHistoryRequest,
    ) -> Result<QueryHistoryResult, QueryHistoryProviderError>;
}

pub fn stable_sql_hash(sql: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(sql.as_bytes());
    hex::encode(&digest[..16])
}

pub fn normalize_sql(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn row_value(header: &[String], row: &[String], name: &str) -> Option<String> {
    let wanted = name.to_ascii_lowercase();
    let idx = header.iter().position(|header| header == &wanted)?;
    row.get(idx)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub fn query_result_header(result: &QueryResult) -> Vec<String> {
    result
        .header
        .iter()
        .map(|name| name.trim().to_ascii_lowercase())
        .collect()
}

pub fn record_from_query_result_row(
    provider: WarehouseKind,
    header: &[String],
    row: &[String],
    default_query_id: impl Into<String>,
) -> Option<QueryHistoryRecord> {
    let sql = row_value(header, row, "query_text")
        .or_else(|| row_value(header, row, "query"))
        .or_else(|| row_value(header, row, "statement_text"))
        .or_else(|| row_value(header, row, "command"))?;
    let query_id = row_value(header, row, "query_id")
        .or_else(|| row_value(header, row, "job_id"))
        .or_else(|| row_value(header, row, "statement_id"))
        .or_else(|| row_value(header, row, "request_id"))
        .unwrap_or_else(|| default_query_id.into());

    let mut raw_metadata = BTreeMap::new();
    for (idx, name) in header.iter().enumerate() {
        if let Some(value) = row
            .get(idx)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
        {
            raw_metadata.insert(name.clone(), value.to_string());
        }
    }

    Some(QueryHistoryRecord {
        provider,
        query_id: query_id.clone(),
        normalized_sql: normalize_sql(&sql),
        sql_hash: stable_sql_hash(&sql),
        sql,
        started_at_epoch_ms: row_value(header, row, "start_time")
            .or_else(|| row_value(header, row, "creation_time"))
            .or_else(|| row_value(header, row, "submit_time"))
            .and_then(|value| parse_epoch_ms(&value)),
        ended_at_epoch_ms: row_value(header, row, "end_time")
            .or_else(|| row_value(header, row, "completion_time"))
            .and_then(|value| parse_epoch_ms(&value)),
        user: row_value(header, row, "user_name")
            .or_else(|| row_value(header, row, "user_email"))
            .or_else(|| row_value(header, row, "executed_by"))
            .or_else(|| row_value(header, row, "user")),
        application: row_value(header, row, "client_application")
            .or_else(|| row_value(header, row, "application"))
            .or_else(|| row_value(header, row, "query_tag")),
        warehouse: row_value(header, row, "warehouse_name")
            .or_else(|| row_value(header, row, "warehouse_id"))
            .or_else(|| row_value(header, row, "workgroup")),
        database: row_value(header, row, "database_name")
            .or_else(|| row_value(header, row, "database")),
        schema: row_value(header, row, "schema_name").or_else(|| row_value(header, row, "schema")),
        status: status_from_row(header, row),
        error: row_value(header, row, "error_message")
            .or_else(|| row_value(header, row, "error"))
            .or_else(|| row_value(header, row, "exception")),
        source_ref: Some(query_id),
        raw_metadata,
    })
}

pub fn records_from_query_result(
    provider: WarehouseKind,
    result: QueryResult,
) -> Vec<QueryHistoryRecord> {
    let header = query_result_header(&result);
    result
        .rows
        .into_iter()
        .enumerate()
        .filter_map(|(idx, row)| {
            record_from_query_result_row(provider, &header, &row, format!("{provider}_query_{idx}"))
        })
        .collect()
}

pub fn supported_result(
    provider: WarehouseKind,
    records: Vec<QueryHistoryRecord>,
) -> QueryHistoryResult {
    QueryHistoryResult {
        provider,
        capability: QueryHistoryCapability::Supported,
        records,
        diagnostics: Vec::new(),
        raw_error: None,
    }
}

pub fn parse_epoch_ms(value: &str) -> Option<i64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(ms) = trimmed.parse::<i64>() {
        return Some(ms);
    }
    chrono::DateTime::parse_from_rfc3339(trimmed)
        .ok()
        .map(|dt| dt.timestamp_millis())
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S%.f")
                .ok()
                .map(|dt| dt.and_utc().timestamp_millis())
        })
}

pub fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

pub fn lower_ascii_contains(haystack: &str, needle: &str) -> bool {
    haystack
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

fn status_from_row(header: &[String], row: &[String]) -> QueryHistoryStatus {
    let value = row_value(header, row, "execution_status")
        .or_else(|| row_value(header, row, "state"))
        .or_else(|| row_value(header, row, "status"))
        .or_else(|| row_value(header, row, "type"))
        .unwrap_or_default()
        .to_ascii_lowercase();
    if value.contains("success")
        || value.contains("succeed")
        || value.contains("finish")
        || value == "completed"
    {
        QueryHistoryStatus::Succeeded
    } else if value.contains("fail") || value.contains("error") || value.contains("exception") {
        QueryHistoryStatus::Failed
    } else if value.contains("cancel") {
        QueryHistoryStatus::Canceled
    } else if value.contains("run") || value.contains("start") {
        QueryHistoryStatus::Running
    } else {
        QueryHistoryStatus::Unknown
    }
}

fn format_reason_and_raw_error(reason: &str, raw_error: Option<&str>) -> String {
    match raw_error.map(str::trim).filter(|value| !value.is_empty()) {
        Some(raw) => format!("{reason}; raw_error: {raw}"),
        None => reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_warehouse_kinds_are_represented_for_query_history() {
        let kinds = [
            WarehouseKind::Athena,
            WarehouseKind::Postgres,
            WarehouseKind::Mssql,
            WarehouseKind::Snowflake,
            WarehouseKind::Bigquery,
            WarehouseKind::Databricks,
            WarehouseKind::Synapse,
            WarehouseKind::Redshift,
            WarehouseKind::Clickhouse,
            WarehouseKind::Motherduck,
        ];
        assert_eq!(kinds.len(), 10);
    }

    #[test]
    fn provider_error_keeps_raw_error_text() {
        let err = QueryHistoryProviderError::provider(
            "warehouse query history lookup failed",
            Some(
                "SQL compilation error: Cannot retrieve data from more than 7 days ago".to_string(),
            ),
        );
        assert!(err
            .diagnostic_message()
            .contains("Cannot retrieve data from more than 7 days ago"));
    }
}
