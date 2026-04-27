use react_core::resolved_config::ReactResolvedConfig;

#[derive(Clone, Debug)]
pub enum ActiveWarehouse {
    Athena,
    Postgres,
    Snowflake,
    Bigquery,
    Mssql,
    Databricks,
    Synapse,
    Redshift,
    Clickhouse,
    Motherduck,
}

#[derive(Clone, Debug)]
pub struct GeneratedProfiles {
    pub target: String,
    pub profiles_yml: String,
}

/// Determine which warehouse provider is active for publishing.
///
/// Today only Athena exists; the shape is designed to extend to other engines.
pub fn active_warehouse(cfg: &ReactResolvedConfig) -> Result<ActiveWarehouse, String> {
    use crate::de_config::WarehouseKind;
    let providers = crate::de_config::de_config_from_resolved(cfg)
        .ok_or_else(|| "suite_config missing or invalid for data_engineer".to_string())?;
    match providers.warehouse.kind {
        WarehouseKind::Athena => Ok(ActiveWarehouse::Athena),
        WarehouseKind::Postgres => Ok(ActiveWarehouse::Postgres),
        WarehouseKind::Snowflake => Ok(ActiveWarehouse::Snowflake),
        WarehouseKind::Bigquery => Ok(ActiveWarehouse::Bigquery),
        WarehouseKind::Mssql => Ok(ActiveWarehouse::Mssql),
        WarehouseKind::Databricks => Ok(ActiveWarehouse::Databricks),
        WarehouseKind::Synapse => Ok(ActiveWarehouse::Synapse),
        WarehouseKind::Redshift => Ok(ActiveWarehouse::Redshift),
        WarehouseKind::Clickhouse => Ok(ActiveWarehouse::Clickhouse),
        WarehouseKind::Motherduck => Ok(ActiveWarehouse::Motherduck),
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
    let providers = crate::de_config::de_config_from_resolved(cfg)
        .ok_or_else(|| "suite_config missing or invalid for data_engineer".to_string())?;
    match active {
        ActiveWarehouse::Athena => {
            let wh = &providers.warehouse;
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
            let schema = if providers.dbt.naming.target_schema.trim().is_empty() {
                derive_scope_db_name(cfg)
            } else {
                providers.dbt.naming.target_schema.trim().to_string()
            };

            // Profile name must match dbt_project.yml `profile:` setting.
            // Existing project scaffolding uses `scope.project_id` today.
            let profile_name = cfg.scope.project_id.clone();

            // Target name: allow config override, default to "athena" for publish workflow.
            let target = if providers.dbt.target.trim().is_empty() {
                "athena".to_string()
            } else {
                providers.dbt.target.trim().to_string()
            };

            // dbt-athena-adapter typical output keys: type, s3_staging_dir, region_name, database, schema, work_group, catalog_name
            let mut out = String::new();
            out.push_str(&format!("{}:\n", yaml_escape_key(profile_name.as_str())));
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
                target,
                profiles_yml: out,
            })
        }
        ActiveWarehouse::Postgres => {
            let wh = &providers.warehouse;
            let profile_name = cfg.scope.project_id.clone();
            let target = if providers.dbt.target.trim().is_empty() {
                "postgres".to_string()
            } else {
                providers.dbt.target.trim().to_string()
            };
            let schema = if providers.dbt.naming.target_schema.trim().is_empty() {
                derive_scope_db_name(cfg)
            } else {
                providers.dbt.naming.target_schema.trim().to_string()
            };
            let dbname = if !wh.container.trim().is_empty() {
                wh.container.trim().to_string()
            } else {
                "{{ env_var('POSTGRES_DATABASE') }}".to_string()
            };
            let host = "{{ env_var('POSTGRES_HOST', 'localhost') }}";
            let user = "{{ env_var('POSTGRES_USER', 'postgres') }}";
            let pass = "{{ env_var('POSTGRES_PASSWORD', '') }}";
            let port = "{{ env_var('POSTGRES_PORT', '5432') | int }}";
            let mut out = String::new();
            out.push_str(&format!("{}:\n", yaml_escape_key(profile_name.as_str())));
            out.push_str(&format!("  target: {}\n", yaml_escape_scalar(&target)));
            out.push_str("  outputs:\n");
            out.push_str(&format!("    {}:\n", yaml_escape_key(&target)));
            out.push_str("      type: postgres\n");
            out.push_str(&format!("      host: {}\n", yaml_escape_scalar(host)));
            out.push_str(&format!("      user: {}\n", yaml_escape_scalar(user)));
            out.push_str(&format!("      password: {}\n", yaml_escape_scalar(pass)));
            out.push_str(&format!("      port: {}\n", yaml_escape_scalar(port)));
            out.push_str(&format!("      dbname: {}\n", yaml_escape_scalar(&dbname)));
            out.push_str(&format!("      schema: {}\n", yaml_escape_scalar(&schema)));
            if let Some(t) = threads {
                out.push_str(&format!("      threads: {}\n", t.max(1)));
            }
            Ok(GeneratedProfiles {
                target,
                profiles_yml: out,
            })
        }
        ActiveWarehouse::Snowflake => {
            let profile_name = cfg.scope.project_id.clone();
            let target = if providers.dbt.target.trim().is_empty() {
                "snowflake".to_string()
            } else {
                providers.dbt.target.trim().to_string()
            };
            let schema = if providers.dbt.naming.target_schema.trim().is_empty() {
                derive_scope_db_name(cfg)
            } else {
                providers.dbt.naming.target_schema.trim().to_string()
            };
            let wh = &providers.warehouse;
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
            let use_keypair = std::env::var("SNOWFLAKE_PRIVATE_KEY_PATH")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .is_some();

            let mut out = String::new();
            out.push_str(&format!("{}:\n", yaml_escape_key(profile_name.as_str())));
            out.push_str(&format!("  target: {}\n", yaml_escape_scalar(&target)));
            out.push_str("  outputs:\n");
            out.push_str(&format!("    {}:\n", yaml_escape_key(&target)));
            out.push_str("      type: snowflake\n");
            out.push_str("      account: \"{{ env_var('SNOWFLAKE_ACCOUNT') }}\"\n");
            out.push_str("      user: \"{{ env_var('SNOWFLAKE_USER') }}\"\n");
            if use_keypair {
                let raw = std::env::var("SNOWFLAKE_PRIVATE_KEY_PATH").unwrap_or_default();
                let abs = resolve_to_absolute_path(&raw);
                std::env::set_var("SNOWFLAKE_PRIVATE_KEY_PATH", &abs);
                out.push_str(
                    "      private_key_path: \"{{ env_var('SNOWFLAKE_PRIVATE_KEY_PATH') }}\"\n",
                );
            } else {
                out.push_str("      password: \"{{ env_var('SNOWFLAKE_PASSWORD') }}\"\n");
            }
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
                target,
                profiles_yml: out,
            })
        }
        ActiveWarehouse::Bigquery => {
            let profile_name = cfg.scope.project_id.clone();
            let target = if providers.dbt.target.trim().is_empty() {
                "bigquery".to_string()
            } else {
                providers.dbt.target.trim().to_string()
            };
            let wh = &providers.warehouse;
            let project = if !wh.container.trim().is_empty() {
                wh.container.trim().to_string()
            } else {
                "{{ env_var('BIGQUERY_PROJECT') }}".to_string()
            };
            let schema = if providers.dbt.naming.target_schema.trim().is_empty() {
                derive_scope_db_name(cfg)
            } else {
                providers.dbt.naming.target_schema.trim().to_string()
            };
            let location = wh
                .extras
                .get("location")
                .and_then(|v| v.as_str())
                .unwrap_or("{{ env_var('BIGQUERY_LOCATION', 'US') }}");
            let use_service_account = std::env::var("GOOGLE_APPLICATION_CREDENTIALS")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .is_some();

            let mut out = String::new();
            out.push_str(&format!("{}:\n", yaml_escape_key(profile_name.as_str())));
            out.push_str(&format!("  target: {}\n", yaml_escape_scalar(&target)));
            out.push_str("  outputs:\n");
            out.push_str(&format!("    {}:\n", yaml_escape_key(&target)));
            out.push_str("      type: bigquery\n");
            if use_service_account {
                out.push_str(&format!(
                    "      method: {}\n",
                    yaml_escape_scalar("service-account")
                ));
                let raw = std::env::var("GOOGLE_APPLICATION_CREDENTIALS").unwrap_or_default();
                let abs = resolve_to_absolute_path(&raw);
                std::env::set_var("GOOGLE_APPLICATION_CREDENTIALS", &abs);
                out.push_str(
                    "      keyfile: \"{{ env_var('GOOGLE_APPLICATION_CREDENTIALS') }}\"\n",
                );
            } else {
                out.push_str(&format!("      method: {}\n", yaml_escape_scalar("oauth")));
            }
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
                target,
                profiles_yml: out,
            })
        }
        ActiveWarehouse::Mssql => {
            let profile_name = cfg.scope.project_id.clone();
            let target = if providers.dbt.target.trim().is_empty() {
                "sqlserver".to_string()
            } else {
                providers.dbt.target.trim().to_string()
            };
            let wh = &providers.warehouse;
            let dbname = if !wh.container.trim().is_empty() {
                wh.container.trim().to_string()
            } else {
                "{{ env_var('MSSQL_DATABASE') }}".to_string()
            };
            let schema = if providers.dbt.naming.target_schema.trim().is_empty() {
                derive_scope_db_name(cfg)
            } else {
                providers.dbt.naming.target_schema.trim().to_string()
            };
            let mut out = String::new();
            out.push_str(&format!("{}:\n", yaml_escape_key(profile_name.as_str())));
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
                target,
                profiles_yml: out,
            })
        }
        ActiveWarehouse::Databricks => {
            let profile_name = cfg.scope.project_id.clone();
            let target = if providers.dbt.target.trim().is_empty() {
                "databricks".to_string()
            } else {
                providers.dbt.target.trim().to_string()
            };
            let wh = &providers.warehouse;
            let schema = if providers.dbt.naming.target_schema.trim().is_empty() {
                derive_scope_db_name(cfg)
            } else {
                providers.dbt.naming.target_schema.trim().to_string()
            };
            let catalog = if !wh.container.trim().is_empty() {
                wh.container.trim().to_string()
            } else {
                "main".to_string()
            };
            let mut out = String::new();
            out.push_str(&format!("{}:\n", yaml_escape_key(profile_name.as_str())));
            out.push_str(&format!("  target: {}\n", yaml_escape_scalar(&target)));
            out.push_str("  outputs:\n");
            out.push_str(&format!("    {}:\n", yaml_escape_key(&target)));
            out.push_str("      type: databricks\n");
            out.push_str("      host: \"{{ env_var('DATABRICKS_HOST', env_var('DATABRICKS_WORKSPACE_URL', '')) }}\"\n");
            out.push_str("      http_path: \"{{ env_var('DATABRICKS_HTTP_PATH', '') }}\"\n");
            out.push_str("      token: \"{{ env_var('DATABRICKS_TOKEN') }}\"\n");
            out.push_str(&format!(
                "      catalog: {}\n",
                yaml_escape_scalar(&catalog)
            ));
            out.push_str(&format!("      schema: {}\n", yaml_escape_scalar(&schema)));
            if let Some(t) = threads {
                out.push_str(&format!("      threads: {}\n", t.max(1)));
            }
            Ok(GeneratedProfiles {
                target,
                profiles_yml: out,
            })
        }
        ActiveWarehouse::Synapse => {
            let profile_name = cfg.scope.project_id.clone();
            let target = if providers.dbt.target.trim().is_empty() {
                "synapse".to_string()
            } else {
                providers.dbt.target.trim().to_string()
            };
            let wh = &providers.warehouse;
            let schema = if providers.dbt.naming.target_schema.trim().is_empty() {
                derive_scope_db_name(cfg)
            } else {
                providers.dbt.naming.target_schema.trim().to_string()
            };
            let mut out = String::new();
            out.push_str(&format!("{}:\n", yaml_escape_key(profile_name.as_str())));
            out.push_str(&format!("  target: {}\n", yaml_escape_scalar(&target)));
            out.push_str("  outputs:\n");
            out.push_str(&format!("    {}:\n", yaml_escape_key(&target)));
            out.push_str("      type: synapse\n");
            out.push_str("      driver: 'ODBC Driver 18 for SQL Server'\n");
            out.push_str("      host: \"{{ env_var('SYNAPSE_HOST') }}\"\n");
            out.push_str("      port: 1433\n");
            out.push_str("      database: \"{{ env_var('SYNAPSE_DATABASE') }}\"\n");
            out.push_str(&format!("      schema: {}\n", yaml_escape_scalar(&schema)));
            out.push_str("      user: \"{{ env_var('SYNAPSE_USER') }}\"\n");
            out.push_str("      password: \"{{ env_var('SYNAPSE_PASSWORD') }}\"\n");
            if let Some(t) = threads {
                out.push_str(&format!("      threads: {}\n", t.max(1)));
            }
            Ok(GeneratedProfiles {
                target,
                profiles_yml: out,
            })
        }
        ActiveWarehouse::Redshift => {
            let profile_name = cfg.scope.project_id.clone();
            let target = if providers.dbt.target.trim().is_empty() {
                "redshift".to_string()
            } else {
                providers.dbt.target.trim().to_string()
            };
            let wh = &providers.warehouse;
            let schema = if providers.dbt.naming.target_schema.trim().is_empty() {
                derive_scope_db_name(cfg)
            } else {
                providers.dbt.naming.target_schema.trim().to_string()
            };
            let dbname = if !wh.container.trim().is_empty() {
                wh.container.trim().to_string()
            } else {
                "{{ env_var('REDSHIFT_DATABASE') }}".to_string()
            };
            let mut out = String::new();
            out.push_str(&format!("{}:\n", yaml_escape_key(profile_name.as_str())));
            out.push_str(&format!("  target: {}\n", yaml_escape_scalar(&target)));
            out.push_str("  outputs:\n");
            out.push_str(&format!("    {}:\n", yaml_escape_key(&target)));
            out.push_str("      type: redshift\n");
            out.push_str("      host: \"{{ env_var('REDSHIFT_HOST') }}\"\n");
            out.push_str("      port: 5439\n");
            out.push_str(&format!("      dbname: {}\n", yaml_escape_scalar(&dbname)));
            out.push_str(&format!("      schema: {}\n", yaml_escape_scalar(&schema)));
            out.push_str("      user: \"{{ env_var('REDSHIFT_USER', '') }}\"\n");
            out.push_str("      password: \"{{ env_var('REDSHIFT_PASSWORD', '') }}\"\n");
            out.push_str("      method: \"{{ env_var('REDSHIFT_METHOD', 'database') }}\"\n");
            if let Some(t) = threads {
                out.push_str(&format!("      threads: {}\n", t.max(1)));
            }
            Ok(GeneratedProfiles {
                target,
                profiles_yml: out,
            })
        }
        ActiveWarehouse::Clickhouse => {
            let profile_name = cfg.scope.project_id.clone();
            let target = if providers.dbt.target.trim().is_empty() {
                "clickhouse".to_string()
            } else {
                providers.dbt.target.trim().to_string()
            };
            let wh = &providers.warehouse;
            let schema = if !wh.container.trim().is_empty() {
                wh.container.trim().to_string()
            } else {
                "default".to_string()
            };
            let mut out = String::new();
            out.push_str(&format!("{}:\n", yaml_escape_key(profile_name.as_str())));
            out.push_str(&format!("  target: {}\n", yaml_escape_scalar(&target)));
            out.push_str("  outputs:\n");
            out.push_str(&format!("    {}:\n", yaml_escape_key(&target)));
            out.push_str("      type: clickhouse\n");
            out.push_str("      host: \"{{ env_var('CLICKHOUSE_HOST', 'localhost') }}\"\n");
            out.push_str("      port: 8123\n");
            out.push_str(&format!("      schema: {}\n", yaml_escape_scalar(&schema)));
            out.push_str("      user: \"{{ env_var('CLICKHOUSE_USER', 'default') }}\"\n");
            out.push_str("      password: \"{{ env_var('CLICKHOUSE_PASSWORD', '') }}\"\n");
            if let Some(t) = threads {
                out.push_str(&format!("      threads: {}\n", t.max(1)));
            }
            Ok(GeneratedProfiles {
                target,
                profiles_yml: out,
            })
        }
        ActiveWarehouse::Motherduck => {
            let profile_name = cfg.scope.project_id.clone();
            let target = if providers.dbt.target.trim().is_empty() {
                "motherduck".to_string()
            } else {
                providers.dbt.target.trim().to_string()
            };
            let wh = &providers.warehouse;
            let schema = if providers.dbt.naming.target_schema.trim().is_empty() {
                derive_scope_db_name(cfg)
            } else {
                providers.dbt.naming.target_schema.trim().to_string()
            };
            let database = if wh.container.is_empty() {
                "my_db"
            } else {
                wh.container.as_str()
            };
            let md_path = format!("md:{}", database);
            let mut out = String::new();
            out.push_str(&format!("{}:\n", yaml_escape_key(profile_name.as_str())));
            out.push_str(&format!("  target: {}\n", yaml_escape_scalar(&target)));
            out.push_str("  outputs:\n");
            out.push_str(&format!("    {}:\n", yaml_escape_key(&target)));
            out.push_str("      type: duckdb\n");
            out.push_str(&format!("      path: {}\n", yaml_escape_scalar(&md_path)));
            out.push_str(&format!("      schema: {}\n", yaml_escape_scalar(&schema)));
            out.push_str("      settings:\n");
            out.push_str("        motherduck_token: \"{{ env_var('MOTHERDUCK_TOKEN') }}\"\n");
            if let Some(t) = threads {
                out.push_str(&format!("      threads: {}\n", t.max(1)));
            }
            Ok(GeneratedProfiles {
                target,
                profiles_yml: out,
            })
        }
    }
}

pub(crate) fn derive_scope_db_name(cfg: &ReactResolvedConfig) -> String {
    // Default to a simple, stable base schema derived from project_id.
    //
    // dbt's default generate_schema_name macro will append our tier suffixes, yielding:
    //   <project_id>_<silver_suffix> and <project_id>_<gold_suffix>
    //
    // This keeps warehouse names short and predictable (e.g. "example_silver2"),
    // while still allowing overrides via providers.dbt.naming.target_schema.
    sanitize_ident(cfg.scope.project_id.as_str())
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

fn resolve_to_absolute_path(raw: &str) -> String {
    let p = std::path::Path::new(raw);
    let abs = if p.is_absolute() {
        raw.to_string()
    } else {
        std::env::current_dir()
            .ok()
            .map(|cwd| cwd.join(p).to_string_lossy().to_string())
            .unwrap_or_else(|| raw.to_string())
    };
    // Normalize to forward slashes so the path is safe in YAML double-quoted
    // strings and portable across OSes (Windows accepts forward slashes).
    abs.replace('\\', "/")
}

fn yaml_escape_key(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

fn yaml_escape_scalar<S: AsRef<str>>(s: S) -> String {
    let s = s.as_ref();
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use react_core::scope::RequestScope;

    #[test]
    fn generate_athena_profiles_requires_result_s3() {
        let cfg = ReactResolvedConfig {
            server: react_core::resolved_config::ServerResolved { port: 1 },
            storage: react_core::resolved_config::StorageResolved {
                mode: react_core::resolved_config::StorageMode::Local,
                bucket: None,
                path: None,
                s3_credentials: None,
            },
            scope: RequestScope::parse("t", "w", "p").expect("valid test scope"),
            llm: react_core::resolved_config::LlmResolved::default(),
            suite_config: serde_json::json!({
                "warehouse": { "kind": "athena", "container": "AwsDataCatalog", "namespace": "src", "extras": {} },
                "catalog": { "enabled": false, "refresh_secs": 60, "max_concurrency": 8 },
                "dbt": { "enabled": false, "target": "athena", "naming": {}, "runner": "host" },
                "vector": { "enabled": false }
            }),
        };
        let err = generate_profiles_yml(&cfg, None).unwrap_err();
        assert!(err.contains("result_s3"));
    }

    #[test]
    fn yaml_escape_scalar_escapes_backslashes() {
        assert_eq!(
            yaml_escape_scalar(r"D:\a\react\react\key.p8"),
            r#""D:\\a\\react\\react\\key.p8""#
        );
    }

    #[test]
    fn yaml_escape_scalar_escapes_quotes_and_backslashes() {
        assert_eq!(
            yaml_escape_scalar(r#"say "hello" to C:\Users"#),
            r#""say \"hello\" to C:\\Users""#
        );
    }

    #[test]
    fn yaml_escape_scalar_plain_string_unchanged() {
        assert_eq!(yaml_escape_scalar("simple"), r#""simple""#);
    }

    #[test]
    fn resolve_to_absolute_path_normalizes_backslashes() {
        let result = resolve_to_absolute_path("some/relative/path.p8");
        assert!(
            !result.contains('\\'),
            "resolved path should not contain backslashes: {result}"
        );
        assert!(result.contains('/'));
    }
}
