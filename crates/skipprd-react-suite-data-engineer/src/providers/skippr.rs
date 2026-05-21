use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use react_core::scope::RequestScope;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SkipprPipelineConfig {
    pub pipeline_name: String,
    pub skippr_input: serde_json::Value,
    pub output_plugin: SkipprOutputConfig,
    #[serde(default)]
    pub schema_sink: Option<SchemaSinkResolvedConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SchemaSinkResolvedConfig {
    pub kind: String,
    pub glue_database_name: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SkipprOutputConfig {
    pub kind: String,
    pub account: Option<String>,
    pub database: Option<String>,
    pub schema: Option<String>,
    pub warehouse: Option<String>,
    pub role: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SkipprDiscoverResult {
    pub ok: bool,
    pub namespaces_count: usize,
    pub total_fields: u64,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SkipprFieldSchema {
    pub name: String,
    #[serde(rename = "type")]
    pub field_type: String,
    pub nullable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_field_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_field_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field_id: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lineage_id: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SkipprPipelineStatus {
    pub pipeline: String,
    pub status: String,
    pub namespaces: Vec<SkipprNamespaceStatus>,
    pub metadata_location: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SkipprNamespaceStatus {
    pub namespace: String,
    pub fields: Vec<SkipprFieldSchema>,
    pub offset: Option<serde_json::Value>,
    /// Whether this namespace is running in CDC mode.
    #[serde(default)]
    pub cdc_enabled: bool,
    /// Last committed LSN/position for CDC sources.
    #[serde(default)]
    pub last_checkpoint: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SkipprSyncResult {
    pub ok: bool,
    pub tables_synced: usize,
    #[serde(default)]
    pub total_rows: u64,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub rows_written: u64,
    pub errors: Vec<String>,
}

#[async_trait]
pub trait SkipprProvider: Send + Sync {
    async fn write_pipeline_config(
        &self,
        scope: &RequestScope,
        config: &SkipprPipelineConfig,
    ) -> Result<String, String>;

    async fn discover_pipeline(
        &self,
        scope: &RequestScope,
        pipeline: &str,
    ) -> Result<SkipprDiscoverResult, String>;

    async fn show_pipeline(
        &self,
        scope: &RequestScope,
        pipeline: &str,
    ) -> Result<SkipprPipelineStatus, String>;

    async fn load_schema(
        &self,
        scope: &RequestScope,
        pipeline: &str,
        schema_json_path: &str,
    ) -> Result<(), String>;

    async fn sync_pipeline(
        &self,
        scope: &RequestScope,
        pipeline: &str,
    ) -> Result<SkipprSyncResult, String>;
}
