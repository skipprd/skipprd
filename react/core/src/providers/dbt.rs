use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::scope::RequestScope;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DbtValidateArgs {
    pub project_name: String,
    pub profiles_dir: Option<String>,
    pub target: String,
    pub run: bool,
    pub build: bool,
    /// Optional dbt selection terms passed as repeated `--select` flags.
    /// When unset or empty, validation runs against the whole project.
    #[serde(default)]
    pub select: Option<Vec<String>>,
    /// Optional dbt exclusion terms passed as repeated `--exclude` flags.
    #[serde(default)]
    pub exclude: Option<Vec<String>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DbtValidateResult {
    pub ok: bool,
    pub deps_ok: bool,
    pub parse_ok: bool,
    pub compile_ok: bool,
    pub run_ok: Option<bool>,
    pub uploaded_target_files: usize,
    pub failure_class: DbtFailureClass,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub logs: serde_json::Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DbtFailureClass {
    NoFailure,
    WarehouseConfig,
    SqlOrRuntime,
    SchemaOrProject,
    MissingSource,
    Unknown,
}

impl Default for DbtFailureClass {
    fn default() -> Self {
        Self::NoFailure
    }
}

#[async_trait]
pub trait DbtProvider: Send + Sync {
    async fn ensure_minimal_project(&self, scope: &RequestScope) -> Result<(), String>;

    async fn write_model_sql(
        &self,
        scope: &RequestScope,
        rel_path: &str,
        sql: &str,
    ) -> Result<String, String>;

    async fn write_metricflow_yaml(
        &self,
        scope: &RequestScope,
        rel_path: &str,
        yaml_text: &str,
    ) -> Result<String, String>;

    async fn validate_project(
        &self,
        scope: &RequestScope,
        args: &DbtValidateArgs,
    ) -> Result<DbtValidateResult, String>;
}
