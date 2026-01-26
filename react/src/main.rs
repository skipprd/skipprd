use std::sync::Arc;

use clap::{Parser, Subcommand};
use std::path::Path;

use react::adapters::storage::S3StorageAdapter;
use react::llm;
use react::providers::{AthenaQueryProvider, AthenaSettings};
use react_suites::SuiteCtx;
use react::providers::{DefaultKeyspace, EnvSecretsProvider, DbtProjectProvider, LanceVectorStore};
use react::providers::dbt::DbtRunnerConfig;
use react::providers::catalog::DefaultCatalogProvider;

#[derive(Parser, Debug)]
#[command(name = "react")]
struct Cli {
    /// Enable logging (defaults to `info` when present). Respects `RUST_LOG` if set.
    #[arg(long, num_args = 0..=1, default_missing_value = "info")]
    log: Option<String>,

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

        /// S3 bucket for ReAct artifacts (catalogs, vectors, threads, dbt project).
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
    if log.is_none() {
        return;
    }
    if std::env::var("RUST_LOG").ok().filter(|v| !v.trim().is_empty()).is_none() {
        if let Some(level) = log.as_ref() {
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
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    init_logging(&cli.log, cli.verbose_debug);

    match cli.cmd {
        Command::Serve { config, port, bucket, tenant, workspace, project_id } => {
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
                    bucket,
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

            let storage = Arc::new(S3StorageAdapter::from_env(cfg.storage.bucket.clone()).await);
            let keyspace = Arc::new(DefaultKeyspace::new(cfg.storage.bucket.clone()));
            let secrets = Arc::new(EnvSecretsProvider::default());
            let llm = llm::create_llm(&llm::config_from_resolved(&cfg));

            let mut suite_ctx = SuiteCtx::new(storage, secrets, llm, cfg.scope.clone(), keyspace.clone());
            suite_ctx.resolved_config = Some(Arc::new(react_suites::ReactResolvedConfig {
                server: react_suites::config::ServerResolved { port: cfg.server.port },
                storage: react_suites::config::StorageResolved { bucket: cfg.storage.bucket.clone() },
                scope: cfg.scope.clone(),
                llm: react_suites::config::LlmResolved::default(),
                providers: react_suites::config::ProvidersResolved {
                    athena: react_suites::config::AthenaResolved {
                        enabled: cfg.providers.athena.enabled,
	                        workgroup: cfg.providers.athena.workgroup.clone().unwrap_or_default(),
	                        region: cfg.providers.athena.region.clone().unwrap_or_default(),
	                        result_s3: cfg.providers.athena.result_s3.clone().unwrap_or_default(),
                        discovery_cache_ttl_secs: cfg.providers.athena.discovery_cache_ttl_secs,
                        target_catalog: cfg.providers.athena.target_catalog.clone(),
	                        source_schema: cfg.providers.athena.source_schema.clone().unwrap_or_default(),
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
	                            target_schema: cfg.providers.dbt.naming.target_schema.clone().unwrap_or_default(),
	                            silver_suffix: cfg.providers.dbt.naming.silver_suffix.clone().unwrap_or_default(),
	                            gold_suffix: cfg.providers.dbt.naming.gold_suffix.clone().unwrap_or_default(),
                        },
	                        runner: cfg.providers.dbt.runner.clone(),
                        docker_image: cfg.providers.dbt.docker_image.clone(),
                        docker_platform: cfg.providers.dbt.docker_platform.clone(),
                        docker_network: cfg.providers.dbt.docker_network.clone(),
                        docker_mount_aws_dir: cfg.providers.dbt.docker_mount_aws_dir,
                    },
                    vector: react_suites::config::VectorResolved { enabled: cfg.providers.vector.enabled },
                },
            }));

            // Query + dataset discovery (Athena/Glue)
            if cfg.providers.athena.enabled {
                let athena = Arc::new(
                    AthenaQueryProvider::from_settings(AthenaSettings {
                        workgroup: cfg.providers.athena.workgroup.clone(),
                        result_output_location: cfg.providers.athena.result_s3.clone(),
                        default_catalog: cfg.providers.athena.target_catalog.clone(),
                        source_schema: cfg.providers.athena.source_schema.clone(),
                        max_concurrency: cfg.providers.athena.max_concurrency,
                        discovery_cache_ttl_secs: cfg.providers.athena.discovery_cache_ttl_secs,
                    })
                    .await,
                );
                suite_ctx.query = Some(athena.clone());
                suite_ctx.datasets = Some(athena.clone());
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
                suite_ctx.vector = Some(Arc::new(LanceVectorStore::new(keyspace.clone(), cfg.scope.clone())));
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
    }
}

