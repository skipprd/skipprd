use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use react_core::scope::RequestScope;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DbtValidateArgs {
    pub project_name: String,
    pub profiles_dir: Option<String>,
    pub target: String,
    pub run: bool,
    pub build: bool,
    #[serde(default)]
    pub select: Option<Vec<String>>,
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
    pub failure_class: crate::failure_kind::FailureKind,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub logs: serde_json::Value,
    /// LLM-authored content the sanitizer removed from system-shared files during this
    /// invocation. Surfaced to the next author/repair turn so the agent can decide whether to
    /// re-author the intent in a sanctioned location. See [`crate::file_ownership`] for the
    /// ownership rules that drive sanitization.
    #[serde(default)]
    pub stripped: Vec<crate::plan_types::StrippedArtifact>,
}

#[async_trait]
pub trait DbtProvider: Send + Sync {
    /// Initialize the scoped dbt project on disk/storage. Returns any content the sanitizer had
    /// to remove from `dbt_project.yml` so callers can persist the strips into the active plan's
    /// stripped-artifact buffer (see [`crate::plan_types::PlanSnapshot::push_stripped_artifact`]).
    async fn ensure_minimal_project(
        &self,
        scope: &RequestScope,
    ) -> Result<Vec<crate::plan_types::StrippedArtifact>, String>;

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
