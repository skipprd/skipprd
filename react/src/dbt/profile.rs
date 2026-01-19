use crate::config::ReactResolvedConfig;

#[derive(Clone, Debug)]
pub enum ActiveWarehouse {
    Athena,
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
    let mut enabled: Vec<&'static str> = Vec::new();
    if cfg.providers.athena.enabled {
        enabled.push("athena");
    }
    // Future: bigquery/snowflake/postgres/etc.
    if enabled.is_empty() {
        return Err("no warehouse provider enabled for publishing (enable providers.athena or another warehouse provider)".to_string());
    }
    if enabled.len() > 1 {
        return Err(format!(
            "multiple warehouse providers enabled for publishing: {} (only one may be enabled)",
            enabled.join(", ")
        ));
    }
    Ok(ActiveWarehouse::Athena)
}

/// Generate a DBT `profiles.yml` from resolved config.
///
/// - No secrets are written (AWS credentials remain env-driven/default chain).
/// - Uses provider-native adapter config shape.
/// - Uses deterministic database/schema naming derived from scope (unless overridden by provider config).
pub fn generate_profiles_yml(cfg: &ReactResolvedConfig) -> Result<GeneratedProfiles, String> {
    let active = active_warehouse(cfg)?;
    match active {
        ActiveWarehouse::Athena => {
            let ath = &cfg.providers.athena;
            // dbt-athena-adapter expects a region.
            // Prefer explicit configured region (ensures work_group lookup happens in the right region),
            // otherwise defer to env with a safe default.
            let region_value = ath
                .region
                .clone()
                .unwrap_or_else(|| "{{ env_var('AWS_REGION', env_var('AWS_DEFAULT_REGION', 'us-east-1')) }}".to_string());
            let s3_staging_dir = ath
                .result_s3
                .as_ref()
                .ok_or_else(|| "providers.athena.result_s3 is required to generate DBT athena profile".to_string())?
                .trim()
                .to_string();
            let work_group = ath.workgroup.clone();
            // NOTE: dbt-athena adapter config is sensitive to "catalog" vs "database" mappings.
            // Some versions treat catalog-related keys as aliases of database, causing:
            // "Got duplicate keys: (catalog) all map to \"database\"".
            // We keep catalog in `react` config for query qualification, but do NOT emit it into
            // profiles.yml to avoid adapter conflicts.
            let _catalog_name = ath.target_catalog.clone();

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
            let database = ath.target_catalog.clone();
            let schema = cfg
                .providers
                .dbt
                .naming
                .target_schema
                .clone()
                .unwrap_or_else(|| derive_scope_db_name(cfg));

            // Profile name must match dbt_project.yml `profile:` setting.
            // Existing project scaffolding uses `scope.project_id` today.
            let profile_name = cfg.scope.project_id.clone();

            // Target name: allow config override, default to "athena" for publish workflow.
            let target = cfg
                .providers
                .dbt
                .target
                .clone()
                .unwrap_or_else(|| "athena".to_string());

            // dbt-athena-adapter typical output keys: type, s3_staging_dir, region_name, database, schema, work_group, catalog_name
            let mut out = String::new();
            out.push_str(&format!("{}:\n", yaml_escape_key(&profile_name)));
            out.push_str(&format!("  target: {}\n", yaml_escape_scalar(&target)));
            out.push_str("  outputs:\n");
            out.push_str(&format!("    {}:\n", yaml_escape_key(&target)));
            out.push_str("      type: athena\n");
            out.push_str(&format!("      s3_staging_dir: {}\n", yaml_escape_scalar(&s3_staging_dir)));
            // Quote the Jinja expression so YAML parses correctly.
            out.push_str(&format!("      region_name: {}\n", yaml_escape_scalar(&region_value)));
            out.push_str(&format!("      database: {}\n", yaml_escape_scalar(&database)));
            out.push_str(&format!("      schema: {}\n", yaml_escape_scalar(&schema)));
            // Default concurrency unless overridden elsewhere (env/CLI).
            out.push_str("      threads: 4\n");
            if let Some(wg) = work_group {
                if !wg.trim().is_empty() {
                    out.push_str(&format!("      work_group: {}\n", yaml_escape_scalar(wg.trim())));
                }
            }
            Ok(GeneratedProfiles { profile_name, target, profiles_yml: out, active })
        }
    }
}

fn derive_scope_db_name(cfg: &ReactResolvedConfig) -> String {
    // Conservative identifier: de_<tenant>_<workspace>_<project_id>
    let raw = format!("de_{}_{}_{}", cfg.scope.tenant, cfg.scope.workspace, cfg.scope.project_id);
    sanitize_ident(&raw)
}

fn sanitize_ident(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let ok = ch.is_ascii_alphanumeric() || ch == '_';
        out.push(if ok { ch } else { '_' });
    }
    // Avoid leading digits (Athena/Glue constraints vary; keep safe)
    if out.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
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
    if s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
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

    #[test]
    fn generate_athena_profiles_requires_result_s3() {
        let cfg = ReactResolvedConfig {
            server: crate::config::ServerResolved { port: 1 },
            storage: crate::config::StorageResolved { bucket: "b".to_string() },
            scope: crate::providers::RequestScope { tenant: "t".to_string(), workspace: "w".to_string(), project_id: "p".to_string() },
            llm: crate::config::LlmResolved::default(),
            providers: crate::config::ProvidersResolved {
                athena: crate::config::AthenaResolved {
                    enabled: true,
                    workgroup: None,
                    region: None,
                    result_s3: None,
                    source_schema: None,
                    target_catalog: "AwsDataCatalog".to_string(),
                    silver_schema: None,
                    gold_schema: None,
                    discovery_cache_ttl_secs: 120,
                },
                catalog: crate::config::CatalogResolved { enabled: false, refresh_secs: 60, max_concurrency: 8 },
                dbt: crate::config::DbtResolved {
                    enabled: false,
                    profiles_dir: None,
                    target: None,
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
        let err = generate_profiles_yml(&cfg).unwrap_err();
        assert!(err.contains("result_s3"));
    }
}

