use serde::Deserialize;
use std::fs;
use std::path::Path;
 
use crate::providers::RequestScope;

/// # `react` configuration
///
/// `react serve` loads a **YAML** config file via `--config <PATH>`.
///
/// ## Precedence
/// - CLI flag (if provided)
/// - Environment variable (if set and non-empty)
/// - YAML config file
/// - Hardcoded default
///
/// ## Example YAML
///
/// ```yaml
/// version: 1
///
/// server:
///   port: 8787
///
/// storage:
///   bucket: my-react-bucket
///
/// scope:
///   tenant: default
///   workspace: default
///   project_id: default
///
/// llm:
///   provider: OPENAI_COMPAT
///   base_url: https://api.openai.com
///   chat_model: gpt-5.1
///   embed_model: text-embedding-3-small
///   context_length: 4096
///   http_timeout_secs: 30
///   max_tokens: 1024
///   temperature: 0.2
///   top_p: 1.0
///
/// providers:
///   athena:
///     enabled: true
///     workgroup: my_wg
///     default_database: my_db
///     catalog: AwsDataCatalog
///     result_s3: s3://my-query-results/
///     discovery_cache_ttl_secs: 120
///   catalog:
///     enabled: true
///     refresh_secs: 60
///     max_concurrency: 8
///   dbt:
///     enabled: true
///     # Run DBT in a deterministic environment.
///     runner: docker
///     docker_image: ghcr.io/dbt-labs/dbt-athena:1.8.3
///     docker_mount_aws_dir: true
///     target: athena
///   vector:
///     enabled: true
/// ```
///
/// Notes:
/// - Secrets remain env-driven (e.g. `LLM_API_KEY`).
/// - `storage.bucket` can also be provided via env `SKIPPR_S3_BUCKET` or CLI `--bucket`.
 
/// CLI overrides for `react serve`.
///
/// Any `Some` value takes precedence over env and file config.
#[derive(Clone, Debug, Default)]
pub struct ServeOverrides {
    pub port: Option<u16>,
    pub bucket: Option<String>,
    pub tenant: Option<String>,
    pub workspace: Option<String>,
    pub project_id: Option<String>,
}
 
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ReactConfigFile {
    pub version: Option<u32>,
    pub server: Option<ServerFile>,
    pub storage: Option<StorageFile>,
    pub scope: Option<ScopeFile>,
    pub llm: Option<LlmFile>,
    pub providers: Option<ProvidersFile>,
}
 
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ServerFile {
    pub port: Option<u16>,
}
 
#[derive(Clone, Debug, Default, Deserialize)]
pub struct StorageFile {
    pub bucket: Option<String>,
}
 
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ScopeFile {
    pub tenant: Option<String>,
    pub workspace: Option<String>,
    pub project_id: Option<String>,
}
 
#[derive(Clone, Debug, Default, Deserialize)]
pub struct LlmFile {
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub chat_model: Option<String>,
    pub embed_model: Option<String>,
    pub context_length: Option<usize>,
    pub gpu_layers: Option<usize>,
 
    // Common tuning knobs already supported via env in `react` today.
    pub http_timeout_secs: Option<u64>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
}
 
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ProvidersFile {
    pub athena: Option<AthenaFile>,
    pub catalog: Option<CatalogFile>,
    pub dbt: Option<DbtFile>,
    pub vector: Option<VectorFile>,
}
 
#[derive(Clone, Debug, Default, Deserialize)]
pub struct AthenaFile {
    pub enabled: Option<bool>,
    pub workgroup: Option<String>,
    pub result_s3: Option<String>,
    pub default_database: Option<String>,
    pub catalog: Option<String>,
    pub discovery_cache_ttl_secs: Option<u64>,
}
 
#[derive(Clone, Debug, Default, Deserialize)]
pub struct CatalogFile {
    pub enabled: Option<bool>,
    pub refresh_secs: Option<u64>,
    pub max_concurrency: Option<usize>,
}
 
#[derive(Clone, Debug, Default, Deserialize)]
pub struct DbtFile {
    pub enabled: Option<bool>,
    pub profiles_dir: Option<String>,
    pub target: Option<String>,
    /// DBT runner mode: "host" (default) or "docker".
    pub runner: Option<String>,
    /// Docker image reference to use when runner=="docker".
    pub docker_image: Option<String>,
    pub docker_platform: Option<String>,
    pub docker_network: Option<String>,
    pub docker_mount_aws_dir: Option<bool>,
}
 
#[derive(Clone, Debug, Default, Deserialize)]
pub struct VectorFile {
    pub enabled: Option<bool>,
}
 
#[derive(Clone, Debug)]
pub struct ReactResolvedConfig {
    pub server: ServerResolved,
    pub storage: StorageResolved,
    pub scope: RequestScope,
    pub llm: LlmResolved,
    pub providers: ProvidersResolved,
}
 
#[derive(Clone, Debug)]
pub struct ServerResolved {
    pub port: u16,
}
 
#[derive(Clone, Debug)]
pub struct StorageResolved {
    pub bucket: String,
}
 
#[derive(Clone, Debug, Default)]
pub struct LlmResolved {
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub chat_model: Option<String>,
    pub embed_model: Option<String>,
    pub context_length: Option<usize>,
    pub gpu_layers: Option<usize>,
    pub http_timeout_secs: Option<u64>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
}
 
#[derive(Clone, Debug)]
pub struct ProvidersResolved {
    pub athena: AthenaResolved,
    pub catalog: CatalogResolved,
    pub dbt: DbtResolved,
    pub vector: VectorResolved,
}
 
#[derive(Clone, Debug)]
pub struct AthenaResolved {
    pub enabled: bool,
    pub workgroup: Option<String>,
    pub result_s3: Option<String>,
    pub default_database: Option<String>,
    pub catalog: String,
    pub discovery_cache_ttl_secs: u64,
}
 
#[derive(Clone, Debug)]
pub struct CatalogResolved {
    pub enabled: bool,
    pub refresh_secs: u64,
    pub max_concurrency: usize,
}
 
#[derive(Clone, Debug)]
pub struct DbtResolved {
    pub enabled: bool,
    pub profiles_dir: Option<String>,
    pub target: Option<String>,
    pub runner: String,
    pub docker_image: Option<String>,
    pub docker_platform: Option<String>,
    pub docker_network: Option<String>,
    pub docker_mount_aws_dir: bool,
}
 
#[derive(Clone, Debug)]
pub struct VectorResolved {
    pub enabled: bool,
}
 
fn getenv_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().and_then(|v| {
        let t = v.trim();
        if t.is_empty() { None } else { Some(t.to_string()) }
    })
}
 
fn set_env_if_unset(key: &str, value: &str) {
    if getenv_nonempty(key).is_some() {
        return;
    }
    std::env::set_var(key, value);
}
 
fn ensure_safe_segment(name: &str, v: &str) -> Result<(), String> {
    let t = v.trim();
    if t.is_empty() {
        return Err(format!("scope.{} is empty", name));
    }
    if t.contains('/') || t.contains('\\') {
        return Err(format!("scope.{} must not contain path separators", name));
    }
    if t.contains("..") {
        return Err(format!("scope.{} must not contain '..'", name));
    }
    Ok(())
}
 
impl ReactConfigFile {
    pub fn load_yaml(path: &Path) -> Result<Self, String> {
        let bytes = fs::read(path).map_err(|e| format!("failed to read config file: {}", e))?;
        serde_yaml::from_slice::<Self>(&bytes)
            .map_err(|e| format!("failed to parse YAML config: {}", e))
    }
}
 
impl ReactResolvedConfig {
    pub fn resolve(file: ReactConfigFile, ov: ServeOverrides) -> Result<Self, String> {
        let server_port = ov
            .port
            .or_else(|| file.server.as_ref().and_then(|s| s.port))
            .unwrap_or(8787);
 
        // Bucket: CLI > env > YAML
        let bucket = ov
            .bucket
            .or_else(|| getenv_nonempty("SKIPPR_S3_BUCKET"))
            .or_else(|| file.storage.as_ref().and_then(|s| s.bucket.clone()))
            .ok_or_else(|| "missing storage bucket (set --bucket, env SKIPPR_S3_BUCKET, or storage.bucket in YAML)".to_string())?;
 
        let tenant = ov
            .tenant
            .or_else(|| file.scope.as_ref().and_then(|s| s.tenant.clone()))
            .unwrap_or_else(|| "default".to_string());
        let workspace = ov
            .workspace
            .or_else(|| file.scope.as_ref().and_then(|s| s.workspace.clone()))
            .unwrap_or_else(|| "default".to_string());
        let project_id = ov
            .project_id
            .or_else(|| file.scope.as_ref().and_then(|s| s.project_id.clone()))
            .unwrap_or_else(|| "default".to_string());
 
        ensure_safe_segment("tenant", &tenant)?;
        ensure_safe_segment("workspace", &workspace)?;
        ensure_safe_segment("project_id", &project_id)?;
 
        // Providers (defaults preserve current behavior: all enabled)
        let pf = file.providers.unwrap_or_default();
        let ath_f = pf.athena.unwrap_or_default();
        let cat_f = pf.catalog.unwrap_or_default();
        let dbt_f = pf.dbt.unwrap_or_default();
        let vec_f = pf.vector.unwrap_or_default();
 
        // Athena env surface (preserve current aliases)
        let ath_workgroup = getenv_nonempty("ATHENA_WORKGROUP")
            .or_else(|| getenv_nonempty("DATA_OUTPUT_ATHENA_WORKGROUP_NAME"))
            .or_else(|| ath_f.workgroup);
        let ath_default_db = getenv_nonempty("ATHENA_DEFAULT_DATABASE").or_else(|| ath_f.default_database);
        let ath_catalog = getenv_nonempty("ATHENA_CATALOG").unwrap_or_else(|| ath_f.catalog.unwrap_or_else(|| "AwsDataCatalog".to_string()));
 
        let ath_result_s3 = getenv_nonempty("ATHENA_RESULT_S3").or_else(|| {
            getenv_nonempty("DATA_OUTPUT_ATHENA_RESULTS_S3_BUCKET").map(|b| format!("s3://{}/", b.trim_end_matches('/')))
        }).or_else(|| ath_f.result_s3);
 
        let ttl_env = getenv_nonempty("ATHENA_DISCOVERY_CACHE_TTL_SECS").and_then(|v| v.parse::<u64>().ok());
        let ath_ttl = ttl_env
            .or(ath_f.discovery_cache_ttl_secs)
            .unwrap_or(120)
            .max(5)
            .min(3600);
 
        // LLM env surface
        let llmf = file.llm.unwrap_or_default();
        let llm = LlmResolved {
            provider: getenv_nonempty("LLM_PROVIDER").or(llmf.provider),
            base_url: getenv_nonempty("LLM_BASE_URL").or(llmf.base_url),
            chat_model: getenv_nonempty("LLM_CHAT_MODEL").or(llmf.chat_model),
            embed_model: getenv_nonempty("LLM_EMBED_MODEL").or(llmf.embed_model),
            context_length: getenv_nonempty("LLM_CONTEXT_LENGTH")
                .and_then(|v| v.parse::<usize>().ok())
                .or(llmf.context_length),
            gpu_layers: getenv_nonempty("LLM_GPU_LAYERS")
                .and_then(|v| v.parse::<usize>().ok())
                .or(llmf.gpu_layers),
            http_timeout_secs: getenv_nonempty("LLM_HTTP_TIMEOUT_SECS")
                .and_then(|v| v.parse::<u64>().ok())
                .or(llmf.http_timeout_secs),
            max_tokens: getenv_nonempty("LLM_MAX_TOKENS")
                .and_then(|v| v.parse::<u32>().ok())
                .or(llmf.max_tokens),
            temperature: getenv_nonempty("LLM_TEMPERATURE")
                .and_then(|v| v.parse::<f32>().ok())
                .or(llmf.temperature),
            top_p: getenv_nonempty("LLM_TOP_P")
                .and_then(|v| v.parse::<f32>().ok())
                .or(llmf.top_p),
        };
 
        let cfg = Self {
            server: ServerResolved { port: server_port },
            storage: StorageResolved { bucket: bucket.clone() },
            scope: RequestScope { tenant, workspace, project_id },
            llm,
            providers: ProvidersResolved {
                athena: AthenaResolved {
                    enabled: ath_f.enabled.unwrap_or(true),
                    workgroup: ath_workgroup,
                    result_s3: ath_result_s3,
                    default_database: ath_default_db,
                    catalog: ath_catalog,
                    discovery_cache_ttl_secs: ath_ttl,
                },
                catalog: CatalogResolved {
                    enabled: cat_f.enabled.unwrap_or(true),
                    refresh_secs: cat_f.refresh_secs.unwrap_or(60),
                    max_concurrency: cat_f.max_concurrency.unwrap_or(8),
                },
                dbt: DbtResolved {
                    enabled: dbt_f.enabled.unwrap_or(true),
                    profiles_dir: getenv_nonempty("DBT_PROFILES_DIR").or(dbt_f.profiles_dir),
                    target: getenv_nonempty("DBT_TARGET").or(dbt_f.target),
                    runner: getenv_nonempty("DBT_RUNNER").or(dbt_f.runner).unwrap_or_else(|| "host".to_string()),
                    docker_image: getenv_nonempty("DBT_DOCKER_IMAGE").or(dbt_f.docker_image),
                    docker_platform: getenv_nonempty("DBT_DOCKER_PLATFORM").or(dbt_f.docker_platform),
                    docker_network: getenv_nonempty("DBT_DOCKER_NETWORK").or(dbt_f.docker_network),
                    docker_mount_aws_dir: getenv_nonempty("DBT_DOCKER_MOUNT_AWS_DIR")
                        .map(|v| {
                            let vv = v.trim().to_lowercase();
                            vv == "1" || vv == "true" || vv == "yes"
                        })
                        .or(dbt_f.docker_mount_aws_dir)
                        .unwrap_or(false),
                },
                vector: VectorResolved {
                    enabled: vec_f.enabled.unwrap_or(true),
                },
            },
        };
 
        // Fill env defaults for subsystems that still read env internally.
        // IMPORTANT: we never overwrite an explicitly set env var.
        if let Some(v) = cfg.llm.provider.as_ref() { set_env_if_unset("LLM_PROVIDER", v); }
        if let Some(v) = cfg.llm.base_url.as_ref() { set_env_if_unset("LLM_BASE_URL", v); }
        if let Some(v) = cfg.llm.chat_model.as_ref() { set_env_if_unset("LLM_CHAT_MODEL", v); }
        if let Some(v) = cfg.llm.embed_model.as_ref() { set_env_if_unset("LLM_EMBED_MODEL", v); }
        if let Some(v) = cfg.llm.context_length.as_ref() { set_env_if_unset("LLM_CONTEXT_LENGTH", &v.to_string()); }
        if let Some(v) = cfg.llm.gpu_layers.as_ref() { set_env_if_unset("LLM_GPU_LAYERS", &v.to_string()); }
        if let Some(v) = cfg.llm.http_timeout_secs.as_ref() { set_env_if_unset("LLM_HTTP_TIMEOUT_SECS", &v.to_string()); }
        if let Some(v) = cfg.llm.max_tokens.as_ref() { set_env_if_unset("LLM_MAX_TOKENS", &v.to_string()); }
        if let Some(v) = cfg.llm.temperature.as_ref() { set_env_if_unset("LLM_TEMPERATURE", &v.to_string()); }
        if let Some(v) = cfg.llm.top_p.as_ref() { set_env_if_unset("LLM_TOP_P", &v.to_string()); }
 
        if let Some(v) = cfg.providers.dbt.profiles_dir.as_ref() { set_env_if_unset("DBT_PROFILES_DIR", v); }
        if let Some(v) = cfg.providers.dbt.target.as_ref() { set_env_if_unset("DBT_TARGET", v); }
        set_env_if_unset("DBT_RUNNER", &cfg.providers.dbt.runner);
        if let Some(v) = cfg.providers.dbt.docker_image.as_ref() { set_env_if_unset("DBT_DOCKER_IMAGE", v); }
        if let Some(v) = cfg.providers.dbt.docker_platform.as_ref() { set_env_if_unset("DBT_DOCKER_PLATFORM", v); }
        if let Some(v) = cfg.providers.dbt.docker_network.as_ref() { set_env_if_unset("DBT_DOCKER_NETWORK", v); }
        set_env_if_unset("DBT_DOCKER_MOUNT_AWS_DIR", if cfg.providers.dbt.docker_mount_aws_dir { "true" } else { "false" });
 
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use once_cell::sync::Lazy;
    use std::sync::Mutex;

    // Env vars are process-global; tests run in parallel by default.
    // Serialize env-dependent tests to avoid flakes.
    static ENV_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

    fn clear_env(keys: &[&str]) {
        for k in keys {
            std::env::remove_var(k);
        }
    }

    #[test]
    fn resolve_bucket_precedence_cli_over_env_over_yaml() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env(&["SKIPPR_S3_BUCKET"]);
        std::env::set_var("SKIPPR_S3_BUCKET", "env-bucket");

        let file = ReactConfigFile {
            storage: Some(StorageFile { bucket: Some("yaml-bucket".into()) }),
            ..Default::default()
        };
        let ov = ServeOverrides { bucket: Some("cli-bucket".into()), ..Default::default() };

        let cfg = ReactResolvedConfig::resolve(file, ov).expect("resolve");
        assert_eq!(cfg.storage.bucket, "cli-bucket");

        clear_env(&["SKIPPR_S3_BUCKET"]);
    }

    #[test]
    fn resolve_bucket_errors_if_missing_everywhere() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env(&["SKIPPR_S3_BUCKET"]);
        let file = ReactConfigFile::default();
        let ov = ServeOverrides::default();
        let err = ReactResolvedConfig::resolve(file, ov).err().unwrap_or_default();
        assert!(err.contains("missing storage bucket"));
    }

    #[test]
    fn resolve_rejects_unsafe_scope_segments() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env(&["SKIPPR_S3_BUCKET"]);
        let file = ReactConfigFile {
            storage: Some(StorageFile { bucket: Some("b".into()) }),
            scope: Some(ScopeFile { tenant: Some("a/b".into()), workspace: Some("w".into()), project_id: Some("p".into()) }),
            ..Default::default()
        };
        let err = ReactResolvedConfig::resolve(file, ServeOverrides::default()).err().unwrap_or_default();
        assert!(err.contains("must not contain path separators"));
    }
}
 
