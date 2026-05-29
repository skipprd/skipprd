use serde::{Deserialize, Serialize};
use std::fmt;

// ---------------------------------------------------------------------------
// YAML-file structs (deserialized from the `providers:` section of config)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct ProvidersFile {
    pub warehouse: Option<WarehouseFile>,
    pub catalog: Option<CatalogFile>,
    pub dbt: Option<DbtFile>,
    pub vector: Option<VectorFile>,
    pub el: Option<ElToolFile>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct ElToolFile {
    pub enabled: Option<bool>,
    pub skippr_binary: Option<String>,
    pub skippr_input: Option<serde_json::Value>,
    pub cdc: Option<CdcConfig>,
}

/// Warehouse configuration for a single provider (source or target).
///
/// Keep secrets in env; only non-secret wiring here.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum WarehouseFile {
    Athena {
        workgroup: Option<String>,
        region: Option<String>,
        result_s3: Option<String>,
        max_concurrency: Option<usize>,
        catalog: Option<String>,
        schema: Option<String>,
        discovery_cache_ttl_secs: Option<u64>,
    },
    Postgres {
        database: Option<String>,
        schema: Option<String>,
    },
    Mssql {
        database: Option<String>,
        schema: Option<String>,
        max_concurrency: Option<usize>,
        discovery_cache_ttl_secs: Option<u64>,
    },
    Snowflake {
        account: Option<String>,
        user: Option<String>,
        password: Option<String>,
        private_key_path: Option<String>,
        stage: Option<String>,
        staging_uri: Option<String>,
        staging_storage_integration: Option<String>,
        staging_azure_sas_token: Option<String>,
        staging_azure_account_key: Option<String>,
        staging_gcs_service_account_key_path: Option<String>,
        database: Option<String>,
        schema: Option<String>,
        warehouse: Option<String>,
        role: Option<String>,
        max_concurrency: Option<usize>,
        discovery_cache_ttl_secs: Option<u64>,
    },
    Bigquery {
        project: Option<String>,
        dataset: Option<String>,
        location: Option<String>,
        max_concurrency: Option<usize>,
        discovery_cache_ttl_secs: Option<u64>,
    },
    Databricks {
        workspace_url: Option<String>,
        token: Option<String>,
        warehouse_id: Option<String>,
        catalog: Option<String>,
        schema: Option<String>,
    },
    Synapse {
        connection_string: Option<String>,
        schema: Option<String>,
    },
    Redshift {
        database: Option<String>,
        cluster_identifier: Option<String>,
        workgroup_name: Option<String>,
        db_user: Option<String>,
        schema: Option<String>,
        region: Option<String>,
        staging_s3_bucket: Option<String>,
        staging_s3_prefix: Option<String>,
        iam_role_arn: Option<String>,
    },
    Clickhouse {
        url: Option<String>,
        database: Option<String>,
        user: Option<String>,
        password: Option<String>,
    },
    Motherduck {
        motherduck_token: Option<String>,
        database: Option<String>,
        schema: Option<String>,
    },
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct CatalogFile {
    pub enabled: Option<bool>,
    pub refresh_secs: Option<u64>,
    pub max_concurrency: Option<usize>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct DbtNamingFile {
    pub target_schema: Option<String>,
    pub silver_suffix: Option<String>,
    pub gold_suffix: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct DbtFile {
    pub enabled: Option<bool>,
    pub profiles_dir: Option<String>,
    pub target: Option<String>,
    pub naming: Option<DbtNamingFile>,
    pub runner: Option<String>,
    pub docker_image: Option<String>,
    pub docker_platform: Option<String>,
    pub docker_network: Option<String>,
    pub docker_mount_aws_dir: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct VectorFile {
    pub enabled: Option<bool>,
}

// ---------------------------------------------------------------------------
// resolve_providers_from_yaml – builds the normalised suite_config JSON
// ---------------------------------------------------------------------------

use super::env_util::{env_keys, getenv_nonempty, resolve_env_ref};

fn resolve_opt_env_ref(value: Option<String>) -> Option<String> {
    value.map(|v| resolve_env_ref(&v))
}

fn resolve_warehouse(w: WarehouseFile) -> WarehouseResolved {
    match w {
        WarehouseFile::Athena {
            workgroup,
            region,
            result_s3,
            max_concurrency,
            catalog,
            schema,
            discovery_cache_ttl_secs,
        } => WarehouseResolved {
            kind: WarehouseKind::Athena,
            container: resolve_env_ref(&catalog.unwrap_or_else(|| "AwsDataCatalog".to_string())),
            namespace: resolve_env_ref(&schema.unwrap_or_default()),
            extras: serde_json::json!({
                "workgroup": workgroup,
                "region": region,
                "result_s3": result_s3,
                "max_concurrency": max_concurrency,
                "discovery_cache_ttl_secs": discovery_cache_ttl_secs,
            }),
        },
        WarehouseFile::Postgres { database, schema } => WarehouseResolved {
            kind: WarehouseKind::Postgres,
            container: resolve_env_ref(&database.unwrap_or_default()),
            namespace: resolve_env_ref(
                &schema
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| "public".to_string()),
            ),
            extras: serde_json::json!({}),
        },
        WarehouseFile::Mssql {
            database,
            schema,
            max_concurrency,
            discovery_cache_ttl_secs,
        } => WarehouseResolved {
            kind: WarehouseKind::Mssql,
            container: resolve_env_ref(&database.unwrap_or_default()),
            namespace: resolve_env_ref(&schema.unwrap_or_default()),
            extras: serde_json::json!({
                "max_concurrency": max_concurrency,
                "discovery_cache_ttl_secs": discovery_cache_ttl_secs,
            }),
        },
        WarehouseFile::Snowflake {
            account,
            user,
            password,
            private_key_path,
            stage,
            staging_uri,
            staging_storage_integration,
            staging_azure_sas_token,
            staging_azure_account_key,
            staging_gcs_service_account_key_path,
            database,
            schema,
            warehouse,
            role,
            max_concurrency,
            discovery_cache_ttl_secs,
        } => WarehouseResolved {
            kind: WarehouseKind::Snowflake,
            container: resolve_env_ref(&database.unwrap_or_default()),
            namespace: resolve_env_ref(&schema.unwrap_or_default()),
            extras: serde_json::json!({
                "account": resolve_opt_env_ref(account),
                "user": resolve_opt_env_ref(user),
                "password": resolve_opt_env_ref(password),
                "private_key_path": resolve_opt_env_ref(private_key_path),
                "stage": resolve_opt_env_ref(stage),
                "staging_uri": resolve_opt_env_ref(staging_uri),
                "staging_storage_integration": resolve_opt_env_ref(staging_storage_integration),
                "staging_azure_sas_token": resolve_opt_env_ref(staging_azure_sas_token),
                "staging_azure_account_key": resolve_opt_env_ref(staging_azure_account_key),
                "staging_gcs_service_account_key_path": resolve_opt_env_ref(staging_gcs_service_account_key_path),
                "warehouse": resolve_opt_env_ref(warehouse),
                "role": resolve_opt_env_ref(role),
                "max_concurrency": max_concurrency,
                "discovery_cache_ttl_secs": discovery_cache_ttl_secs,
            }),
        },
        WarehouseFile::Bigquery {
            project,
            dataset,
            location,
            max_concurrency,
            discovery_cache_ttl_secs,
        } => WarehouseResolved {
            kind: WarehouseKind::Bigquery,
            container: resolve_env_ref(&project.unwrap_or_default()),
            namespace: resolve_env_ref(&dataset.unwrap_or_default()),
            extras: serde_json::json!({
                "location": location,
                "max_concurrency": max_concurrency,
                "discovery_cache_ttl_secs": discovery_cache_ttl_secs,
            }),
        },
        WarehouseFile::Databricks {
            workspace_url,
            token,
            warehouse_id,
            catalog,
            schema,
        } => WarehouseResolved {
            kind: WarehouseKind::Databricks,
            container: resolve_env_ref(&catalog.unwrap_or_else(|| "main".to_string())),
            namespace: resolve_env_ref(&schema.unwrap_or_else(|| "default".to_string())),
            extras: serde_json::json!({
                "workspace_url": workspace_url,
                "token": token,
                "warehouse_id": warehouse_id,
            }),
        },
        WarehouseFile::Synapse {
            connection_string,
            schema,
        } => WarehouseResolved {
            kind: WarehouseKind::Synapse,
            container: String::new(),
            namespace: resolve_env_ref(&schema.unwrap_or_else(|| "dbo".to_string())),
            extras: serde_json::json!({
                "connection_string": connection_string,
            }),
        },
        WarehouseFile::Redshift {
            database,
            cluster_identifier,
            workgroup_name,
            db_user,
            schema,
            region,
            staging_s3_bucket,
            staging_s3_prefix,
            iam_role_arn,
        } => WarehouseResolved {
            kind: WarehouseKind::Redshift,
            container: resolve_env_ref(&database.unwrap_or_default()),
            namespace: resolve_env_ref(&schema.unwrap_or_default()),
            extras: serde_json::json!({
                "cluster_identifier": cluster_identifier,
                "workgroup_name": workgroup_name,
                "db_user": db_user,
                "region": region,
                "staging_s3_bucket": staging_s3_bucket,
                "staging_s3_prefix": staging_s3_prefix,
                "iam_role_arn": iam_role_arn,
            }),
        },
        WarehouseFile::Clickhouse {
            url,
            database,
            user,
            password,
        } => WarehouseResolved {
            kind: WarehouseKind::Clickhouse,
            container: resolve_env_ref(&database.unwrap_or_else(|| "default".to_string())),
            namespace: "default".to_string(),
            extras: serde_json::json!({
                "url": url,
                "user": user,
                "password": password,
            }),
        },
        WarehouseFile::Motherduck {
            motherduck_token,
            database,
            schema,
        } => WarehouseResolved {
            kind: WarehouseKind::Motherduck,
            container: resolve_env_ref(&database.unwrap_or_default()),
            namespace: resolve_env_ref(&schema.unwrap_or_else(|| "main".to_string())),
            extras: serde_json::json!({
                "motherduck_token": motherduck_token,
            }),
        },
    }
}

/// Resolve the raw YAML `providers:` value into the normalised suite_config JSON.
pub fn resolve_providers_from_yaml(
    providers_yaml: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let pf: ProvidersFile = serde_json::from_value(providers_yaml)
        .map_err(|e| format!("failed to parse providers config: {}", e))?;

    let wh_f = pf
        .warehouse
        .ok_or_else(|| "missing providers.warehouse in YAML config".to_string())?;
    let cat_f = pf.catalog.unwrap_or_default();
    let dbt_f = pf.dbt.unwrap_or_default();
    let vec_f = pf.vector.unwrap_or_default();

    let dbt_naming_f = dbt_f.naming.clone().unwrap_or_default();
    let naming_target_schema =
        getenv_nonempty(env_keys::DBT_TARGET_SCHEMA).or(dbt_naming_f.target_schema);
    let naming_silver_suffix = getenv_nonempty(env_keys::DBT_SILVER_SUFFIX)
        .or(dbt_naming_f.silver_suffix)
        .or(Some("silver".to_string()));
    let naming_gold_suffix = getenv_nonempty(env_keys::DBT_GOLD_SUFFIX)
        .or(dbt_naming_f.gold_suffix)
        .or(Some("gold".to_string()));

    let docker_mount_aws_dir = getenv_nonempty(env_keys::DBT_DOCKER_MOUNT_AWS_DIR)
        .map(|v| {
            let vv = v.trim().to_lowercase();
            vv == "1" || vv == "true" || vv == "yes"
        })
        .or(dbt_f.docker_mount_aws_dir)
        .unwrap_or(false);

    let el_f = pf.el.unwrap_or_default();

    let warehouse_resolved = resolve_warehouse(wh_f);
    let providers = serde_json::json!({
        "warehouse": warehouse_resolved,
        "catalog": {
            "enabled": cat_f.enabled.unwrap_or(true),
            "refresh_secs": cat_f.refresh_secs.unwrap_or(60),
            "max_concurrency": cat_f.max_concurrency.unwrap_or(8),
        },
        "dbt": {
            "enabled": dbt_f.enabled.unwrap_or(true),
            "profiles_dir": getenv_nonempty(env_keys::DBT_PROFILES_DIR).or(dbt_f.profiles_dir),
            "target": getenv_nonempty(env_keys::DBT_TARGET)
                .or(dbt_f.target)
                .unwrap_or_default(),
            "naming": {
                "target_schema": naming_target_schema.unwrap_or_default(),
                "silver_suffix": naming_silver_suffix.unwrap_or_default(),
                "gold_suffix": naming_gold_suffix.unwrap_or_default(),
            },
            "runner": getenv_nonempty(env_keys::DBT_RUNNER)
                .or(dbt_f.runner)
                .unwrap_or_else(|| "host".to_string()),
            "docker_image": getenv_nonempty(env_keys::DBT_DOCKER_IMAGE).or(dbt_f.docker_image),
            "docker_platform": getenv_nonempty(env_keys::DBT_DOCKER_PLATFORM)
                .or(dbt_f.docker_platform),
            "docker_network": getenv_nonempty(env_keys::DBT_DOCKER_NETWORK).or(dbt_f.docker_network),
            "docker_mount_aws_dir": docker_mount_aws_dir,
        },
        "vector": {
            "enabled": vec_f.enabled.unwrap_or(true),
        },
        "el": {
            "enabled": el_f.enabled.unwrap_or(false),
            "skippr_binary": getenv_nonempty(env_keys::SKIPPRD_BINARY)
                .or(getenv_nonempty(env_keys::SKIPPR_BINARY))
                .or(el_f.skippr_binary)
                .unwrap_or_else(|| "skipprd".to_string()),
            "skippr_input": el_f.skippr_input.unwrap_or(serde_json::Value::Null),
            "cdc": el_f.cdc,
        },
    });

    Ok(providers)
}

// ---------------------------------------------------------------------------
// Resolved types (runtime representation after config is loaded)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WarehouseKind {
    Athena,
    Postgres,
    Mssql,
    Snowflake,
    Bigquery,
    Databricks,
    Synapse,
    Redshift,
    Clickhouse,
    Motherduck,
}

impl fmt::Display for WarehouseKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Athena => write!(f, "athena"),
            Self::Postgres => write!(f, "postgres"),
            Self::Mssql => write!(f, "mssql"),
            Self::Snowflake => write!(f, "snowflake"),
            Self::Bigquery => write!(f, "bigquery"),
            Self::Databricks => write!(f, "databricks"),
            Self::Synapse => write!(f, "synapse"),
            Self::Redshift => write!(f, "redshift"),
            Self::Clickhouse => write!(f, "clickhouse"),
            Self::Motherduck => write!(f, "motherduck"),
        }
    }
}

impl Default for WarehouseKind {
    fn default() -> Self {
        Self::Athena
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProvidersResolved {
    #[serde(default)]
    pub warehouse: WarehouseResolved,
    #[serde(default)]
    pub catalog: CatalogResolved,
    #[serde(default)]
    pub dbt: DbtResolved,
    #[serde(default)]
    pub vector: VectorResolved,
    #[serde(default)]
    pub el: ElToolResolved,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ElToolResolved {
    pub enabled: bool,
    #[serde(default)]
    pub skippr_binary: String,
    #[serde(default)]
    pub skippr_input: serde_json::Value,
    #[serde(default)]
    pub schema_sink: Option<serde_json::Value>,
    /// CDC configuration. When present, the pipeline runs in CDC mode.
    #[serde(default)]
    pub cdc: Option<CdcConfig>,
}

/// CDC configuration for the EL pipeline.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CdcConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub business_key_columns: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WarehouseResolved {
    #[serde(default)]
    pub kind: WarehouseKind,
    #[serde(default)]
    pub container: String,
    #[serde(default)]
    pub namespace: String,
    #[serde(default)]
    pub extras: serde_json::Value,
}

impl Default for WarehouseResolved {
    fn default() -> Self {
        Self {
            kind: WarehouseKind::default(),
            container: String::new(),
            namespace: String::new(),
            extras: serde_json::Value::Null,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CatalogResolved {
    pub enabled: bool,
    #[serde(default)]
    pub refresh_secs: u64,
    #[serde(default)]
    pub max_concurrency: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VectorResolved {
    pub enabled: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DbtNamingResolved {
    #[serde(default)]
    pub target_schema: String,
    #[serde(default)]
    pub silver_suffix: String,
    #[serde(default)]
    pub gold_suffix: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DbtResolved {
    pub enabled: bool,
    #[serde(default)]
    pub profiles_dir: Option<String>,
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub naming: DbtNamingResolved,
    #[serde(default)]
    pub runner: String,
    #[serde(default)]
    pub docker_image: Option<String>,
    #[serde(default)]
    pub docker_platform: Option<String>,
    #[serde(default)]
    pub docker_network: Option<String>,
    #[serde(default)]
    pub docker_mount_aws_dir: bool,
}

/// Deserialize the data_engineer-specific config from the suite_config Value.
pub fn de_config_from_resolved(
    cfg: &react_core::resolved_config::ReactResolvedConfig,
) -> Option<ProvidersResolved> {
    serde_json::from_value::<ProvidersResolved>(cfg.suite_config.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postgres_warehouse_missing_schema_defaults_to_public() {
        let resolved = resolve_providers_from_yaml(serde_json::json!({
            "warehouse": {
                "kind": "postgres",
                "database": "skippr_test",
                "schema": null
            }
        }))
        .expect("providers should resolve");

        let providers: ProvidersResolved =
            serde_json::from_value(resolved).expect("resolved providers should deserialize");

        assert_eq!(providers.warehouse.kind, WarehouseKind::Postgres);
        assert_eq!(providers.warehouse.container, "skippr_test");
        assert_eq!(providers.warehouse.namespace, "public");
    }

    #[test]
    fn snowflake_extras_resolve_env_refs() {
        const ACCOUNT: &str = "SKIPPR_TEST_SNOWFLAKE_ACCOUNT_DE_CONFIG";
        const USER: &str = "SKIPPR_TEST_SNOWFLAKE_USER_DE_CONFIG";
        const KEY: &str = "SKIPPR_TEST_SNOWFLAKE_KEY_DE_CONFIG";
        std::env::set_var(ACCOUNT, "ORG-ACCT");
        std::env::set_var(USER, "user@example.com");
        std::env::set_var(KEY, "/tmp/snowflake_key.p8");

        let resolved = resolve_providers_from_yaml(serde_json::json!({
            "warehouse": {
                "kind": "snowflake",
                "account": format!("${{{ACCOUNT}}}"),
                "user": format!("${{{USER}}}"),
                "private_key_path": format!("${{{KEY}}}"),
                "database": "ANALYTICS",
                "schema": "RAW",
                "warehouse": "COMPUTE_WH",
                "role": "ACCOUNTADMIN"
            }
        }))
        .expect("providers should resolve");

        std::env::remove_var(ACCOUNT);
        std::env::remove_var(USER);
        std::env::remove_var(KEY);

        let providers: ProvidersResolved =
            serde_json::from_value(resolved).expect("resolved providers should deserialize");
        assert_eq!(
            providers
                .warehouse
                .extras
                .get("account")
                .and_then(|v| v.as_str()),
            Some("ORG-ACCT")
        );
        assert_eq!(
            providers
                .warehouse
                .extras
                .get("user")
                .and_then(|v| v.as_str()),
            Some("user@example.com")
        );
        assert_eq!(
            providers
                .warehouse
                .extras
                .get("private_key_path")
                .and_then(|v| v.as_str()),
            Some("/tmp/snowflake_key.p8")
        );
    }
}
