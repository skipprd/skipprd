use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use react_core::scope::RequestScope;

pub const DBT_SILVER_DATABASE_ENV: &str = "DBT_SILVER_DATABASE";
pub const DBT_SILVER_SCHEMA_ENV: &str = "DBT_SILVER_SCHEMA";
pub const DBT_GOLD_DATABASE_ENV: &str = "DBT_GOLD_DATABASE";
pub const DBT_GOLD_SCHEMA_ENV: &str = "DBT_GOLD_SCHEMA";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DbtTier {
    Silver,
    Gold,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DbtNamespaceShape {
    /// Warehouse supports dbt `database` + `schema` relation placement.
    DatabaseAndSchema,
    /// Warehouse uses a catalog/project/database as container plus a schema/dataset namespace.
    CatalogAndSchema,
    /// Warehouse connects to one database; dbt tier routing is schema-level.
    ConnectionDatabaseAndSchema,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DbtCustomSchemaPolicy {
    #[default]
    AdapterDefault,
    Exact,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DbtTierNamespace {
    pub database: Option<String>,
    pub schema: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DbtTierRouting {
    pub base_schema: String,
    pub silver: DbtTierNamespace,
    pub gold: DbtTierNamespace,
    pub shape: DbtNamespaceShape,
    #[serde(default)]
    pub custom_schema_policy: DbtCustomSchemaPolicy,
}

impl DbtTierRouting {
    pub fn namespace(&self, tier: DbtTier) -> &DbtTierNamespace {
        match tier {
            DbtTier::Silver => &self.silver,
            DbtTier::Gold => &self.gold,
        }
    }

    pub fn env_vars(&self) -> Vec<(&'static str, String)> {
        let mut vars = Vec::new();
        if let Some(database) = self
            .silver
            .database
            .as_ref()
            .filter(|v| !v.trim().is_empty())
        {
            vars.push((DBT_SILVER_DATABASE_ENV, database.clone()));
        }
        vars.push((DBT_SILVER_SCHEMA_ENV, self.silver.schema.clone()));
        if let Some(database) = self.gold.database.as_ref().filter(|v| !v.trim().is_empty()) {
            vars.push((DBT_GOLD_DATABASE_ENV, database.clone()));
        }
        vars.push((DBT_GOLD_SCHEMA_ENV, self.gold.schema.clone()));
        vars
    }

    pub fn relation_prefix(&self, tier: DbtTier, warehouse_container: &str) -> Option<String> {
        let (catalog, schema) = self.relation_catalog_schema(tier, warehouse_container)?;
        Some(format!("{}.{}", catalog, schema))
    }

    pub fn relation_catalog_schema(
        &self,
        tier: DbtTier,
        warehouse_container: &str,
    ) -> Option<(String, String)> {
        let ns = self.namespace(tier);
        match self.shape {
            DbtNamespaceShape::DatabaseAndSchema => ns
                .database
                .as_ref()
                .filter(|db| !db.trim().is_empty())
                .map(|db| (db.clone(), ns.schema.clone())),
            DbtNamespaceShape::CatalogAndSchema
            | DbtNamespaceShape::ConnectionDatabaseAndSchema => {
                let container = warehouse_container.trim();
                if container.is_empty() {
                    None
                } else {
                    Some((container.to_string(), ns.schema.clone()))
                }
            }
        }
    }
}

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
    /// Tier namespace routing required by the system-rendered dbt project.
    ///
    /// This is derived from resolved Skippr config, not authored by the LLM.
    #[serde(default)]
    pub tier_routing: Option<DbtTierRouting>,
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
