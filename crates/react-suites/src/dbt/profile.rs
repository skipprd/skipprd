use crate::config::ReactResolvedConfig;

#[derive(Clone, Debug)]
pub enum ActiveWarehouse {
    Athena,
    Postgres,
    Snowflake,
    Bigquery,
    Mssql,
}

#[derive(Clone, Debug)]
pub struct GeneratedProfiles {
    pub profile_name: String,
    pub target: String,
    pub profiles_yml: String,
    pub active: ActiveWarehouse,
}

/// Determine which warehouse provider is active for publishing.
///
/// Today only Athena exists; the shape is designed to extend to other engines.
pub fn active_warehouse(cfg: &ReactResolvedConfig) -> Result<ActiveWarehouse, String> {
    match cfg
        .providers
        .warehouse
        .kind
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "athena" => Ok(ActiveWarehouse::Athena),
        "postgres" => Ok(ActiveWarehouse::Postgres),
        "snowflake" => Ok(ActiveWarehouse::Snowflake),
        "bigquery" => Ok(ActiveWarehouse::Bigquery),
        "mssql" | "sqlserver" => Ok(ActiveWarehouse::Mssql),
        _ => Err(format!(
            "unsupported providers.warehouse.kind '{}' for profiles generation",
            cfg.providers.warehouse.kind
        )),
    }
}

/// Generate a DBT `profiles.yml` from resolved config.
///
/// - No secrets are written (AWS credentials remain env-driven/default chain).
/// - Uses provider-native adapter config shape.
/// - Uses deterministic database/schema naming derived from scope (unless overridden by provider config).
pub fn generate_profiles_yml(
    cfg: &ReactResolvedConfig,
    threads: Option<usize>,
) -> Result<GeneratedProfiles, String> {
    let active = active_warehouse(cfg)?;
    match active {
        ActiveWarehouse::Athena => {
            let wh = &cfg.providers.warehouse;
            // dbt-athena-adapter expects a region.
            // Prefer explicit configured region (ensures work_group lookup happens in the right region),
            // otherwise defer to env with a safe default.
            let region_value = wh
                .extras
                .get("region")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| {
                    "{{ env_var('AWS_REGION', env_var('AWS_DEFAULT_REGION', 'us-east-1')) }}"
                        .to_string()
                });
            let s3_staging_dir = wh
                .extras
                .get("result_s3")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if s3_staging_dir.is_empty() {
                return Err(
                    "providers.target (athena) requires result_s3 to generate a dbt-athena profile"
                        .to_string(),
                );
            }
            let work_group = wh
                .extras
                .get("workgroup")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // NOTE: dbt-athena adapter config is sensitive to "catalog" vs "database" mappings.
            // Some versions treat catalog-related keys as aliases of database, causing:
            // "Got duplicate keys: (catalog) all map to \"database\"".
            // We keep catalog in `react` config for query qualification, but do NOT emit it into
            // profiles.yml to avoid adapter conflicts.
            let _catalog_name = wh.container.clone();

            // dbt-athena mapping (IMPORTANT):
            // - `database` is the Athena Data Catalog name (e.g. "AwsDataCatalog")
            // - `schema` is the Athena database within that catalog (e.g. "picnic")
            //
            // Previously we incorrectly mapped `default_database` into `database`, which caused:
            // "GetDataCatalog(Name=<schema>)" and failures like "DataCatalog picnic was not found".
            // dbt-athena mapping (IMPORTANT):
            // - `database` is the Athena Data Catalog name (e.g. "AwsDataCatalog")
            // - `schema` is the base schema used by dbt's schema generation macros.
            //
            // With the suffix strategy (recommended), dbt materializes into:
            //   <target_schema>_<tier_suffix>
            // so `schema` must be the BASE (e.g. "test"), not the final tier schema.
            let database = wh.container.clone();
            let schema = if cfg.providers.dbt.naming.target_schema.trim().is_empty() {
                derive_scope_db_name(cfg)
            } else {
                cfg.providers.dbt.naming.target_schema.trim().to_string()
            };

            // Profile name must match dbt_project.yml `profile:` setting.
            // Existing project scaffolding uses `scope.project_id` today.
            let profile_name = cfg.scope.project_id.clone();

            // Target name: allow config override, default to "athena" for publish workflow.
            let target = if cfg.providers.dbt.target.trim().is_empty() {
                "athena".to_string()
            } else {
                cfg.providers.dbt.target.trim().to_string()
            };

            // dbt-athena-adapter typical output keys: type, s3_staging_dir, region_name, database, schema, work_group, catalog_name
            let mut out = String::new();
            out.push_str(&format!("{}:\n", yaml_escape_key(&profile_name)));
            out.push_str(&format!("  target: {}\n", yaml_escape_scalar(&target)));
            out.push_str("  outputs:\n");
            out.push_str(&format!("    {}:\n", yaml_escape_key(&target)));
            out.push_str("      type: athena\n");
            out.push_str(&format!(
                "      s3_staging_dir: {}\n",
                yaml_escape_scalar(&s3_staging_dir)
            ));
            // Quote the Jinja expression so YAML parses correctly.
            out.push_str(&format!(
                "      region_name: {}\n",
                yaml_escape_scalar(&region_value)
            ));
            out.push_str(&format!(
                "      database: {}\n",
                yaml_escape_scalar(&database)
            ));
            out.push_str(&format!("      schema: {}\n", yaml_escape_scalar(&schema)));
            if let Some(t) = threads {
                out.push_str(&format!("      threads: {}\n", t.max(1)));
            }
            if !work_group.trim().is_empty() {
                out.push_str(&format!(
                    "      work_group: {}\n",
                    yaml_escape_scalar(work_group.trim())
                ));
            }
            Ok(GeneratedProfiles {
                profile_name,
                target,
                profiles_yml: out,
                active,
            })
        }
        ActiveWarehouse::Postgres => {
            let wh = &cfg.providers.warehouse;
            let profile_name = cfg.scope.project_id.clone();
            let target = if cfg.providers.dbt.target.trim().is_empty() {
                "postgres".to_string()
            } else {
                cfg.providers.dbt.target.trim().to_string()
            };
            let schema = if cfg.providers.dbt.naming.target_schema.trim().is_empty() {
                derive_scope_db_name(cfg)
            } else {
                cfg.providers.dbt.naming.target_schema.trim().to_string()
            };
            let dbname = if !wh.container.trim().is_empty() {
                wh.container.trim().to_string()
            } else {
                "{{ env_var('PGDATABASE') }}".to_string()
            };
            let host = "{{ env_var('PGHOST', 'localhost') }}";
            let user = "{{ env_var('PGUSER', 'postgres') }}";
            let pass = "{{ env_var('PGPASSWORD', '') }}";
            let port = "{{ env_var('PGPORT', '5432') | int }}";
            let mut out = String::new();
            out.push_str(&format!("{}:\n", yaml_escape_key(&profile_name)));
            out.push_str(&format!("  target: {}\n", yaml_escape_scalar(&target)));
            out.push_str("  outputs:\n");
            out.push_str(&format!("    {}:\n", yaml_escape_key(&target)));
            out.push_str("      type: postgres\n");
            out.push_str(&format!("      host: {}\n", yaml_escape_scalar(host)));
            out.push_str(&format!("      user: {}\n", yaml_escape_scalar(user)));
            out.push_str(&format!("      password: {}\n", yaml_escape_scalar(pass)));
            out.push_str(&format!("      port: {}\n", port));
            out.push_str(&format!("      dbname: {}\n", yaml_escape_scalar(&dbname)));
            out.push_str(&format!("      schema: {}\n", yaml_escape_scalar(&schema)));
            if let Some(t) = threads {
                out.push_str(&format!("      threads: {}\n", t.max(1)));
            }
            Ok(GeneratedProfiles {
                profile_name,
                target,
                profiles_yml: out,
                active,
            })
        }
        ActiveWarehouse::Snowflake => {
            let profile_name = cfg.scope.project_id.clone();
            let target = if cfg.providers.dbt.target.trim().is_empty() {
                "snowflake".to_string()
            } else {
                cfg.providers.dbt.target.trim().to_string()
            };
            let schema = if cfg.providers.dbt.naming.target_schema.trim().is_empty() {
                derive_scope_db_name(cfg)
            } else {
                cfg.providers.dbt.naming.target_schema.trim().to_string()
            };
            let wh = &cfg.providers.warehouse;
            let database = if !wh.container.trim().is_empty() {
                wh.container.trim().to_string()
            } else {
                "{{ env_var('SNOWFLAKE_DATABASE') }}".to_string()
            };
            let warehouse = wh
                .extras
                .get("warehouse")
                .and_then(|v| v.as_str())
                .unwrap_or("{{ env_var('SNOWFLAKE_WAREHOUSE') }}");
            let role = wh
                .extras
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("{{ env_var('SNOWFLAKE_ROLE') }}");
            let mut out = String::new();
            out.push_str(&format!("{}:\n", yaml_escape_key(&profile_name)));
            out.push_str(&format!("  target: {}\n", yaml_escape_scalar(&target)));
            out.push_str("  outputs:\n");
            out.push_str(&format!("    {}:\n", yaml_escape_key(&target)));
            out.push_str("      type: snowflake\n");
            out.push_str("      account: \"{{ env_var('SNOWFLAKE_ACCOUNT') }}\"\n");
            out.push_str("      user: \"{{ env_var('SNOWFLAKE_USER') }}\"\n");
            out.push_str("      password: \"{{ env_var('SNOWFLAKE_PASSWORD') }}\"\n");
            out.push_str(&format!("      role: {}\n", yaml_escape_scalar(role)));
            out.push_str(&format!(
                "      database: {}\n",
                yaml_escape_scalar(&database)
            ));
            out.push_str(&format!(
                "      warehouse: {}\n",
                yaml_escape_scalar(warehouse)
            ));
            out.push_str(&format!("      schema: {}\n", yaml_escape_scalar(&schema)));
            if let Some(t) = threads {
                out.push_str(&format!("      threads: {}\n", t.max(1)));
            }
            Ok(GeneratedProfiles {
                profile_name,
                target,
                profiles_yml: out,
                active,
            })
        }
        ActiveWarehouse::Bigquery => {
            let profile_name = cfg.scope.project_id.clone();
            let target = if cfg.providers.dbt.target.trim().is_empty() {
                "bigquery".to_string()
            } else {
                cfg.providers.dbt.target.trim().to_string()
            };
            let wh = &cfg.providers.warehouse;
            let project = if !wh.container.trim().is_empty() {
                wh.container.trim().to_string()
            } else {
                "{{ env_var('BIGQUERY_PROJECT') }}".to_string()
            };
            let schema = if cfg.providers.dbt.naming.target_schema.trim().is_empty() {
                derive_scope_db_name(cfg)
            } else {
                cfg.providers.dbt.naming.target_schema.trim().to_string()
            };
            let location = wh
                .extras
                .get("location")
                .and_then(|v| v.as_str())
                .unwrap_or("{{ env_var('BIGQUERY_LOCATION', 'US') }}");
            let mut out = String::new();
            out.push_str(&format!("{}:\n", yaml_escape_key(&profile_name)));
            out.push_str(&format!("  target: {}\n", yaml_escape_scalar(&target)));
            out.push_str("  outputs:\n");
            out.push_str(&format!("    {}:\n", yaml_escape_key(&target)));
            out.push_str("      type: bigquery\n");
            out.push_str(&format!("      method: {}\n", yaml_escape_scalar("oauth")));
            out.push_str(&format!(
                "      project: {}\n",
                yaml_escape_scalar(&project)
            ));
            out.push_str(&format!("      dataset: {}\n", yaml_escape_scalar(&schema)));
            out.push_str(&format!(
                "      location: {}\n",
                yaml_escape_scalar(location)
            ));
            if let Some(t) = threads {
                out.push_str(&format!("      threads: {}\n", t.max(1)));
            }
            Ok(GeneratedProfiles {
                profile_name,
                target,
                profiles_yml: out,
                active,
            })
        }
        ActiveWarehouse::Mssql => {
            let profile_name = cfg.scope.project_id.clone();
            let target = if cfg.providers.dbt.target.trim().is_empty() {
                "sqlserver".to_string()
            } else {
                cfg.providers.dbt.target.trim().to_string()
            };
            let wh = &cfg.providers.warehouse;
            let dbname = if !wh.container.trim().is_empty() {
                wh.container.trim().to_string()
            } else {
                "{{ env_var('MSSQL_DATABASE') }}".to_string()
            };
            let schema = if cfg.providers.dbt.naming.target_schema.trim().is_empty() {
                derive_scope_db_name(cfg)
            } else {
                cfg.providers.dbt.naming.target_schema.trim().to_string()
            };
            let mut out = String::new();
            out.push_str(&format!("{}:\n", yaml_escape_key(&profile_name)));
            out.push_str(&format!("  target: {}\n", yaml_escape_scalar(&target)));
            out.push_str("  outputs:\n");
            out.push_str(&format!("    {}:\n", yaml_escape_key(&target)));
            out.push_str("      type: sqlserver\n");
            out.push_str("      driver: 'ODBC Driver 18 for SQL Server'\n");
            out.push_str("      server: \"{{ env_var('MSSQL_HOST') }}\"\n");
            out.push_str("      port: 1433\n");
            out.push_str(&format!(
                "      database: {}\n",
                yaml_escape_scalar(&dbname)
            ));
            out.push_str(&format!("      schema: {}\n", yaml_escape_scalar(&schema)));
            out.push_str("      user: \"{{ env_var('MSSQL_USER') }}\"\n");
            out.push_str("      password: \"{{ env_var('MSSQL_PASSWORD') }}\"\n");
            if let Some(t) = threads {
                out.push_str(&format!("      threads: {}\n", t.max(1)));
            }
            Ok(GeneratedProfiles {
                profile_name,
                target,
                profiles_yml: out,
                active,
            })
        }
    }
}

fn derive_scope_db_name(cfg: &ReactResolvedConfig) -> String {
    // Default to a simple, stable base schema derived from project_id.
    //
    // dbt's default generate_schema_name macro will append our tier suffixes, yielding:
    //   <project_id>_<silver_suffix> and <project_id>_<gold_suffix>
    //
    // This keeps warehouse names short and predictable (e.g. "example_silver2"),
    // while still allowing overrides via providers.dbt.naming.target_schema.
    sanitize_ident(&cfg.scope.project_id)
}

fn sanitize_ident(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let ok = ch.is_ascii_alphanumeric() || ch == '_';
        out.push(if ok { ch } else { '_' });
    }
    // Avoid leading digits (Athena/Glue constraints vary; keep safe)
    if out
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
    {
        out.insert(0, '_');
    }
    // Collapse repeated underscores
    while out.contains("__") {
        out = out.replace("__", "_");
    }
    out.trim_matches('_').to_string()
}

fn yaml_escape_key(s: &str) -> String {
    // Minimal escape: quote if it contains special chars
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('"', "\\\""))
    }
}

fn yaml_escape_scalar<S: AsRef<str>>(s: S) -> String {
    let s = s.as_ref();
    // Always quote scalars to be safe with punctuation like ':' or '/'.
    format!("\"{}\"", s.replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use react_core::scope::RequestScope;

    #[test]
    fn generate_athena_profiles_requires_result_s3() {
        let cfg = ReactResolvedConfig {
            server: crate::config::ServerResolved { port: 1 },
            storage: crate::config::StorageResolved {
                bucket: "b".to_string(),
            },
            scope: RequestScope {
                tenant: "t".to_string(),
                workspace: "w".to_string(),
                project_id: "p".to_string(),
            },
            llm: crate::config::LlmResolved::default(),
            providers: crate::config::ProvidersResolved {
                warehouse: crate::config::WarehouseResolved {
                    kind: "athena".to_string(),
                    container: "AwsDataCatalog".to_string(),
                    namespace: "src".to_string(),
                    extras: serde_json::json!({}),
                },
                catalog: crate::config::CatalogResolved {
                    enabled: false,
                    refresh_secs: 60,
                    max_concurrency: 8,
                },
                dbt: crate::config::DbtResolved {
                    enabled: false,
                    profiles_dir: None,
                    target: "athena".to_string(),
                    naming: crate::config::DbtNamingResolved::default(),
                    runner: "host".to_string(),
                    docker_image: None,
                    docker_platform: None,
                    docker_network: None,
                    docker_mount_aws_dir: false,
                },
                vector: crate::config::VectorResolved { enabled: false },
            },
        };
        let err = generate_profiles_yml(&cfg, None).unwrap_err();
        assert!(err.contains("result_s3"));
    }
}
