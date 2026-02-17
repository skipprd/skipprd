use serde::Deserialize;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use crate::providers::RequestScope;
use react_core::providers::DEFAULT_WAREHOUSE_MAX_CONCURRENCY;

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
///   # Local-first (default)
///   mode: local
///   path: ./.react
///   # For S3:
///   # mode: s3
///   # bucket: my-react-bucket
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
///     # Athena Data Catalog (Glue).
///     target_catalog: AwsDataCatalog
///     # Bronze/raw schema (Glue database) for discovery + dbt sources.
///     source_schema: raw
///     result_s3: s3://my-query-results/
///     discovery_cache_ttl_secs: 120
///   catalog:
///     enabled: true
///     refresh_secs: 60
///     max_concurrency: 8
///   dbt:
///     enabled: true
///     runner: host
///     target: athena
///     naming:
///       # dbt target.schema (base) and tier suffixes. Example schemas: raw=test_raw, silver=test_silver, gold=test_warehouse
///       target_schema: test
///       silver_suffix: silver
///       gold_suffix: warehouse
///   vector:
///     enabled: true
/// ```
///
/// Notes:
/// - Secrets remain env-driven (e.g. `LLM_API_KEY`).
/// - Local-first: `storage.mode` defaults to `local` (stores artifacts under `storage.path`).
/// - For S3: set `storage.mode: s3` and provide `storage.bucket` (or env `SKIPPR_S3_BUCKET` / CLI `--bucket`).

/// CLI overrides for `react serve`.
///
/// Any `Some` value takes precedence over env and file config.
#[derive(Clone, Debug, Default)]
pub struct ServeOverrides {
    pub port: Option<u16>,
    pub storage_mode: Option<String>,
    pub bucket: Option<String>,
    pub storage_path: Option<String>,
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
    pub mode: Option<String>,
    pub bucket: Option<String>,
    pub path: Option<String>,
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
    pub warehouse: Option<WarehouseFile>,
    pub catalog: Option<CatalogFile>,
    pub dbt: Option<DbtFile>,
    pub vector: Option<VectorFile>,
}

/// Warehouse configuration for a single provider (source or target).
///
/// Hard-cutover: no legacy aliases. Keep secrets in env, only non-secret wiring here.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WarehouseFile {
    /// AWS Athena + Glue.
    Athena {
        workgroup: Option<String>,
        region: Option<String>,
        result_s3: Option<String>,
        max_concurrency: Option<usize>,
        /// Container above schema: Athena catalog (Glue Data Catalog). Default: AwsDataCatalog.
        catalog: Option<String>,
        /// Optional default schema/database for discovery + unqualified queries.
        schema: Option<String>,
        discovery_cache_ttl_secs: Option<u64>,
    },
    /// PostgreSQL.
    Postgres {
        /// Optional default database name (container above schema).
        database: Option<String>,
        /// Optional default schema for discovery/unqualified references.
        schema: Option<String>,
    },
    /// Microsoft SQL Server.
    Mssql {
        database: Option<String>,
        schema: Option<String>,
    },
    /// Snowflake (warehouse database+schema live in Snowflake).
    Snowflake {
        database: Option<String>,
        schema: Option<String>,
        warehouse: Option<String>,
        role: Option<String>,
    },
    /// BigQuery (project+dataset+table).
    Bigquery {
        project: Option<String>,
        dataset: Option<String>,
        location: Option<String>,
        max_concurrency: Option<usize>,
        discovery_cache_ttl_secs: Option<u64>,
    },
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct CatalogFile {
    pub enabled: Option<bool>,
    pub refresh_secs: Option<u64>,
    pub max_concurrency: Option<usize>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct DbtNamingFile {
    pub target_schema: Option<String>,
    pub silver_suffix: Option<String>,
    pub gold_suffix: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct DbtFile {
    pub enabled: Option<bool>,
    pub profiles_dir: Option<String>,
    pub target: Option<String>,
    pub naming: Option<DbtNamingFile>,
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
    pub mode: String,              // local|s3
    pub bucket: Option<String>,     // required for s3
    pub path: Option<String>,       // required for local (absolute)
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
    pub warehouse: WarehouseResolved,
    pub catalog: CatalogResolved,
    pub dbt: DbtResolved,
    pub vector: VectorResolved,
}

#[derive(Clone, Debug)]
pub struct WarehouseResolved {
    pub kind: String, // snake_case kind
    /// Provider-specific container name (catalog/project/database) if applicable.
    pub container: Option<String>,
    /// Provider-specific default namespace (schema/dataset) if applicable.
    pub namespace: Option<String>,
    /// Provider-specific additional parameters (non-secret).
    pub extras: serde_json::Value,
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
    pub naming: DbtNamingResolved,
    pub runner: String,
    pub docker_image: Option<String>,
    pub docker_platform: Option<String>,
    pub docker_network: Option<String>,
    pub docker_mount_aws_dir: bool,
}

#[derive(Clone, Debug, Default)]
pub struct DbtNamingResolved {
    pub target_schema: Option<String>,
    pub silver_suffix: Option<String>,
    pub gold_suffix: Option<String>,
}

#[derive(Clone, Debug)]
pub struct VectorResolved {
    pub enabled: bool,
}

fn getenv_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().and_then(|v| {
        let t = v.trim();
        if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
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

        // Storage mode: CLI > env > YAML > default(local)
        let mode = ov
            .storage_mode
            .or_else(|| getenv_nonempty("REACT_STORAGE_MODE"))
            .or_else(|| file.storage.as_ref().and_then(|s| s.mode.clone()))
            .unwrap_or_else(|| "local".to_string())
            .trim()
            .to_ascii_lowercase();
        let mode = match mode.as_str() {
            "local" => "local".to_string(),
            "s3" => "s3".to_string(),
            other => return Err(format!("unsupported storage.mode '{other}' (expected local|s3)")),
        };

        fn abs_path(p: &str) -> Result<String, String> {
            let t = p.trim();
            if t.is_empty() {
                return Err("empty storage.path".to_string());
            }
            let pb = PathBuf::from(t);
            if pb.is_absolute() {
                return Ok(pb.to_string_lossy().to_string());
            }
            let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
            Ok(cwd.join(pb).to_string_lossy().to_string())
        }

        // Resolve storage fields based on mode.
        let (bucket, path) = if mode == "s3" {
            // Bucket: CLI > env > YAML
            let b = ov
                .bucket
                .or_else(|| getenv_nonempty("SKIPPR_S3_BUCKET"))
                .or_else(|| file.storage.as_ref().and_then(|s| s.bucket.clone()))
                .ok_or_else(|| {
                    "missing storage bucket for s3 mode (set --bucket, env SKIPPR_S3_BUCKET, or storage.bucket in YAML)".to_string()
                })?;
            (Some(b), None)
        } else {
            // Path: CLI > env > YAML > default(./.react)
            let p = ov
                .storage_path
                .or_else(|| getenv_nonempty("REACT_STORAGE_PATH"))
                .or_else(|| file.storage.as_ref().and_then(|s| s.path.clone()))
                .unwrap_or_else(|| "./.react".to_string());
            (None, Some(abs_path(&p)?))
        };

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

        // Providers
        let pf = file.providers.unwrap_or_default();
        let wh_f = pf
            .warehouse
            .ok_or_else(|| "missing providers.warehouse in YAML config".to_string())?;
        let cat_f = pf.catalog.unwrap_or_default();
        let dbt_f = pf.dbt.unwrap_or_default();
        let vec_f = pf.vector.unwrap_or_default();

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

        // DBT naming (suffix strategy). If configured, dbt will materialize schemas like:
        //   <target_schema>_<silver_suffix> and <target_schema>_<gold_suffix>
        let dbt_naming_f = dbt_f.naming.clone().unwrap_or_default();
        let naming_target_schema =
            getenv_nonempty("DBT_TARGET_SCHEMA").or(dbt_naming_f.target_schema);
        let naming_silver_suffix = getenv_nonempty("DBT_SILVER_SUFFIX")
            .or(dbt_naming_f.silver_suffix)
            .or(Some("silver".to_string()));
        let naming_gold_suffix = getenv_nonempty("DBT_GOLD_SUFFIX")
            .or(dbt_naming_f.gold_suffix)
            .or(Some("warehouse".to_string()));

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
                    kind: "athena".to_string(),
                    container: Some(catalog.unwrap_or_else(|| "AwsDataCatalog".to_string())),
                    namespace: schema,
                    extras: serde_json::json!({
                        "workgroup": workgroup,
                        "region": region,
                        "result_s3": result_s3,
                        "max_concurrency": max_concurrency,
                        "discovery_cache_ttl_secs": discovery_cache_ttl_secs,
                    }),
                },
                WarehouseFile::Postgres { database, schema } => WarehouseResolved {
                    kind: "postgres".to_string(),
                    container: database,
                    namespace: schema,
                    extras: serde_json::json!({}),
                },
                WarehouseFile::Mssql { database, schema } => WarehouseResolved {
                    kind: "mssql".to_string(),
                    container: database,
                    namespace: schema,
                    extras: serde_json::json!({}),
                },
                WarehouseFile::Snowflake {
                    database,
                    schema,
                    warehouse,
                    role,
                } => WarehouseResolved {
                    kind: "snowflake".to_string(),
                    container: database,
                    namespace: schema,
                    extras: serde_json::json!({ "warehouse": warehouse, "role": role }),
                },
                WarehouseFile::Bigquery {
                    project,
                    dataset,
                    location,
                    max_concurrency,
                    discovery_cache_ttl_secs,
                } => WarehouseResolved {
                    kind: "bigquery".to_string(),
                    container: project,
                    namespace: dataset,
                    extras: serde_json::json!({
                        "location": location,
                        "max_concurrency": max_concurrency,
                        "discovery_cache_ttl_secs": discovery_cache_ttl_secs,
                    }),
                },
            }
        }

        let cfg = Self {
            server: ServerResolved { port: server_port },
            storage: StorageResolved {
                mode: mode.clone(),
                bucket: bucket.clone(),
                path: path.clone(),
            },
            scope: RequestScope {
                tenant,
                workspace,
                project_id,
            },
            llm,
            providers: ProvidersResolved {
                warehouse: resolve_warehouse(wh_f),
                catalog: CatalogResolved {
                    enabled: cat_f.enabled.unwrap_or(true),
                    refresh_secs: cat_f.refresh_secs.unwrap_or(60),
                    max_concurrency: cat_f.max_concurrency.unwrap_or(8),
                },
                dbt: DbtResolved {
                    enabled: dbt_f.enabled.unwrap_or(true),
                    profiles_dir: getenv_nonempty("DBT_PROFILES_DIR").or(dbt_f.profiles_dir),
                    target: getenv_nonempty("DBT_TARGET").or(dbt_f.target),
                    naming: DbtNamingResolved {
                        target_schema: naming_target_schema,
                        silver_suffix: naming_silver_suffix,
                        gold_suffix: naming_gold_suffix,
                    },
                    runner: getenv_nonempty("DBT_RUNNER")
                        .or(dbt_f.runner)
                        .unwrap_or_else(|| "host".to_string()),
                    docker_image: getenv_nonempty("DBT_DOCKER_IMAGE").or(dbt_f.docker_image),
                    docker_platform: getenv_nonempty("DBT_DOCKER_PLATFORM")
                        .or(dbt_f.docker_platform),
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
        if let Some(v) = cfg.llm.provider.as_ref() {
            set_env_if_unset("LLM_PROVIDER", v);
        }
        if let Some(v) = cfg.llm.base_url.as_ref() {
            set_env_if_unset("LLM_BASE_URL", v);
        }
        if let Some(v) = cfg.llm.chat_model.as_ref() {
            set_env_if_unset("LLM_CHAT_MODEL", v);
        }
        if let Some(v) = cfg.llm.embed_model.as_ref() {
            set_env_if_unset("LLM_EMBED_MODEL", v);
        }
        if let Some(v) = cfg.llm.context_length.as_ref() {
            set_env_if_unset("LLM_CONTEXT_LENGTH", &v.to_string());
        }
        if let Some(v) = cfg.llm.gpu_layers.as_ref() {
            set_env_if_unset("LLM_GPU_LAYERS", &v.to_string());
        }
        if let Some(v) = cfg.llm.http_timeout_secs.as_ref() {
            set_env_if_unset("LLM_HTTP_TIMEOUT_SECS", &v.to_string());
        }
        if let Some(v) = cfg.llm.max_tokens.as_ref() {
            set_env_if_unset("LLM_MAX_TOKENS", &v.to_string());
        }
        if let Some(v) = cfg.llm.temperature.as_ref() {
            set_env_if_unset("LLM_TEMPERATURE", &v.to_string());
        }
        if let Some(v) = cfg.llm.top_p.as_ref() {
            set_env_if_unset("LLM_TOP_P", &v.to_string());
        }

        if let Some(v) = cfg.providers.dbt.profiles_dir.as_ref() {
            set_env_if_unset("DBT_PROFILES_DIR", v);
        }
        if let Some(v) = cfg.providers.dbt.target.as_ref() {
            set_env_if_unset("DBT_TARGET", v);
        }
        // DBT naming env defaults (portable)
        if let Some(v) = cfg.providers.dbt.naming.target_schema.as_ref() {
            set_env_if_unset("DBT_TARGET_SCHEMA", v);
        }
        if let Some(v) = cfg.providers.dbt.naming.silver_suffix.as_ref() {
            set_env_if_unset("DBT_SILVER_SUFFIX", v);
        }
        if let Some(v) = cfg.providers.dbt.naming.gold_suffix.as_ref() {
            set_env_if_unset("DBT_GOLD_SUFFIX", v);
        }
        set_env_if_unset("DBT_RUNNER", &cfg.providers.dbt.runner);
        if let Some(v) = cfg.providers.dbt.docker_image.as_ref() {
            set_env_if_unset("DBT_DOCKER_IMAGE", v);
        }
        if let Some(v) = cfg.providers.dbt.docker_platform.as_ref() {
            set_env_if_unset("DBT_DOCKER_PLATFORM", v);
        }
        if let Some(v) = cfg.providers.dbt.docker_network.as_ref() {
            set_env_if_unset("DBT_DOCKER_NETWORK", v);
        }
        set_env_if_unset(
            "DBT_DOCKER_MOUNT_AWS_DIR",
            if cfg.providers.dbt.docker_mount_aws_dir {
                "true"
            } else {
                "false"
            },
        );

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
            storage: Some(StorageFile {
                mode: Some("s3".into()),
                bucket: Some("yaml-bucket".into()),
                path: None,
            }),
            providers: Some(ProvidersFile {
                warehouse: Some(WarehouseFile::Postgres {
                    database: None,
                    schema: None,
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ov = ServeOverrides {
            storage_mode: Some("s3".into()),
            bucket: Some("cli-bucket".into()),
            ..Default::default()
        };

        let cfg = ReactResolvedConfig::resolve(file, ov).expect("resolve");
        assert_eq!(cfg.storage.mode, "s3");
        assert_eq!(cfg.storage.bucket, Some("cli-bucket".to_string()));

        clear_env(&["SKIPPR_S3_BUCKET"]);
    }

    #[test]
    fn resolve_bucket_errors_if_missing_everywhere() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env(&["SKIPPR_S3_BUCKET"]);
        let file = ReactConfigFile {
            providers: Some(ProvidersFile {
                warehouse: Some(WarehouseFile::Postgres {
                    database: None,
                    schema: None,
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ov = ServeOverrides {
            storage_mode: Some("s3".into()),
            ..Default::default()
        };
        let err = ReactResolvedConfig::resolve(file, ov)
            .err()
            .unwrap_or_default();
        assert!(err.contains("missing storage bucket for s3 mode"));
    }

    #[test]
    fn resolve_rejects_unsafe_scope_segments() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env(&["SKIPPR_S3_BUCKET"]);
        let file = ReactConfigFile {
            storage: Some(StorageFile {
                mode: Some("s3".into()),
                bucket: Some("b".into()),
                path: None,
            }),
            scope: Some(ScopeFile {
                tenant: Some("a/b".into()),
                workspace: Some("w".into()),
                project_id: Some("p".into()),
            }),
            providers: Some(ProvidersFile {
                warehouse: Some(WarehouseFile::Postgres {
                    database: None,
                    schema: None,
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let err = ReactResolvedConfig::resolve(file, ServeOverrides::default())
            .err()
            .unwrap_or_default();
        assert!(err.contains("must not contain path separators"));
    }

    #[test]
    fn resolve_defaults_to_local_storage_without_bucket() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env(&["SKIPPR_S3_BUCKET", "REACT_STORAGE_MODE", "REACT_STORAGE_PATH"]);
        let file = ReactConfigFile {
            providers: Some(ProvidersFile {
                warehouse: Some(WarehouseFile::Postgres {
                    database: None,
                    schema: None,
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let cfg = ReactResolvedConfig::resolve(file, ServeOverrides::default()).expect("resolve");
        assert_eq!(cfg.storage.mode, "local");
        assert!(cfg.storage.bucket.is_none());
        assert!(cfg.storage.path.as_ref().is_some());
    }
}
