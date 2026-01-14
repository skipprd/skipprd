use std::sync::Arc;

use clap::{Parser, Subcommand};

use react::adapters::storage::S3StorageAdapter;
use react::llm;
use react::providers::{
    AthenaQueryProvider, DefaultKeyspace, EnvSecretsProvider, SkipprDbtProvider, SkipprLanceVectorStore,
};
use react::providers::catalog::SkipprCatalogProvider;
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
        /// WebSocket port to listen on.
        #[arg(long, default_value_t = 8787)]
        port: u16,

        /// S3 bucket for ReAct artifacts (catalogs, vectors, threads, dbt project).
        /// Defaults to env `SKIPPR_S3_BUCKET` if set.
        #[arg(long)]
        bucket: Option<String>,

        /// Tenant scope (artifact partition).
        #[arg(long, default_value = "default")]
        tenant: String,

        /// Workspace scope (artifact partition).
        #[arg(long, default_value = "default")]
        workspace: String,

        /// ReAct project identifier (artifact partition).
        #[arg(long, default_value = "default")]
        project_id: String,
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
        Command::Serve { port, bucket, tenant, workspace, project_id } => {
            let bucket = bucket
                .or_else(|| std::env::var("SKIPPR_S3_BUCKET").ok())
                .unwrap_or_else(|| "unset".to_string());

            let storage = Arc::new(S3StorageAdapter::from_env(bucket.clone()).await);
            let keyspace = Arc::new(DefaultKeyspace::new(bucket.clone()));
            let secrets = Arc::new(EnvSecretsProvider::default());
            let llm = llm::create_llm(&llm::config_from_env());

            let scope = react::providers::RequestScope { tenant, workspace, project_id };

            let mut suite_ctx = SuiteCtx::new(storage, secrets, llm, scope, keyspace.clone());

            // Query + dataset discovery (Athena/Glue)
            let athena = Arc::new(AthenaQueryProvider::from_env().await);
            suite_ctx.query = Some(athena.clone());
            suite_ctx.datasets = Some(athena);

            // Catalog + DBT + vectors
            suite_ctx.catalog = Some(Arc::new(SkipprCatalogProvider::new(
                suite_ctx.storage.clone(),
                suite_ctx.keyspace.clone(),
                suite_ctx.llm.clone(),
                60,
                8,
            )));
            suite_ctx.dbt = Some(Arc::new(SkipprDbtProvider::new(
                suite_ctx.storage.clone(),
                suite_ctx.keyspace.clone(),
            )));
            suite_ctx.vector = Some(Arc::new(SkipprLanceVectorStore::new(
                suite_ctx.keyspace.clone(),
                suite_ctx.scope.clone(),
            )));

            if let Err(e) = react::ws::server::start_with_ctx(port, suite_ctx).await {
                eprintln!("ERROR: {}", e);
                std::process::exit(1);
            }
        }
    }
}

