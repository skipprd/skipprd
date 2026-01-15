use std::sync::Arc;

use clap::{Parser, Subcommand};
use std::path::Path;

use react::adapters::storage::S3StorageAdapter;
use react::llm;
use react::providers::{
    AthenaQueryProvider, AthenaSettings, DbtProjectProvider, DefaultKeyspace, EnvSecretsProvider, LanceVectorStore,
};
use react::providers::catalog::DefaultCatalogProvider;
use react::suites::SuiteCtx;

#[derive(Parser, Debug)]
#[command(name = "react")]
struct Cli {
    /// Enable logging (defaults to `info` when present). Respects `RUST_LOG` if set.
    #[arg(long, num_args = 0..=1, default_missing_value = "info")]
    log: Option<String>,

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

fn init_logging(log: &Option<String>) {
    if log.is_none() {
        return;
    }
    if std::env::var("RUST_LOG").ok().filter(|v| !v.trim().is_empty()).is_none() {
        if let Some(level) = log.as_ref() {
            std::env::set_var("RUST_LOG", level);
        }
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    init_logging(&cli.log);

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
            suite_ctx.resolved_config = Some(Arc::new(cfg.clone()));

            // Query + dataset discovery (Athena/Glue)
            if cfg.providers.athena.enabled {
                let athena = Arc::new(
                    AthenaQueryProvider::from_settings(AthenaSettings {
                        workgroup: cfg.providers.athena.workgroup.clone(),
                        result_output_location: cfg.providers.athena.result_s3.clone(),
                        default_catalog: cfg.providers.athena.catalog.clone(),
                        default_database: cfg.providers.athena.default_database.clone(),
                        discovery_cache_ttl_secs: cfg.providers.athena.discovery_cache_ttl_secs,
                    })
                    .await,
                );
                suite_ctx.query = Some(athena.clone());
                suite_ctx.datasets = Some(athena);
            }

            // Catalog + DBT + vectors
            if cfg.providers.catalog.enabled {
                suite_ctx.catalog = Some(Arc::new(DefaultCatalogProvider::new(
                    suite_ctx.storage.clone(),
                    suite_ctx.keyspace.clone(),
                    suite_ctx.llm.clone(),
                    cfg.providers.catalog.refresh_secs,
                    cfg.providers.catalog.max_concurrency,
                )));
            }
            if cfg.providers.dbt.enabled {
                suite_ctx.dbt = Some(Arc::new(DbtProjectProvider::new(
                    suite_ctx.storage.clone(),
                    suite_ctx.keyspace.clone(),
                    react::providers::dbt::DbtRunnerConfig {
                        mode: cfg.providers.dbt.runner.clone(),
                        docker_image: cfg.providers.dbt.docker_image.clone(),
                        docker_platform: cfg.providers.dbt.docker_platform.clone(),
                        docker_network: cfg.providers.dbt.docker_network.clone(),
                        docker_mount_aws_dir: cfg.providers.dbt.docker_mount_aws_dir,
                    },
                )));
            }
            if cfg.providers.vector.enabled {
                suite_ctx.vector = Some(Arc::new(LanceVectorStore::new(
                    suite_ctx.keyspace.clone(),
                    suite_ctx.scope.clone(),
                )));
            }

            if let Err(e) = react::ws::server::start_with_ctx(cfg.server.port, suite_ctx).await {
                eprintln!("ERROR: {}", e);
                std::process::exit(1);
            }
        }
    }
}

