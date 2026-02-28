use std::sync::Arc;

use clap::{Parser, Subcommand};

fn nonempty(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() { None } else { Some(t.to_string()) }
}
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

use react::llm;
use react::providers::catalog::DefaultCatalogProvider;
use react::providers::{DefaultKeyspace, EnvSecretsProvider, LanceVectorStore, LocalKeyspace};
use react_core::suite::SuiteCtx;
use react_module_provider_athena::{AthenaQueryProvider, AthenaSettings};
use react_module_provider_bigquery::{BigQueryProvider, BigQuerySettings};
use react_module_provider_dbt::{DbtProjectProvider, DbtRunnerConfig};
use react_module_provider_postgres::{PostgresProvider, PostgresSettings};
use react_module_storage::{LocalFileStorageAdapter, S3StorageAdapter};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinSet;
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
        /// Path(s) to YAML config file(s). Repeat `--config` to run multiple.
        #[arg(long, value_name = "PATH", required = true, num_args = 1..)]
        config: Vec<String>,

        /// Run multiple `--config` entries concurrently in this process.
        ///
        /// - Requires at least two `--config` values.
        /// - Prints compact per-config progress and exits non-zero if any run fails.
        #[arg(long, default_value_t = false)]
        parallel: bool,

        /// Existing thread id to continue.
        #[arg(long)]
        thread_id: Option<String>,

        /// Suite to run (defaults to first registered suite).
        #[arg(long)]
        suite_id: Option<String>,

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

fn config_run_label(index: usize, config_path: &str) -> String {
    let stem = Path::new(config_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("config");
    format!("{:02}-{}", index + 1, stem)
}

fn sanitize_for_filename(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn getenv_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn env_usize(key: &str) -> Option<usize> {
    getenv_nonempty(key).and_then(|v| v.parse::<usize>().ok())
}

fn env_u64(key: &str) -> Option<u64> {
    getenv_nonempty(key).and_then(|v| v.parse::<u64>().ok())
}

fn apply_aws_region_fallback_from_warehouse(warehouse_extras: &serde_json::Value) {
    if let Some(region) = warehouse_extras
        .get("region")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let has_default = std::env::var("AWS_DEFAULT_REGION")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .is_some();
        let has_region = std::env::var("AWS_REGION")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .is_some();
        if !has_default {
            std::env::set_var("AWS_DEFAULT_REGION", region);
        }
        if !has_region {
            std::env::set_var("AWS_REGION", region);
        }
    }
}

fn resolve_athena_settings(cfg: &react::config::ReactResolvedConfig) -> AthenaSettings {
    let extras = &cfg.providers.warehouse.extras;
    let workgroup = getenv_nonempty("ATHENA_WORKGROUP")
        .or_else(|| getenv_nonempty("DATA_OUTPUT_ATHENA_WORKGROUP_NAME"))
        .or_else(|| {
            extras
                .get("workgroup")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        });

    let result_output_location = getenv_nonempty("ATHENA_RESULT_S3")
        .or_else(|| {
            getenv_nonempty("DATA_OUTPUT_ATHENA_RESULTS_S3_BUCKET")
                .map(|b| format!("s3://{}/", b.trim_end_matches('/')))
        })
        .or_else(|| {
            extras
                .get("result_s3")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        });

    let default_catalog = getenv_nonempty("ATHENA_TARGET_CATALOG")
        .or_else(|| getenv_nonempty("ATHENA_CATALOG"))
        .or_else(|| Some(cfg.providers.warehouse.container.clone()).filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "AwsDataCatalog".to_string());

    let source_schema = getenv_nonempty("ATHENA_SOURCE_SCHEMA")
        .or_else(|| getenv_nonempty("ATHENA_SOURCE_DATABASE"))
        .or_else(|| Some(cfg.providers.warehouse.namespace.clone()).filter(|s| !s.is_empty()));

    let max_concurrency = env_usize("ATHENA_MAX_CONCURRENCY")
        .or_else(|| {
            extras
                .get("max_concurrency")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
        })
        .unwrap_or(15);

    let discovery_cache_ttl_secs = env_u64("ATHENA_DISCOVERY_CACHE_TTL_SECS")
        .or_else(|| {
            extras
                .get("discovery_cache_ttl_secs")
                .and_then(|v| v.as_u64())
        })
        .unwrap_or(120);

    AthenaSettings {
        workgroup,
        result_output_location,
        default_catalog,
        source_schema,
        max_concurrency,
        discovery_cache_ttl_secs,
    }
}

fn resolve_default_suite_id() -> String {
    react_suites::default_registry()
        .list_ids()
        .into_iter()
        .next()
        .unwrap_or("kb")
        .to_string()
}

async fn run_parallel_configs(
    log: &Option<String>,
    verbose_debug: bool,
    configs: &[String],
    suite_id: &Option<String>,
    agent: &str,
    storage_mode: &Option<String>,
    storage_path: &Option<String>,
    bucket: &Option<String>,
    tenant: &Option<String>,
    workspace: &Option<String>,
    project_id: &Option<String>,
) -> Result<i32, String> {
    if configs.len() < 2 {
        return Err("parallel mode requires at least two --config values".to_string());
    }

    let exe = std::env::current_exe()
        .map_err(|e| format!("failed to resolve current executable path: {e}"))?;
    let logs_dir = PathBuf::from("./.react/multi-run-logs");
    std::fs::create_dir_all(&logs_dir).map_err(|e| {
        format!(
            "failed to create multi-run log dir '{}': {e}",
            logs_dir.display()
        )
    })?;

    println!("Starting parallel run for {} config(s)", configs.len());

    let mut joins = JoinSet::new();
    for (idx, config_path) in configs.iter().enumerate() {
        let label = config_run_label(idx, config_path);
        let log_file = logs_dir.join(format!("{}.log", sanitize_for_filename(&label)));
        let file = tokio::fs::File::create(&log_file).await.map_err(|e| {
            format!(
                "failed to create log file '{}' for {}: {e}",
                log_file.display(),
                label
            )
        })?;
        let shared_file = Arc::new(Mutex::new(file));

        let mut cmd = tokio::process::Command::new(&exe);
        if let Some(level) = log.as_ref() {
            cmd.arg("--log").arg(level);
        } else {
            // Parallel mode is non-interactive; force plain logs instead of TTY renderer.
            cmd.arg("--log").arg("info");
        }
        if verbose_debug {
            cmd.arg("--verbose-debug");
        }
        cmd.env("REACT_PLAIN_PROGRESS", "1");
        cmd.arg("run").arg("--config").arg(config_path).arg("--agent").arg(agent);
        if let Some(s) = suite_id.as_ref().filter(|s| !s.trim().is_empty()) {
            cmd.arg("--suite-id").arg(s);
        }

        if let Some(v) = storage_mode.as_ref() {
            cmd.arg("--storage-mode").arg(v);
        }
        if let Some(v) = storage_path.as_ref() {
            cmd.arg("--storage-path").arg(v);
        }
        if let Some(v) = bucket.as_ref() {
            cmd.arg("--bucket").arg(v);
        }
        if let Some(v) = tenant.as_ref() {
            cmd.arg("--tenant").arg(v);
        }
        if let Some(v) = workspace.as_ref() {
            cmd.arg("--workspace").arg(v);
        }
        if let Some(v) = project_id.as_ref() {
            cmd.arg("--project-id").arg(v);
        }

        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let started_at = Instant::now();
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn {} ({config_path}): {e}", label))?;
        println!(
            "[{}] started ({}) log={}",
            label,
            config_path,
            log_file.display()
        );
        let stdout = child.stdout.take().ok_or_else(|| {
            format!(
                "failed to capture stdout pipe for {} ({})",
                label, config_path
            )
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            format!(
                "failed to capture stderr pipe for {} ({})",
                label, config_path
            )
        })?;
        let stdout_task = tokio::spawn(stream_child_output(
            label.clone(),
            false,
            stdout,
            shared_file.clone(),
        ));
        let stderr_task = tokio::spawn(stream_child_output(
            label.clone(),
            true,
            stderr,
            shared_file.clone(),
        ));

        let cfg = config_path.clone();
        let lb = label.clone();
        let lf = log_file.clone();
        joins.spawn(async move {
            let status = child.wait().await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            (lb, cfg, lf, started_at, status)
        });
    }

    let mut failures = 0usize;
    while let Some(next) = joins.join_next().await {
        let (label, config_path, log_file, started_at, status) =
            next.map_err(|e| format!("parallel runner task join failed: {e}"))?;
        let elapsed = started_at.elapsed().as_secs_f32();
        match status {
            Ok(s) if s.success() => {
                println!("[{}] ok ({:.1}s) {}", label, elapsed, config_path);
            }
            Ok(s) => {
                failures += 1;
                let code = s
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".to_string());
                println!(
                    "[{}] failed ({:.1}s, exit={}) {} log={}",
                    label,
                    elapsed,
                    code,
                    config_path,
                    log_file.display()
                );
            }
            Err(e) => {
                failures += 1;
                println!(
                    "[{}] failed ({:.1}s, spawn/wait error={}) {} log={}",
                    label,
                    elapsed,
                    e,
                    config_path,
                    log_file.display()
                );
            }
        }
    }

    if failures > 0 {
        println!(
            "Parallel run finished: {} failed, {} succeeded",
            failures,
            configs.len().saturating_sub(failures)
        );
        Ok(1)
    } else {
        println!(
            "Parallel run finished: all {} config(s) succeeded",
            configs.len()
        );
        Ok(0)
    }
}

async fn stream_child_output<R>(
    label: String,
    is_stderr: bool,
    reader: R,
    file: Arc<Mutex<tokio::fs::File>>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        println!("[{}] {}", label, line);
        let prefix = if is_stderr { "stderr" } else { "stdout" };
        let mut f = file.lock().await;
        let _ = tokio::io::AsyncWriteExt::write_all(
            &mut *f,
            format!("[{}] {}\n", prefix, line).as_bytes(),
        )
        .await;
    }
}

struct TracingGuards {
    _file: tracing_appender::non_blocking::WorkerGuard,
    _run: Option<tracing_appender::non_blocking::WorkerGuard>,
}

fn init_tracing(
    log_dir: &Path,
    enable_console: bool,
    run_writer: Option<react::thread_logs::RunThreadLogWriter>,
) -> TracingGuards {
    let _ = std::fs::create_dir_all(log_dir);
    let appender = tracing_appender::rolling::daily(log_dir, "react.log");
    let (nb, guard) = tracing_appender::non_blocking(appender);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(nb)
        .with_target(true)
        .with_filter(tracing_subscriber::EnvFilter::from_default_env());

    let (run_layer_opt, run_guard) = if let Some(w) = run_writer {
        let (run_nb, run_guard) = tracing_appender::non_blocking(w);
        let run_layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(run_nb)
            .with_target(true)
            // Per-thread run logs should include LLM request/response observability
            // even when the default env filter is `info`.
            .with_filter(tracing_subscriber::filter::LevelFilter::DEBUG);
        (Some(run_layer), Some(run_guard))
    } else {
        (None, None)
    };

    let console_layer_opt = if enable_console {
        Some(
            tracing_subscriber::fmt::layer()
                .with_ansi(true)
                .with_target(true)
                .with_filter(tracing_subscriber::EnvFilter::from_default_env()),
        )
    } else {
        None
    };

    // Note: `Option<Layer>` itself implements `Layer`, so conditional layers can be
    // applied without changing subscriber types.
    let _ = tracing_subscriber::registry()
        .with(file_layer)
        .with(run_layer_opt)
        .with(console_layer_opt)
        .try_init();

    TracingGuards {
        _file: guard,
        _run: run_guard,
    }
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
            let cfg = match react::config::resolve_config(
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
            let _guards = init_tracing(&log_dir, enable_console, None);

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
                (
                    storage,
                    keyspace as Arc<dyn react::providers::Keyspace>,
                    "local".to_string(),
                )
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
            suite_ctx.resolved_config = Some(Arc::new(cfg.clone()));

            let wh_kind = cfg.providers.warehouse.kind.trim().to_ascii_lowercase();
            if wh_kind == "athena" {
                apply_aws_region_fallback_from_warehouse(&cfg.providers.warehouse.extras);
                let athena = Arc::new(
                    AthenaQueryProvider::from_settings(resolve_athena_settings(&cfg)).await,
                );
                suite_ctx.warehouse = athena.clone();
                suite_ctx.query = Some(athena.clone());
                suite_ctx.datasets = Some(athena.clone());
            } else if wh_kind == "postgres" {
                let dbname = nonempty(&cfg.providers.warehouse.container);
                let default_schema = nonempty(&cfg.providers.warehouse.namespace);
                let pg = Arc::new(PostgresProvider::from_settings(PostgresSettings {
                    dbname,
                    default_schema,
                    ..Default::default()
                }));
                suite_ctx.warehouse = pg.clone();
                suite_ctx.query = Some(pg.clone());
                suite_ctx.datasets = Some(pg.clone());
            } else if wh_kind == "bigquery" {
                let project = nonempty(&cfg.providers.warehouse.container);
                let dataset = nonempty(&cfg.providers.warehouse.namespace);
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

            let registry = react_suites::default_registry();
            if let Err(e) = react::ws::server::start_with_ctx(cfg.server.port, suite_ctx, registry).await {
                eprintln!("ERROR: {}", e);
                std::process::exit(1);
            }
        }
        Command::Run {
            config,
            parallel,
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
            if config.len() > 1 {
                if cli.terminal {
                    eprintln!("ERROR: --terminal is not supported with parallel multi-config runs");
                    std::process::exit(2);
                }
                if !parallel {
                    eprintln!("ERROR: multiple --config values require --parallel");
                    std::process::exit(2);
                }
                let exit_code = match run_parallel_configs(
                    &cli.log,
                    cli.verbose_debug,
                    &config,
                    &suite_id,
                    &agent,
                    &storage_mode,
                    &storage_path,
                    &bucket,
                    &tenant,
                    &workspace,
                    &project_id,
                )
                .await
                {
                    Ok(code) => code,
                    Err(e) => {
                        eprintln!("ERROR: {}", e);
                        1
                    }
                };
                std::process::exit(exit_code);
            }
            if parallel {
                eprintln!("WARN: --parallel ignored with a single --config");
            }
            let config = match config.into_iter().next() {
                Some(v) => v,
                None => {
                    eprintln!("ERROR: missing --config");
                    std::process::exit(2);
                }
            };

            init_logging(&cli.log, cli.verbose_debug);

            let file_cfg = match react::config::ReactConfigFile::load_yaml(Path::new(&config)) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("ERROR: {}", e);
                    std::process::exit(1);
                }
            };
            let cfg = match react::config::resolve_config(
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

            // Ensure LLM request/response observability is enabled for terminal runs.
            // (The actual log lines are emitted at DEBUG level.)
            if terminal_enabled {
                if std::env::var("REACT_LOG_LLM_CALLS")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                    .is_none()
                {
                    std::env::set_var("REACT_LOG_LLM_CALLS", "1");
                }
                if std::env::var("REACT_LOG_LLM_RESPONSE_TEXT")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                    .is_none()
                {
                    std::env::set_var("REACT_LOG_LLM_RESPONSE_TEXT", "1");
                }
            }

            // When running in terminal mode, also persist a per-thread run log:
            // - local: stream to a temp file under `<root>/<scope>/logs/` and rename once thread_id is known
            // - non-local: stream to a temp file and upload at end
            let run_logs = if terminal_enabled {
                if cfg.storage.mode == "local" {
                    if let Some(root) = cfg.storage.path.as_ref() {
                        react::thread_logs::RunThreadLogs::new_local(
                            root.clone(),
                            cfg.scope.clone(),
                        )
                        .ok()
                    } else {
                        None
                    }
                } else {
                    react::thread_logs::RunThreadLogs::new_buffered(cfg.scope.clone()).ok()
                }
            } else {
                None
            };
            let run_writer = run_logs.as_ref().map(|l| l.make_writer());
            let guards = init_tracing(&log_dir, enable_console, run_writer);

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
                (
                    storage,
                    keyspace as Arc<dyn react::providers::Keyspace>,
                    "local".to_string(),
                )
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
            suite_ctx.resolved_config = Some(Arc::new(cfg.clone()));

            let wh_kind = cfg.providers.warehouse.kind.trim().to_ascii_lowercase();
            if wh_kind == "athena" {
                apply_aws_region_fallback_from_warehouse(&cfg.providers.warehouse.extras);
                let athena = Arc::new(
                    AthenaQueryProvider::from_settings(resolve_athena_settings(&cfg)).await,
                );
                suite_ctx.warehouse = athena.clone();
                suite_ctx.query = Some(athena.clone());
                suite_ctx.datasets = Some(athena.clone());
            } else if wh_kind == "postgres" {
                let dbname = nonempty(&cfg.providers.warehouse.container);
                let default_schema = nonempty(&cfg.providers.warehouse.namespace);
                let pg = Arc::new(PostgresProvider::from_settings(PostgresSettings {
                    dbname,
                    default_schema,
                    ..Default::default()
                }));
                suite_ctx.warehouse = pg.clone();
                suite_ctx.query = Some(pg.clone());
                suite_ctx.datasets = Some(pg.clone());
            } else if wh_kind == "bigquery" {
                let project = nonempty(&cfg.providers.warehouse.container);
                let dataset = nonempty(&cfg.providers.warehouse.namespace);
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

            let requested_thread_id = thread_id.clone();

            // If user requested a specific thread_id, we already know it; bind immediately so the
            // run log is written to `logs/{thread_id}.log` throughout the run (local mode rename).
            if let (Some(ref tid), Some(ref logs)) = (thread_id.as_ref(), run_logs.as_ref()) {
                let _ = logs.bind_thread_id(keyspace.as_ref(), tid);
            }

            // Clone before moving `suite_ctx` into the run future.
            let storage_for_logs = suite_ctx.storage.clone();
            let keyspace_for_logs = suite_ctx.keyspace.clone();

            // Bind the run log to the real thread_id as soon as it is observed so the
            // file appears as `logs/{thread_id}.log` during the run (local mode rename).
            let (thread_id_tx, tid_rx) = if run_logs.is_some() && requested_thread_id.is_none() {
                let (tx, rx) = mpsc::unbounded_channel::<String>();
                (Some(tx), Some(rx))
            } else {
                (None, None)
            };

            if let (Some(mut rx), Some(logs)) = (tid_rx, run_logs.clone()) {
                let ks = keyspace.clone();
                tokio::spawn(async move {
                    if let Some(tid) = rx.recv().await {
                        let _ = logs.bind_thread_id(ks.as_ref(), &tid);
                    }
                });
            }

            let suite_id = suite_id
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(resolve_default_suite_id);
            let registry = react_suites::default_registry();
            let run_fut = react::run::headless::run_headless(
                suite_ctx,
                react::run::headless::RunOpts {
                    thread_id,
                    suite_id,
                    agent,
                    thread_id_tx,
                },
                registry,
            );

            let mut thread_id_for_logs: Option<String> = None;
            let exit_code: i32 = tokio::select! {
                r = run_fut => {
                    match r {
                        Ok((code, tid)) => {
                            thread_id_for_logs = Some(tid);
                            code
                        }
                        Err(e) => {
                            eprintln!("ERROR: {}", e);
                            1
                        }
                    }
                },
                _ = tokio::signal::ctrl_c() => 130,
            };
            // Ensure the terminal is restored before exiting.
            if terminal_enabled {
                react::ws::terminal::shutdown();
            }

            // Flush tracing before we read/upload log bytes.
            drop(guards);

            // Finalize per-thread logs.
            if let Some(logs) = run_logs.as_ref() {
                let tid = thread_id_for_logs
                    .clone()
                    .or_else(|| requested_thread_id.clone());
                if let Some(tid) = tid {
                    // Local mode: bind_thread_id renames temp file into place.
                    let _ = logs.bind_thread_id(keyspace.as_ref(), &tid);
                    // Non-local: upload temp file to storage key.
                    let _ = logs
                        .upload_if_needed(storage_for_logs.clone(), keyspace_for_logs.clone(), &tid)
                        .await;
                }
            }
            std::process::exit(exit_code);
        }
    }
}
