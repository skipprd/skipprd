//! Canonical dbt project file paths used by the Data Engineer suite.
//!
//! Centralizing these avoids string drift across tools and repair logic.

/// `dbt_project.yml`
pub const DBT_PROJECT_YML: &str = "dbt_project.yml";

/// `packages.yml`
pub const PACKAGES_YML: &str = "packages.yml";

/// Centralized dbt sources and shared schema metadata.
pub const MODELS_SCHEMA_YML: &str = "models/schema.yml";

/// Core dbt project context files that are allowed/expected to be edited as part of repairs.
pub const CORE_PROJECT_CONTEXT_FILES: &[&str] = &[DBT_PROJECT_YML, PACKAGES_YML, MODELS_SCHEMA_YML];
