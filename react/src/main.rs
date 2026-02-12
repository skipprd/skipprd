use std::sync::Arc;

use clap::{Parser, Subcommand};
use std::path::Path;
use std::path::PathBuf;

use react::adapters::storage::{LocalFileStorageAdapter, S3StorageAdapter};
use react::llm;
use react::providers::catalog::DefaultCatalogProvider;
use react::providers::dbt::DbtRunnerConfig;
use react::providers::{
    AthenaQueryProvider, AthenaSettings, BigQueryProvider, BigQuerySettings, PostgresProvider,
    PostgresSettings,
};
use react::providers::{
    DbtProjectProvider, DefaultKeyspace, EnvSecretsProvider, LanceVectorStore, LocalKeyspace,
};
use react_suites::SuiteCtx;
use tracing_subscriber::prelude::*;

#[derive(Parser, Debug)]
#[command(name = "react")]
struct Cli {
    /// Enable logging (defaults to `info` when present). Respects `RUST_LOG` if set.
    #[arg(long, num_args = 0..=1, default_missing_value = "info")]
    log: Option<String>,

    /// Render a live terminal UI (phases/tasks/tools) instead of relying on logs.
    ///
    /// - Requires a TTY stdout.
    /// - Press `q` to close the UI (server keeps running).
    #[arg(long, global = true, default_value_t = false)]
    terminal: bool,

    /// Include very verbose AWS S3/Smithy HTTP logs when using `--log debug` / `--log trace`.
    ///
    /// By default, `--log debug` suppresses noisy AWS request/response logging to keep output readable.
    #[arg(long, default_value_t = false)]
    verbose_debug: bool,

    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start the ReAct WebSocket server.
    Serve {
        /// Path to YAML config file.
        #[arg(long, value_name = "PATH")]
        config: String,

        /// WebSocket port to listen on.
        #[arg(long)]
        port: Option<u16>,

        /// Storage mode for artifacts/threads/catalog/vectors.
        ///
        /// - `local` stores under `--storage-path` (default).
        /// - `s3` stores in an S3 bucket (requires `--bucket` or env `SKIPPR_S3_BUCKET`).
        #[arg(long, value_name = "MODE")]
        storage_mode: Option<String>,

        /// Local storage root directory (only used when `--storage-mode local`).
        #[arg(long, value_name = "PATH")]
        storage_path: Option<String>,

        /// S3 bucket for ReAct artifacts (only used when `--storage-mode s3`).
        /// Defaults to env `SKIPPR_S3_BUCKET` if set.
        #[arg(long)]
        bucket: Option<String>,

        /// Tenant scope (artifact partition).
        #[arg(long)]
        tenant: Option<String>,

        /// Workspace scope (artifact partition).
        #[arg(long)]
        workspace: Option<String>,

        /// ReAct project identifier (artifact partition).
        #[arg(long)]
        project_id: Option<String>,
    },

    /// Run a headless thread in the terminal (no WebSocket server).
    ///
    /// - If `--thread-id` is provided, the thread must exist and the initial prompt is `continue`.
    /// - Otherwise a new thread is created with initial prompt `go`.
    Run {
        /// Path to YAML config file.
        #[arg(long, value_name = "PATH")]
        config: String,

        /// Existing thread id to continue.
        #[arg(long)]
        thread_id: Option<String>,

        /// Suite to run (defaults to data_engineer).
        #[arg(long, default_value = "data_engineer")]
        suite_id: String,

        /// Agent type to run (defaults to agent).
        #[arg(long, default_value = "agent")]
        agent: String,

        /// Storage mode for artifacts/threads/catalog/vectors.
        #[arg(long, value_name = "MODE")]
        storage_mode: Option<String>,

        /// Local storage root directory (only used when `--storage-mode local`).
        #[arg(long, value_name = "PATH")]
        storage_path: Option<String>,

        /// S3 bucket for ReAct artifacts (only used when `--storage-mode s3`).
        /// Defaults to env `SKIPPR_S3_BUCKET` if set.
        #[arg(long)]
        bucket: Option<String>,

        /// Tenant scope (artifact partition).
        #[arg(long)]
        tenant: Option<String>,

        /// Workspace scope (artifact partition).
        #[arg(long)]
        workspace: Option<String>,

        /// ReAct project identifier (artifact partition).
        #[arg(long)]
        project_id: Option<String>,
    },
}

fn init_logging(log: &Option<String>, verbose_debug: bool) {
    // Always ensure we have some log level for file logs.
    // If the user didn't ask for logging and didn't set RUST_LOG, default to info.
    if std::env::var("RUST_LOG")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .is_none()
    {
        let level = log.as_deref().unwrap_or("info");
        // Keep `debug` useful by default: suppress very noisy AWS SDK (S3/STS/Athena/Glue)
        // and Smithy HTTP logs unless opted in.
        if (level == "debug" || level == "trace") && !verbose_debug {
            let quiet = format!(
                "{level},\
aws_smithy_http=info,\
aws_smithy_http_tower=info,\
aws_smithy_runtime=info,\
aws_sdk_s3=info,\
aws_sdk_sts=info,\
aws_sdk_athena=info,\
aws_sdk_glue=info,\
aws_config=info,\
tokio_tungstenite=info,\
tungstenite=info,\
lance=info,\
lance_core=info,\
lance_io=info,\
lance_table=info,\
react::ws=info,\
react::vector=info,\
h2=info,\
rustls=info,\
tokio_rustls=info,\
hyper_rustls=info,\
hyper=info,\
reqwest=info",
                level = level
            );
            std::env::set_var("RUST_LOG", quiet);
        } else {
            std::env::set_var("RUST_LOG", level);
        }
    }
}

fn resolve_log_dir(cfg: &react::config::ReactResolvedConfig) -> PathBuf {
    if let Ok(v) = std::env::var("REACT_LOG_DIR") {
        let p = v.trim();
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    if cfg.storage.mode == "local" {
        if let Some(ref root) = cfg.storage.path {
            return PathBuf::from(root).join("logs");
        }
    }
    PathBuf::from("./.react/logs")
}

fn init_tracing(log_dir: &Path, enable_console: bool) -> tracing_appender::non_blocking::WorkerGuard {
    let _ = std::fs::create_dir_all(log_dir);
    let appender = tracing_appender::rolling::daily(log_dir, "react.log");
    let (nb, guard) = tracing_appender::non_blocking(appender);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(nb)
        .with_target(true);

    if enable_console {
        let console_layer = tracing_subscriber::fmt::layer()
            .with_ansi(true)
            .with_target(true);
        let _ = tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(file_layer)
            .with(console_layer)
            .try_init();
    } else {
        let _ = tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(file_layer)
            .try_init();
    }
    guard
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.cmd {
        Command::Serve {
            config,
            port,
            storage_mode,
            storage_path,
            bucket,
            tenant,
            workspace,
            project_id,
        } => {
            init_logging(&cli.log, cli.verbose_debug);
            let file_cfg = match react::config::ReactConfigFile::load_yaml(Path::new(&config)) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("ERROR: {}", e);
                    std::process::exit(1);
                }
            };
            let cfg = match react::config::ReactResolvedConfig::resolve(
                file_cfg,
                react::config::ServeOverrides {
                    port,
                    storage_mode,
                    bucket,
                    storage_path,
                    tenant,
                    workspace,
                    project_id,
                },
            ) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("ERROR: {}", e);
                    std::process::exit(1);
                }
            };

            let log_dir = resolve_log_dir(&cfg);
            let enable_console = cli.log.is_some() && !cli.terminal;
            let _guard = init_tracing(&log_dir, enable_console);

            if cli.terminal {
                // Terminal mode is intended to be fully headless: auto-answer any `await_user` prompts.
                // Allow override by explicitly setting env var beforehand.
                if std::env::var("REACT_HEADLESS")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                    .is_none()
                {
                    std::env::set_var("REACT_HEADLESS", "1");
                }
                if let Err(e) = react::ws::terminal::init() {
                    eprintln!("WARN: terminal mode not enabled: {}", e);
                }
            }

            let (storage, keyspace, suite_bucket) = if cfg.storage.mode == "local" {
                let root = cfg
                    .storage
                    .path
                    .clone()
                    .ok_or_else(|| "internal: missing storage.path for local mode".to_string())
                    .unwrap_or_else(|e| {
                        eprintln!("ERROR: {}", e);
                        std::process::exit(1);
                    });
                let storage = match LocalFileStorageAdapter::new(root.clone()) {
                    Ok(s) => Arc::new(s) as Arc<dyn react::adapters::storage::StorageAdapter>,
                    Err(e) => {
                        eprintln!("ERROR: {}", e);
                        std::process::exit(1);
                    }
                };
                let keyspace = Arc::new(LocalKeyspace::new(root));
                (storage, keyspace as Arc<dyn react::providers::Keyspace>, "local".to_string())
            } else {
                let b = cfg
                    .storage
                    .bucket
                    .clone()
                    .ok_or_else(|| "internal: missing storage.bucket for s3 mode".to_string())
                    .unwrap_or_else(|e| {
                        eprintln!("ERROR: {}", e);
                        std::process::exit(1);
                    });
                let storage = Arc::new(S3StorageAdapter::from_env(b.clone()).await)
                    as Arc<dyn react::adapters::storage::StorageAdapter>;
                let keyspace = Arc::new(DefaultKeyspace::new(b.clone()));
                (storage, keyspace as Arc<dyn react::providers::Keyspace>, b)
            };
            let secrets = Arc::new(EnvSecretsProvider::default());
            let llm = llm::create_llm(&llm::config_from_resolved(&cfg));

            let mut suite_ctx =
                SuiteCtx::new(storage, secrets, llm, cfg.scope.clone(), keyspace.clone());
            suite_ctx.resolved_config = Some(Arc::new(react_suites::ReactResolvedConfig {
                server: react_suites::config::ServerResolved {
                    port: cfg.server.port,
                },
                storage: react_suites::config::StorageResolved {
                    bucket: suite_bucket.clone(),
                },
                scope: cfg.scope.clone(),
                llm: react_suites::config::LlmResolved::default(),
                providers: react_suites::config::ProvidersResolved {
                    warehouse: react_suites::config::WarehouseResolved {
                        kind: cfg.providers.warehouse.kind.clone(),
                        container: cfg
                            .providers
                            .warehouse
                            .container
                            .clone()
                            .unwrap_or_default(),
                        namespace: cfg
                            .providers
                            .warehouse
                            .namespace
                            .clone()
                            .unwrap_or_default(),
                        extras: cfg.providers.warehouse.extras.clone(),
                    },
                    catalog: react_suites::config::CatalogResolved {
                        enabled: cfg.providers.catalog.enabled,
                        refresh_secs: cfg.providers.catalog.refresh_secs,
                        max_concurrency: cfg.providers.catalog.max_concurrency,
                    },
                    dbt: react_suites::config::DbtResolved {
                        enabled: cfg.providers.dbt.enabled,
                        profiles_dir: cfg.providers.dbt.profiles_dir.clone(),
                        target: cfg.providers.dbt.target.clone().unwrap_or_default(),
                        naming: react_suites::config::DbtNamingResolved {
                            target_schema: cfg
                                .providers
                                .dbt
                                .naming
                                .target_schema
                                .clone()
                                .unwrap_or_default(),
                            silver_suffix: cfg
                                .providers
                                .dbt
                                .naming
                                .silver_suffix
                                .clone()
                                .unwrap_or_default(),
                            gold_suffix: cfg
                                .providers
                                .dbt
                                .naming
                                .gold_suffix
                                .clone()
                                .unwrap_or_default(),
                        },
                        runner: cfg.providers.dbt.runner.clone(),
                        docker_image: cfg.providers.dbt.docker_image.clone(),
                        docker_platform: cfg.providers.dbt.docker_platform.clone(),
                        docker_network: cfg.providers.dbt.docker_network.clone(),
                        docker_mount_aws_dir: cfg.providers.dbt.docker_mount_aws_dir,
                    },
                    vector: react_suites::config::VectorResolved {
                        enabled: cfg.providers.vector.enabled,
                    },
                },
            }));

            // Warehouse provider (single provider; dbt target)
            let wh_kind = cfg.providers.warehouse.kind.trim().to_ascii_lowercase();
            if wh_kind == "athena" {
                let extras = &cfg.providers.warehouse.extras;
                let workgroup = extras
                    .get("workgroup")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let result_s3 = extras
                    .get("result_s3")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let max_conc = extras
                    .get("max_concurrency")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize)
                    .unwrap_or(15);
                let ttl = extras
                    .get("discovery_cache_ttl_secs")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(120);
                let catalog = cfg
                    .providers
                    .warehouse
                    .container
                    .clone()
                    .unwrap_or_else(|| "AwsDataCatalog".to_string());
                let schema = cfg.providers.warehouse.namespace.clone();
                let athena = Arc::new(
                    AthenaQueryProvider::from_settings(AthenaSettings {
                        workgroup,
                        result_output_location: result_s3,
                        default_catalog: catalog,
                        source_schema: schema,
                        max_concurrency: max_conc,
                        discovery_cache_ttl_secs: ttl,
                    })
                    .await,
                );
                suite_ctx.warehouse = athena.clone();
                // Keep these set for now (some older call sites still use them), but suites should prefer `warehouse`.
                suite_ctx.query = Some(athena.clone());
                suite_ctx.datasets = Some(athena.clone());
            } else if wh_kind == "postgres" {
                let dbname = cfg.providers.warehouse.container.clone();
                let default_schema = cfg.providers.warehouse.namespace.clone();
                let pg = Arc::new(PostgresProvider::from_settings(PostgresSettings {
                    dbname,
                    default_schema,
                    ..Default::default()
                }));
                suite_ctx.warehouse = pg.clone();
                suite_ctx.query = Some(pg.clone());
                suite_ctx.datasets = Some(pg.clone());
            } else if wh_kind == "bigquery" {
                let project = cfg.providers.warehouse.container.clone();
                let dataset = cfg.providers.warehouse.namespace.clone();
                let location = cfg
                    .providers
                    .warehouse
                    .extras
                    .get("location")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let max_conc = cfg
                    .providers
                    .warehouse
                    .extras
                    .get("max_concurrency")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize)
                    .unwrap_or(15);
                let ttl_secs = cfg
                    .providers
                    .warehouse
                    .extras
                    .get("discovery_cache_ttl_secs")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(120);
                let bq = Arc::new(
                    BigQueryProvider::from_settings(BigQuerySettings {
                        project,
                        dataset,
                        location,
                        max_concurrency: max_conc,
                        discovery_cache_ttl_secs: ttl_secs,
                    })
                    .await
                    .map_err(|e| {
                        eprintln!("ERROR: BigQuery provider init failed: {}", e);
                        std::process::exit(1);
                    })
                    .unwrap(),
                );
                suite_ctx.warehouse = bq.clone();
                suite_ctx.query = Some(bq.clone());
                suite_ctx.datasets = Some(bq.clone());
            } else {
                eprintln!(
                    "ERROR: unsupported providers.warehouse.kind '{}'",
                    cfg.providers.warehouse.kind
                );
                std::process::exit(1);
            }

            // Catalog provider (storage-backed), optional but strongly recommended for UX and speed.
            if cfg.providers.catalog.enabled {
                let cat = Arc::new(DefaultCatalogProvider::new(
                    suite_ctx.storage.clone(),
                    keyspace.clone(),
                    suite_ctx.llm.clone(),
                    30,
                    6,
                ));
                suite_ctx.catalog = Some(cat);
            }

            // Vector store (LanceDB on S3), optional but enables dataset/artifact search.
            if cfg.providers.vector.enabled {
                suite_ctx.vector = Some(Arc::new(LanceVectorStore::new(
                    keyspace.clone(),
                    cfg.scope.clone(),
                )));
            }

            // DBT provider (host/docker runner), required for dbt_validate/build/publish workflows.
            if cfg.providers.dbt.enabled {
                let runner = DbtRunnerConfig {
                    mode: cfg.providers.dbt.runner.clone(),
                    docker_image: cfg.providers.dbt.docker_image.clone(),
                    docker_platform: cfg.providers.dbt.docker_platform.clone(),
                    docker_network: cfg.providers.dbt.docker_network.clone(),
                    docker_mount_aws_dir: cfg.providers.dbt.docker_mount_aws_dir,
                };
                suite_ctx.dbt = Some(Arc::new(DbtProjectProvider::new(
                    suite_ctx.storage.clone(),
                    keyspace.clone(),
                    runner,
                )));
            }

            if let Err(e) = react::ws::server::start_with_ctx(cfg.server.port, suite_ctx).await {
                eprintln!("ERROR: {}", e);
                std::process::exit(1);
            }
        }
        Command::Run {
            config,
            thread_id,
            suite_id,
            agent,
            storage_mode,
            storage_path,
            bucket,
            tenant,
            workspace,
            project_id,
        } => {
            init_logging(&cli.log, cli.verbose_debug);

            let file_cfg = match react::config::ReactConfigFile::load_yaml(Path::new(&config)) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("ERROR: {}", e);
                    std::process::exit(1);
                }
            };
            let cfg = match react::config::ReactResolvedConfig::resolve(
                file_cfg,
                react::config::ServeOverrides {
                    port: None,
                    storage_mode,
                    bucket,
                    storage_path,
                    tenant,
                    workspace,
                    project_id,
                },
            ) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("ERROR: {}", e);
                    std::process::exit(1);
                }
            };

            let log_dir = resolve_log_dir(&cfg);
            // In terminal mode we keep console clean; logs always go to file.
            // `--log` only enables console output when terminal UI is not active.
            let terminal_default = true;
            let terminal_enabled = terminal_default && cli.log.is_none();
            let enable_console = cli.log.is_some() && !terminal_enabled;
            let _guard = init_tracing(&log_dir, enable_console);

            // Headless mode is always on for `run`.
            if std::env::var("REACT_HEADLESS")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .is_none()
            {
                std::env::set_var("REACT_HEADLESS", "1");
            }
            // Terminal UI is the default for `run`; `--log` disables it.
            if terminal_enabled {
                if let Err(e) = react::ws::terminal::init() {
                    eprintln!("WARN: terminal mode not enabled: {}", e);
                }
            }

            let (storage, keyspace, suite_bucket) = if cfg.storage.mode == "local" {
                let root = cfg
                    .storage
                    .path
                    .clone()
                    .ok_or_else(|| "internal: missing storage.path for local mode".to_string())
                    .unwrap_or_else(|e| {
                        eprintln!("ERROR: {}", e);
                        std::process::exit(1);
                    });
                let storage = match LocalFileStorageAdapter::new(root.clone()) {
                    Ok(s) => Arc::new(s) as Arc<dyn react::adapters::storage::StorageAdapter>,
                    Err(e) => {
                        eprintln!("ERROR: {}", e);
                        std::process::exit(1);
                    }
                };
                let keyspace = Arc::new(LocalKeyspace::new(root));
                (storage, keyspace as Arc<dyn react::providers::Keyspace>, "local".to_string())
            } else {
                let b = cfg
                    .storage
                    .bucket
                    .clone()
                    .ok_or_else(|| "internal: missing storage.bucket for s3 mode".to_string())
                    .unwrap_or_else(|e| {
                        eprintln!("ERROR: {}", e);
                        std::process::exit(1);
                    });
                let storage = Arc::new(S3StorageAdapter::from_env(b.clone()).await)
                    as Arc<dyn react::adapters::storage::StorageAdapter>;
                let keyspace = Arc::new(DefaultKeyspace::new(b.clone()));
                (storage, keyspace as Arc<dyn react::providers::Keyspace>, b)
            };
            let secrets = Arc::new(EnvSecretsProvider::default());
            let llm = llm::create_llm(&llm::config_from_resolved(&cfg));

            let mut suite_ctx =
                SuiteCtx::new(storage, secrets, llm, cfg.scope.clone(), keyspace.clone());
            suite_ctx.resolved_config = Some(Arc::new(react_suites::ReactResolvedConfig {
                server: react_suites::config::ServerResolved { port: 0 },
                storage: react_suites::config::StorageResolved {
                    bucket: suite_bucket.clone(),
                },
                scope: cfg.scope.clone(),
                llm: react_suites::config::LlmResolved::default(),
                providers: react_suites::config::ProvidersResolved {
                    warehouse: react_suites::config::WarehouseResolved {
                        kind: cfg.providers.warehouse.kind.clone(),
                        container: cfg
                            .providers
                            .warehouse
                            .container
                            .clone()
                            .unwrap_or_default(),
                        namespace: cfg
                            .providers
                            .warehouse
                            .namespace
                            .clone()
                            .unwrap_or_default(),
                        extras: cfg.providers.warehouse.extras.clone(),
                    },
                    catalog: react_suites::config::CatalogResolved {
                        enabled: cfg.providers.catalog.enabled,
                        refresh_secs: cfg.providers.catalog.refresh_secs,
                        max_concurrency: cfg.providers.catalog.max_concurrency,
                    },
                    dbt: react_suites::config::DbtResolved {
                        enabled: cfg.providers.dbt.enabled,
                        profiles_dir: cfg.providers.dbt.profiles_dir.clone(),
                        target: cfg.providers.dbt.target.clone().unwrap_or_default(),
                        naming: react_suites::config::DbtNamingResolved {
                            target_schema: cfg
                                .providers
                                .dbt
                                .naming
                                .target_schema
                                .clone()
                                .unwrap_or_default(),
                            silver_suffix: cfg
                                .providers
                                .dbt
                                .naming
                                .silver_suffix
                                .clone()
                                .unwrap_or_default(),
                            gold_suffix: cfg
                                .providers
                                .dbt
                                .naming
                                .gold_suffix
                                .clone()
                                .unwrap_or_default(),
                        },
                        runner: cfg.providers.dbt.runner.clone(),
                        docker_image: cfg.providers.dbt.docker_image.clone(),
                        docker_platform: cfg.providers.dbt.docker_platform.clone(),
                        docker_network: cfg.providers.dbt.docker_network.clone(),
                        docker_mount_aws_dir: cfg.providers.dbt.docker_mount_aws_dir,
                    },
                    vector: react_suites::config::VectorResolved {
                        enabled: cfg.providers.vector.enabled,
                    },
                },
            }));

            // Warehouse provider (single provider; dbt target)
            let wh_kind = cfg.providers.warehouse.kind.trim().to_ascii_lowercase();
            if wh_kind == "athena" {
                let extras = &cfg.providers.warehouse.extras;
                let workgroup = extras
                    .get("workgroup")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let result_s3 = extras
                    .get("result_s3")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let max_conc = extras
                    .get("max_concurrency")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize)
                    .unwrap_or(15);
                let ttl = extras
                    .get("discovery_cache_ttl_secs")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(120);
                let catalog = cfg
                    .providers
                    .warehouse
                    .container
                    .clone()
                    .unwrap_or_else(|| "AwsDataCatalog".to_string());
                let schema = cfg.providers.warehouse.namespace.clone();
                let athena = Arc::new(
                    AthenaQueryProvider::from_settings(AthenaSettings {
                        workgroup,
                        result_output_location: result_s3,
                        default_catalog: catalog,
                        source_schema: schema,
                        max_concurrency: max_conc,
                        discovery_cache_ttl_secs: ttl,
                    })
                    .await,
                );
                suite_ctx.warehouse = athena.clone();
                suite_ctx.query = Some(athena.clone());
                suite_ctx.datasets = Some(athena.clone());
            } else if wh_kind == "postgres" {
                let dbname = cfg.providers.warehouse.container.clone();
                let default_schema = cfg.providers.warehouse.namespace.clone();
                let pg = Arc::new(PostgresProvider::from_settings(PostgresSettings {
                    dbname,
                    default_schema,
                    ..Default::default()
                }));
                suite_ctx.warehouse = pg.clone();
                suite_ctx.query = Some(pg.clone());
                suite_ctx.datasets = Some(pg.clone());
            } else if wh_kind == "bigquery" {
                let project = cfg.providers.warehouse.container.clone();
                let dataset = cfg.providers.warehouse.namespace.clone();
                let location = cfg
                    .providers
                    .warehouse
                    .extras
                    .get("location")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let max_conc = cfg
                    .providers
                    .warehouse
                    .extras
                    .get("max_concurrency")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize)
                    .unwrap_or(15);
                let ttl_secs = cfg
                    .providers
                    .warehouse
                    .extras
                    .get("discovery_cache_ttl_secs")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(120);
                let bq = Arc::new(
                    BigQueryProvider::from_settings(BigQuerySettings {
                        project,
                        dataset,
                        location,
                        max_concurrency: max_conc,
                        discovery_cache_ttl_secs: ttl_secs,
                    })
                    .await
                    .map_err(|e| {
                        eprintln!("ERROR: BigQuery provider init failed: {}", e);
                        std::process::exit(1);
                    })
                    .unwrap(),
                );
                suite_ctx.warehouse = bq.clone();
                suite_ctx.query = Some(bq.clone());
                suite_ctx.datasets = Some(bq.clone());
            } else {
                eprintln!(
                    "ERROR: unsupported providers.warehouse.kind '{}'",
                    cfg.providers.warehouse.kind
                );
                std::process::exit(1);
            }

            if cfg.providers.catalog.enabled {
                let cat = Arc::new(DefaultCatalogProvider::new(
                    suite_ctx.storage.clone(),
                    keyspace.clone(),
                    suite_ctx.llm.clone(),
                    30,
                    6,
                ));
                suite_ctx.catalog = Some(cat);
            }

            if cfg.providers.vector.enabled {
                suite_ctx.vector = Some(Arc::new(LanceVectorStore::new(
                    keyspace.clone(),
                    cfg.scope.clone(),
                )));
            }

            if cfg.providers.dbt.enabled {
                let runner = DbtRunnerConfig {
                    mode: cfg.providers.dbt.runner.clone(),
                    docker_image: cfg.providers.dbt.docker_image.clone(),
                    docker_platform: cfg.providers.dbt.docker_platform.clone(),
                    docker_network: cfg.providers.dbt.docker_network.clone(),
                    docker_mount_aws_dir: cfg.providers.dbt.docker_mount_aws_dir,
                };
                suite_ctx.dbt = Some(Arc::new(DbtProjectProvider::new(
                    suite_ctx.storage.clone(),
                    keyspace.clone(),
                    runner,
                )));
            }

            let run_fut = react::run::headless::run_headless(
                suite_ctx,
                react::run::headless::RunOpts {
                    thread_id,
                    suite_id,
                    agent,
                },
            );

            let exit_code = tokio::select! {
                r = run_fut => r.unwrap_or_else(|e| { eprintln!("ERROR: {}", e); 1 }),
                _ = tokio::signal::ctrl_c() => 130,
            };
            // Ensure the terminal is restored before exiting.
            if terminal_enabled {
                react::ws::terminal::shutdown();
            }
            std::process::exit(exit_code);
        }
    }
}
