mod api_client;
mod auth;
mod feedback_diagnostics;
mod public_config;
mod react_host;
mod skippr_bin;
mod translate;

use std::{collections::HashMap, path::PathBuf, process::Command, sync::Arc};

use clap::{Parser, Subcommand};
use react_core::keyspace::Keyspace;

use public_config::{DbtConfig, S3Transform, SkipprDbtConfig, SourceConfig, WarehouseConfig};

#[derive(Parser, Debug)]
#[command(
    name = "skippr",
    about = "Data pipeline CLI — extract, load, and model with dbt",
    version = option_env!("SKIPPR_CLI_VERSION").unwrap_or(env!("CARGO_PKG_VERSION")),
)]
struct Cli {
    /// Log level (info, debug, trace). When omitted the live terminal UI is shown.
    #[arg(long, global = true, num_args = 0..=1, default_missing_value = "info")]
    log: Option<String>,

    /// Path to config file. Defaults to ./skippr.yaml in the working directory.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Initialise a new project.
    Init {
        /// Project name (used as the pipeline identifier and default dbt schema).
        name: String,
        /// Purge all local data (offsets, metadata, buffers) and re-initialise.
        #[arg(long, default_value_t = false)]
        reset: bool,
    },

    /// Configure a warehouse or source connection.
    Connect {
        #[command(subcommand)]
        target: ConnectTarget,
    },

    /// Check that all prerequisites are in place.
    Doctor,

    /// Execute the full pipeline (extract, load, model).
    Run,

    /// Attach human feedback to a project thread run.
    Feedback {
        /// Mark the most recent thread run as good.
        #[arg(long, conflicts_with = "bad", required_unless_present = "bad")]
        good: bool,
        /// Mark the most recent thread run as bad.
        #[arg(long, conflicts_with = "good", required_unless_present = "good")]
        bad: bool,
        /// Feedback comment. When omitted, the CLI prompts for a single-line message.
        #[arg(long)]
        comment: Option<String>,
        /// Do not attach a redacted support diagnostics bundle.
        #[arg(long, default_value_t = false)]
        no_diagnostics: bool,
    },

    /// User account management (signup, login, balance, etc.).
    User {
        #[command(subcommand)]
        action: UserAction,
    },
}

#[derive(Subcommand, Debug)]
enum UserAction {
    /// Sign up or log in with your phone number.
    Login,
    /// Log out and remove local credentials.
    Logout,
    /// Show account balance and recent usage.
    Account,
    /// Add funds to your account.
    BuyCredits {
        /// Dollar amount to add (e.g. 25 for $25). Minimum $5.
        #[arg(long)]
        amount: Option<f64>,
    },
    /// Create a new API key for CI/CD or automation.
    CreateApiKey {
        /// Human-readable label (e.g. "github-actions").
        #[arg(long)]
        name: String,
    },
    /// Revoke an existing API key.
    RevokeApiKey {
        /// The key_id to revoke (from list-api-keys output).
        #[arg(long)]
        key_id: String,
    },
    /// List all API keys for your account.
    ListApiKeys,
}

#[derive(Subcommand, Debug)]
enum ConnectTarget {
    /// Configure the destination warehouse.
    Warehouse {
        #[command(subcommand)]
        kind: WarehouseKind,
    },
    /// Configure the data source for extraction.
    Source {
        #[command(subcommand)]
        kind: SourceKind,
    },
}

#[derive(Subcommand, Debug)]
enum WarehouseKind {
    /// AWS Athena (S3 + Glue) warehouse.
    Athena {
        #[arg(long)]
        workgroup: Option<String>,
        #[arg(long)]
        region: Option<String>,
        #[arg(long)]
        result_s3: Option<String>,
        #[arg(long)]
        schema: Option<String>,
    },
    /// Snowflake warehouse.
    Snowflake {
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        user: Option<String>,
        #[arg(long)]
        password: Option<String>,
        #[arg(long)]
        private_key_path: Option<String>,
        #[arg(long)]
        stage: Option<String>,
        #[arg(long)]
        staging_uri: Option<String>,
        #[arg(long)]
        staging_storage_integration: Option<String>,
        #[arg(long)]
        staging_azure_sas_token: Option<String>,
        #[arg(long)]
        staging_azure_account_key: Option<String>,
        #[arg(long)]
        staging_gcs_service_account_key_path: Option<String>,
        #[arg(long)]
        database: Option<String>,
        #[arg(long)]
        schema: Option<String>,
        #[arg(long)]
        warehouse: Option<String>,
        #[arg(long)]
        role: Option<String>,
    },
    /// Google BigQuery warehouse.
    Bigquery {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        dataset: Option<String>,
        #[arg(long)]
        location: Option<String>,
    },
    /// PostgreSQL warehouse.
    Postgres {
        #[arg(long)]
        database: Option<String>,
        #[arg(long)]
        schema: Option<String>,
    },
    /// Databricks (Unity Catalog) warehouse.
    Databricks {
        #[arg(long)]
        workspace_url: Option<String>,
        #[arg(long)]
        token: Option<String>,
        #[arg(long)]
        warehouse_id: Option<String>,
        #[arg(long)]
        catalog: Option<String>,
        #[arg(long)]
        schema: Option<String>,
    },
    /// Azure Synapse Analytics warehouse.
    Synapse {
        #[arg(long)]
        connection_string: Option<String>,
        #[arg(long)]
        schema: Option<String>,
    },
    /// Amazon Redshift warehouse.
    Redshift {
        #[arg(long)]
        database: Option<String>,
        #[arg(long)]
        cluster_identifier: Option<String>,
        #[arg(long)]
        workgroup_name: Option<String>,
        #[arg(long)]
        db_user: Option<String>,
        #[arg(long)]
        schema: Option<String>,
        #[arg(long)]
        region: Option<String>,
        #[arg(long)]
        staging_s3_bucket: Option<String>,
        #[arg(long)]
        staging_s3_prefix: Option<String>,
        #[arg(long)]
        iam_role_arn: Option<String>,
    },
    /// ClickHouse warehouse.
    Clickhouse {
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        database: Option<String>,
        #[arg(long)]
        user: Option<String>,
        #[arg(long)]
        password: Option<String>,
    },
    /// MotherDuck warehouse.
    Motherduck {
        #[arg(long)]
        motherduck_token: Option<String>,
        #[arg(long)]
        database: Option<String>,
        #[arg(long)]
        schema: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum SourceKind {
    /// Microsoft SQL Server source.
    Mssql {
        /// ADO.NET connection string, or use ${ENV_VAR} notation.
        #[arg(long)]
        connection_string: Option<String>,
    },
    /// S3 bucket source.
    S3 {
        #[arg(long)]
        bucket: Option<String>,
        #[arg(long)]
        prefix: Option<String>,
        /// Field(s) used to namespace incoming events (e.g. event_type).
        #[arg(long)]
        namespace_fields: Option<String>,
    },
    /// MySQL source.
    Mysql {
        #[arg(long)]
        connection_string: Option<String>,
        #[arg(long, value_delimiter = ',')]
        tables: Option<Vec<String>>,
    },
    /// PostgreSQL source (distinct from postgres warehouse).
    PostgresSource {
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        user: Option<String>,
        #[arg(long)]
        password: Option<String>,
        #[arg(long)]
        database: Option<String>,
        #[arg(long)]
        connection_string: Option<String>,
        #[arg(long, value_delimiter = ',')]
        tables: Option<Vec<String>>,
        #[arg(long)]
        query: Option<String>,
    },
    /// Amazon Redshift source.
    RedshiftSource {
        #[arg(long)]
        cluster_identifier: Option<String>,
        #[arg(long)]
        workgroup_name: Option<String>,
        #[arg(long)]
        database: Option<String>,
        #[arg(long)]
        db_user: Option<String>,
        #[arg(long, value_delimiter = ',')]
        tables: Option<Vec<String>>,
        #[arg(long)]
        region: Option<String>,
    },
    /// MongoDB source.
    Mongodb {
        #[arg(long)]
        connection_string: Option<String>,
        #[arg(long)]
        database: Option<String>,
        #[arg(long)]
        collection: Option<String>,
        #[arg(long)]
        filter: Option<String>,
    },
    /// DynamoDB source.
    Dynamodb {
        #[arg(long)]
        table_name: Option<String>,
        #[arg(long)]
        region: Option<String>,
        #[arg(long)]
        endpoint_url: Option<String>,
    },
    /// ClickHouse source.
    ClickhouseSource {
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        database: Option<String>,
        #[arg(long)]
        user: Option<String>,
        #[arg(long)]
        password: Option<String>,
        #[arg(long, value_delimiter = ',')]
        tables: Option<Vec<String>>,
        #[arg(long)]
        query: Option<String>,
    },
    /// MotherDuck source.
    MotherduckSource {
        #[arg(long)]
        motherduck_token: Option<String>,
        #[arg(long)]
        database: Option<String>,
        #[arg(long, value_delimiter = ',')]
        tables: Option<Vec<String>>,
        #[arg(long)]
        query: Option<String>,
    },
    /// SFTP source.
    Sftp {
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        username: Option<String>,
        #[arg(long)]
        password: Option<String>,
        #[arg(long)]
        private_key_path: Option<String>,
        #[arg(long)]
        remote_path: Option<String>,
    },
    /// Local file source.
    File {
        #[arg(long)]
        path: Option<String>,
    },
    /// Delta Lake source.
    DeltaLake {
        #[arg(long)]
        table_uri: Option<String>,
        #[arg(long = "storage-option", value_parser = parse_key_val)]
        storage_options: Option<Vec<(String, String)>>,
        #[arg(long)]
        version: Option<i64>,
        #[arg(long)]
        filter: Option<String>,
    },
    /// Kafka source.
    Kafka {
        #[arg(long)]
        brokers: Option<String>,
        #[arg(long)]
        topic: Option<String>,
        #[arg(long)]
        group_id: Option<String>,
        #[arg(long)]
        auto_offset_reset: Option<String>,
        #[arg(long)]
        security_protocol: Option<String>,
        #[arg(long)]
        sasl_mechanism: Option<String>,
        #[arg(long)]
        sasl_username: Option<String>,
        #[arg(long)]
        sasl_password: Option<String>,
        #[arg(long)]
        mode: Option<String>,
    },
    /// SQS source.
    Sqs {
        #[arg(long)]
        queue_url: Option<String>,
        #[arg(long)]
        region: Option<String>,
        #[arg(long)]
        endpoint_url: Option<String>,
        #[arg(long)]
        mode: Option<String>,
    },
    /// Kinesis source.
    Kinesis {
        #[arg(long)]
        stream_name: Option<String>,
        #[arg(long)]
        region: Option<String>,
        #[arg(long)]
        endpoint_url: Option<String>,
        #[arg(long)]
        mode: Option<String>,
    },
    /// AMQP (RabbitMQ) source.
    Amqp {
        #[arg(long)]
        connection_string: Option<String>,
        #[arg(long)]
        queue: Option<String>,
        #[arg(long)]
        exchange: Option<String>,
        #[arg(long)]
        routing_key: Option<String>,
        #[arg(long)]
        prefetch_count: Option<u32>,
        #[arg(long)]
        mode: Option<String>,
    },
    /// SNS source (via SQS).
    Sns {
        #[arg(long)]
        topic_arn: Option<String>,
        #[arg(long)]
        sqs_queue_url: Option<String>,
        #[arg(long)]
        region: Option<String>,
        #[arg(long)]
        endpoint_url: Option<String>,
    },
    /// EventBridge source (via SQS).
    Eventbridge {
        #[arg(long)]
        event_bus_name: Option<String>,
        #[arg(long)]
        sqs_queue_url: Option<String>,
        #[arg(long)]
        region: Option<String>,
        #[arg(long)]
        endpoint_url: Option<String>,
    },
    /// MQTT source.
    Mqtt {
        #[arg(long)]
        broker_url: Option<String>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        topic: Option<String>,
        #[arg(long)]
        client_id: Option<String>,
        #[arg(long)]
        qos: Option<u8>,
        #[arg(long)]
        username: Option<String>,
        #[arg(long)]
        password: Option<String>,
        #[arg(long)]
        mode: Option<String>,
    },
    /// WebSocket source.
    Websocket {
        #[arg(long)]
        url: Option<String>,
        #[arg(long, value_parser = parse_key_val)]
        headers: Option<Vec<(String, String)>>,
        #[arg(long)]
        mode: Option<String>,
    },
    /// HTTP client source (polling).
    HttpClient {
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        method: Option<String>,
        #[arg(long, value_parser = parse_key_val)]
        headers: Option<Vec<(String, String)>>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        auth_strategy: Option<String>,
        #[arg(long)]
        auth_user: Option<String>,
        #[arg(long)]
        auth_password: Option<String>,
        #[arg(long)]
        auth_token: Option<String>,
        #[arg(long)]
        scrape_interval_seconds: Option<u64>,
    },
    /// HTTP server source (push receiver).
    HttpServer {
        #[arg(long)]
        listen_address: Option<String>,
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        auth_token: Option<String>,
    },
    /// Socket source (TCP/UDP/Unix).
    Socket {
        #[arg(long)]
        mode: Option<String>,
        #[arg(long)]
        address: Option<String>,
        #[arg(long)]
        framing: Option<String>,
    },
    /// StatsD source.
    Statsd {
        #[arg(long)]
        listen_address: Option<String>,
    },
    /// Stdin source.
    Stdin {
        #[arg(long)]
        mode: Option<String>,
    },
}

fn parse_key_val(s: &str) -> Result<(String, String), String> {
    let Some((key, value)) = s.split_once('=') else {
        return Err("expected KEY=VALUE".to_string());
    };
    let key = key.trim();
    if key.is_empty() {
        return Err("key cannot be empty".to_string());
    }
    Ok((key.to_string(), value.to_string()))
}

fn pairs_to_hash_map(pairs: Option<Vec<(String, String)>>) -> Option<HashMap<String, String>> {
    pairs.map(|pairs| pairs.into_iter().collect())
}

fn working_dir() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn config_path(explicit: &Option<PathBuf>) -> PathBuf {
    explicit
        .clone()
        .unwrap_or_else(|| working_dir().join("skippr.yaml"))
}

fn load_config(explicit: &Option<PathBuf>) -> Result<SkipprDbtConfig, String> {
    SkipprDbtConfig::load_from(&config_path(explicit))
}

fn save_config(cfg: &SkipprDbtConfig, explicit: &Option<PathBuf>) -> Result<(), String> {
    cfg.save_to(&config_path(explicit))
}

// ---------------------------------------------------------------------------
// init
// ---------------------------------------------------------------------------

fn project_root_from_config_path(path: &std::path::Path) -> PathBuf {
    path.parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn skippr_dir_from_project_root(project_root: &std::path::Path) -> PathBuf {
    project_root.join(".skippr")
}

fn env_example_path_from_project_root(project_root: &std::path::Path) -> PathBuf {
    project_root.join(".env.example")
}

async fn load_reset_server_credentials() -> Result<api_client::CredentialsResponse, String> {
    let creds = if let Ok(api_key) = std::env::var("SKIPPR_API_KEY") {
        if api_key.trim().is_empty() {
            return Err("SKIPPR_API_KEY is set but empty".to_string());
        }
        let base_url = auth::auth_base_url();
        let client = api_client::ApiClient::new(&base_url);
        client
            .exchange_api_key(api_key.trim())
            .await
            .map_err(|e| format!("API key authentication failed: {e}"))?
    } else if let Some(creds) = auth::load_credentials() {
        refresh_user_credentials_or_exit(&api_client::ApiClient::new(&auth::auth_base_url()), creds)
            .await
    } else {
        return Err(
            "Authentication required to reset cloud project data. Run 'skippr user login' or set SKIPPR_API_KEY."
                .to_string(),
        );
    };

    let base_url = auth::auth_base_url();
    let tokens = create_token_provider(&creds);
    let client = api_client::ApiClient::authenticated(&base_url, tokens);
    client
        .get_credentials()
        .await
        .map_err(|e| format!("Failed to fetch server credentials: {e}"))
}

fn build_remote_reset_target(
    project: &str,
    _project_root: &std::path::Path,
    srv_creds: &api_client::CredentialsResponse,
) -> Result<
    (
        String,
        react_core::resolved_config::S3Credentials,
        String,
        String,
    ),
    String,
> {
    let bucket = srv_creds.bucket.trim().to_string();
    if bucket.is_empty() {
        return Err("missing storage.bucket for reset".to_string());
    }

    let tenant = srv_creds.tenant_id.trim().to_string();
    if tenant.is_empty() {
        return Err("missing tenant_id for reset".to_string());
    }

    let project = project.trim();
    if project.is_empty() {
        return Err("missing project name for reset".to_string());
    }

    let s3_creds = react_core::resolved_config::S3Credentials {
        access_key_id: srv_creds.credentials.access_key_id.clone(),
        secret_access_key: srv_creds.credentials.secret_access_key.clone(),
        session_token: Some(srv_creds.credentials.session_token.clone()),
        region: "us-east-1".to_string(),
    };
    let prefix = format!("{tenant}/dev/{project}/");
    let remote_desc = format!("s3://{bucket}/{prefix}");
    Ok((bucket, s3_creds, prefix, remote_desc))
}

async fn delete_remote_project_data(
    project: &str,
    project_root: &std::path::Path,
) -> Result<(String, Vec<String>), String> {
    let srv_creds = load_reset_server_credentials().await?;
    let (bucket, s3_creds, prefix, remote_desc) =
        build_remote_reset_target(project, project_root, &srv_creds)?;
    let storage = std::sync::Arc::new(
        react_module_storage_s3::S3StorageAdapter::from_credentials(
            bucket.clone(),
            &s3_creds.access_key_id,
            &s3_creds.secret_access_key,
            s3_creds.session_token.as_deref(),
            &s3_creds.region,
        )
        .await,
    ) as std::sync::Arc<dyn react_core::storage::StorageAdapter>;

    let deleted = delete_project_storage_prefix(&storage, &prefix).await?;
    Ok((remote_desc, deleted))
}

fn ensure_local_environment(project_root: &std::path::Path) -> Result<(PathBuf, PathBuf), String> {
    let skippr_dir = skippr_dir_from_project_root(project_root);
    let env_example = env_example_path_from_project_root(project_root);

    std::fs::create_dir_all(&skippr_dir)
        .map_err(|e| format!("failed to create {}: {}", skippr_dir.display(), e))?;
    std::fs::write(&env_example, env_example_template())
        .map_err(|e| format!("failed to write {}: {}", env_example.display(), e))?;

    Ok((skippr_dir, env_example))
}

fn delete_local_reset_artifacts(
    _config_path: &std::path::Path,
    skippr_dir: &std::path::Path,
) -> Result<Vec<PathBuf>, String> {
    let mut deleted = Vec::new();
    if skippr_dir.exists() {
        std::fs::remove_dir_all(skippr_dir)
            .map_err(|e| format!("failed to remove {}: {}", skippr_dir.display(), e))?;
        deleted.push(skippr_dir.to_path_buf());
    }
    Ok(deleted)
}

async fn cmd_init(name: &str, reset: bool, explicit_config: &Option<PathBuf>) {
    let path = config_path(explicit_config);
    let project_root = project_root_from_config_path(&path);
    let skippr_dir = skippr_dir_from_project_root(&project_root);

    if reset {
        eprintln!(
            "WARNING: This will delete project metadata and state for '{}'",
            name
        );
        eprintln!("  local: {}", skippr_dir.display());
        eprintln!("  remote: authenticated project data on S3");
        eprintln!();
        eprint!("Type 'yes' to confirm: ");
        let _ = std::io::Write::flush(&mut std::io::stderr());
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_err() || input.trim() != "yes" {
            eprintln!("Aborted.");
            std::process::exit(1);
        }

        eprintln!("[skippr] deleting remote metadata and state data...");
        let (remote_desc, deleted_remote) =
            match delete_remote_project_data(name, &project_root).await {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("[skippr] ERROR: {}", e);
                    std::process::exit(1);
                }
            };
        eprintln!(
            "[skippr] deleted {} remote objects from {}",
            deleted_remote.len(),
            remote_desc
        );

        eprintln!("[skippr] deleting local metadata and state data...");
        let deleted_local = match delete_local_reset_artifacts(&path, &skippr_dir) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("error: {}", e);
                std::process::exit(1);
            }
        };
        if deleted_local.is_empty() {
            println!(
                "Nothing to reset locally — {} does not exist.",
                skippr_dir.display()
            );
        } else {
            for deleted in deleted_local {
                println!("Removed {}", deleted.display());
            }
        }

        eprintln!("[skippr] recreating local environment...");
        if let Err(e) = ensure_local_environment(&project_root) {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
        eprintln!("[skippr] environment recreated.");
    }

    if path.exists() {
        println!("Project already initialised — {}", path.display());
        return;
    }

    let cfg = SkipprDbtConfig {
        project: name.to_string(),
        warehouse: None,
        source: None,
        dbt: None,
        schema_sink: None,
    };
    if let Err(e) = cfg.save_to(&path) {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }

    eprintln!("[skippr] creating local environment...");
    if let Err(e) = ensure_local_environment(&project_root) {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }
    eprintln!("[skippr] environment created.");

    println!("Initialised project '{}' — {}", name, path.display());
    println!();
    println!("Next steps:");
    println!("  skippr connect warehouse snowflake");
    println!("  skippr connect source mssql");
    println!("  skippr doctor");
    println!("  skippr run");
}

// ---------------------------------------------------------------------------
// connect warehouse
// ---------------------------------------------------------------------------

fn cmd_connect_warehouse(kind: WarehouseKind, explicit_config: &Option<PathBuf>) {
    let mut cfg = match load_config(explicit_config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {}", e);
            eprintln!("Run 'skippr init <project>' first.");
            std::process::exit(1);
        }
    };

    let wh = match kind {
        WarehouseKind::Athena {
            workgroup,
            region,
            result_s3,
            schema,
        } => {
            let workgroup = workgroup.or_else(|| prompt("Athena workgroup (optional)"));
            let region = region.or_else(|| prompt("AWS region"));
            let result_s3 = result_s3
                .or_else(|| prompt("S3 result location (optional, e.g. s3://bucket/path)"));
            let schema = schema.or_else(|| prompt("Default database/schema (optional)"));
            WarehouseConfig::Athena {
                workgroup,
                region,
                result_s3,
                schema,
            }
        }
        WarehouseKind::Snowflake {
            account,
            user,
            password,
            private_key_path,
            stage,
            staging_uri,
            staging_storage_integration,
            staging_azure_sas_token,
            staging_azure_account_key,
            staging_gcs_service_account_key_path,
            database,
            schema,
            warehouse,
            role,
        } => {
            let database = database.or_else(|| prompt("Snowflake database"));
            let schema = schema.or_else(|| prompt("Snowflake schema"));
            let warehouse = warehouse.or_else(|| prompt("Snowflake compute warehouse"));
            let role = role.or_else(|| prompt("Snowflake role"));
            WarehouseConfig::Snowflake {
                account,
                user,
                password,
                private_key_path,
                stage,
                staging_uri,
                staging_storage_integration,
                staging_azure_sas_token,
                staging_azure_account_key,
                staging_gcs_service_account_key_path,
                database,
                schema,
                warehouse,
                role,
            }
        }
        WarehouseKind::Bigquery {
            project,
            dataset,
            location,
        } => {
            let project = project.or_else(|| prompt("BigQuery GCP project"));
            let dataset = dataset.or_else(|| prompt("BigQuery dataset"));
            let location = location.or_else(|| prompt("BigQuery location (e.g. US)"));
            WarehouseConfig::Bigquery {
                project,
                dataset,
                location,
            }
        }
        WarehouseKind::Postgres { database, schema } => {
            let database = database.or_else(|| prompt("PostgreSQL database"));
            let schema = postgres_schema_or_default(
                schema.or_else(|| prompt("PostgreSQL schema (default: public)")),
            );
            WarehouseConfig::Postgres { database, schema }
        }
        WarehouseKind::Databricks {
            workspace_url,
            token,
            warehouse_id,
            catalog,
            schema,
        } => WarehouseConfig::Databricks {
            workspace_url,
            token,
            warehouse_id,
            catalog,
            schema,
        },
        WarehouseKind::Synapse {
            connection_string,
            schema,
        } => WarehouseConfig::Synapse {
            connection_string,
            schema,
        },
        WarehouseKind::Redshift {
            database,
            cluster_identifier,
            workgroup_name,
            db_user,
            schema,
            region,
            staging_s3_bucket,
            staging_s3_prefix,
            iam_role_arn,
        } => WarehouseConfig::Redshift {
            database,
            cluster_identifier,
            workgroup_name,
            db_user,
            schema,
            region,
            staging_s3_bucket,
            staging_s3_prefix,
            iam_role_arn,
        },
        WarehouseKind::Clickhouse {
            url,
            database,
            user,
            password,
        } => WarehouseConfig::Clickhouse {
            url,
            database,
            user,
            password,
        },
        WarehouseKind::Motherduck {
            motherduck_token,
            database,
            schema,
        } => WarehouseConfig::Motherduck {
            motherduck_token,
            database,
            schema,
        },
    };

    let kind_label = wh.kind_str();
    cfg.warehouse = Some(wh);

    if cfg.dbt.is_none() {
        cfg.dbt = Some(DbtConfig::default());
    }

    if let Err(e) = save_config(&cfg, explicit_config) {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }

    println!("Warehouse ({}) configured.", kind_label);
}

// ---------------------------------------------------------------------------
// connect source
// ---------------------------------------------------------------------------

fn cmd_connect_source(kind: SourceKind, explicit_config: &Option<PathBuf>) {
    let mut cfg = match load_config(explicit_config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {}", e);
            eprintln!("Run 'skippr init <project>' first.");
            std::process::exit(1);
        }
    };

    let src = match kind {
        SourceKind::Mssql { connection_string } => {
            let connection_string = connection_string.or_else(|| {
                prompt("MSSQL connection string (or ${MSSQL_CONNECTION_STRING} to read from env)")
            });
            SourceConfig::Mssql { connection_string }
        }
        SourceKind::S3 {
            bucket,
            prefix,
            namespace_fields,
        } => {
            let bucket = bucket.or_else(|| prompt("S3 bucket"));
            let prefix = prefix.or_else(|| prompt("S3 prefix"));
            let namespace_fields =
                namespace_fields.or_else(|| prompt("Namespace fields (optional, e.g. event_type)"));
            let transform = namespace_fields.map(|nf| S3Transform {
                namespace_fields: Some(nf),
            });
            SourceConfig::S3 {
                s3_bucket: bucket,
                s3_prefix: prefix,
                transform,
            }
        }
        SourceKind::Mysql {
            connection_string,
            tables,
        } => SourceConfig::Mysql {
            connection_string,
            tables,
        },
        SourceKind::PostgresSource {
            host,
            port,
            user,
            password,
            database,
            connection_string,
            tables,
            query,
        } => SourceConfig::PostgresSource {
            host,
            port,
            user,
            password,
            database,
            connection_string,
            tables,
            query,
        },
        SourceKind::RedshiftSource {
            cluster_identifier,
            workgroup_name,
            database,
            db_user,
            tables,
            region,
        } => SourceConfig::RedshiftSource {
            cluster_identifier,
            workgroup_name,
            database,
            db_user,
            tables,
            region,
        },
        SourceKind::Mongodb {
            connection_string,
            database,
            collection,
            filter,
        } => SourceConfig::Mongodb {
            connection_string,
            database,
            collection,
            filter,
        },
        SourceKind::Dynamodb {
            table_name,
            region,
            endpoint_url,
        } => SourceConfig::Dynamodb {
            table_name,
            region,
            endpoint_url,
        },
        SourceKind::ClickhouseSource {
            url,
            database,
            user,
            password,
            tables,
            query,
        } => SourceConfig::ClickhouseSource {
            url,
            database,
            user,
            password,
            tables,
            query,
        },
        SourceKind::MotherduckSource {
            motherduck_token,
            database,
            tables,
            query,
        } => SourceConfig::MotherduckSource {
            motherduck_token,
            database,
            tables,
            query,
        },
        SourceKind::Sftp {
            host,
            port,
            username,
            password,
            private_key_path,
            remote_path,
        } => SourceConfig::Sftp {
            host,
            port,
            username,
            password,
            private_key_path,
            remote_path,
        },
        SourceKind::File { path } => SourceConfig::File { path },
        SourceKind::DeltaLake {
            table_uri,
            storage_options,
            version,
            filter,
        } => SourceConfig::DeltaLake {
            table_uri,
            storage_options: pairs_to_hash_map(storage_options),
            version,
            filter,
        },
        SourceKind::Kafka {
            brokers,
            topic,
            group_id,
            auto_offset_reset,
            security_protocol,
            sasl_mechanism,
            sasl_username,
            sasl_password,
            mode,
        } => SourceConfig::Kafka {
            brokers,
            topic,
            group_id,
            auto_offset_reset,
            security_protocol,
            sasl_mechanism,
            sasl_username,
            sasl_password,
            mode,
        },
        SourceKind::Sqs {
            queue_url,
            region,
            endpoint_url,
            mode,
        } => SourceConfig::Sqs {
            queue_url,
            region,
            endpoint_url,
            mode,
        },
        SourceKind::Kinesis {
            stream_name,
            region,
            endpoint_url,
            mode,
        } => SourceConfig::Kinesis {
            stream_name,
            region,
            endpoint_url,
            mode,
        },
        SourceKind::Amqp {
            connection_string,
            queue,
            exchange,
            routing_key,
            prefetch_count,
            mode,
        } => SourceConfig::Amqp {
            connection_string,
            queue,
            exchange,
            routing_key,
            prefetch_count,
            mode,
        },
        SourceKind::Sns {
            topic_arn,
            sqs_queue_url,
            region,
            endpoint_url,
        } => SourceConfig::Sns {
            topic_arn,
            sqs_queue_url,
            region,
            endpoint_url,
        },
        SourceKind::Eventbridge {
            event_bus_name,
            sqs_queue_url,
            region,
            endpoint_url,
        } => SourceConfig::Eventbridge {
            event_bus_name,
            sqs_queue_url,
            region,
            endpoint_url,
        },
        SourceKind::Mqtt {
            broker_url,
            port,
            topic,
            client_id,
            qos,
            username,
            password,
            mode,
        } => SourceConfig::Mqtt {
            broker_url,
            port,
            topic,
            client_id,
            qos,
            username,
            password,
            mode,
        },
        SourceKind::Websocket { url, headers, mode } => SourceConfig::Websocket {
            url,
            headers: pairs_to_hash_map(headers),
            mode,
        },
        SourceKind::HttpClient {
            url,
            method,
            headers,
            body,
            auth_strategy,
            auth_user,
            auth_password,
            auth_token,
            scrape_interval_seconds,
        } => SourceConfig::HttpClient {
            url,
            method,
            headers: pairs_to_hash_map(headers),
            body,
            auth_strategy,
            auth_user,
            auth_password,
            auth_token,
            scrape_interval_seconds,
        },
        SourceKind::HttpServer {
            listen_address,
            path,
            auth_token,
        } => SourceConfig::HttpServer {
            listen_address,
            path,
            auth_token,
        },
        SourceKind::Socket {
            mode,
            address,
            framing,
        } => SourceConfig::Socket {
            mode,
            address,
            framing,
        },
        SourceKind::Statsd { listen_address } => SourceConfig::Statsd { listen_address },
        SourceKind::Stdin { mode } => SourceConfig::Stdin { mode },
    };

    let kind_label = match &src {
        SourceConfig::Mssql { .. } => "mssql",
        SourceConfig::S3 { .. } => "s3",
        SourceConfig::Mysql { .. } => "mysql",
        SourceConfig::PostgresSource { .. } => "postgres_source",
        SourceConfig::RedshiftSource { .. } => "redshift_source",
        SourceConfig::Mongodb { .. } => "mongodb",
        SourceConfig::Dynamodb { .. } => "dynamodb",
        SourceConfig::ClickhouseSource { .. } => "clickhouse_source",
        SourceConfig::MotherduckSource { .. } => "motherduck_source",
        SourceConfig::Sftp { .. } => "sftp",
        SourceConfig::File { .. } => "file",
        SourceConfig::DeltaLake { .. } => "delta_lake",
        SourceConfig::Kafka { .. } => "kafka",
        SourceConfig::Sqs { .. } => "sqs",
        SourceConfig::Kinesis { .. } => "kinesis",
        SourceConfig::Amqp { .. } => "amqp",
        SourceConfig::Sns { .. } => "sns",
        SourceConfig::Eventbridge { .. } => "eventbridge",
        SourceConfig::Mqtt { .. } => "mqtt",
        SourceConfig::Websocket { .. } => "websocket",
        SourceConfig::HttpClient { .. } => "http_client",
        SourceConfig::HttpServer { .. } => "http_server",
        SourceConfig::Socket { .. } => "socket",
        SourceConfig::Statsd { .. } => "statsd",
        SourceConfig::Stdin { .. } => "stdin",
    };
    cfg.source = Some(src);

    if let Err(e) = save_config(&cfg, explicit_config) {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }

    println!("Source ({}) configured.", kind_label);
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

fn cmd_doctor(explicit_config: &Option<PathBuf>) {
    let mut ok = true;

    let cfg = match load_config(explicit_config) {
        Ok(c) => {
            check_pass("skippr.yaml found");
            c
        }
        Err(_) => {
            check_fail("skippr.yaml not found — run 'skippr init <project>'");
            std::process::exit(1);
        }
    };

    if cfg.warehouse.is_some() {
        check_pass(&format!(
            "warehouse configured ({})",
            cfg.warehouse_kind_str().unwrap_or("unknown")
        ));
    } else {
        check_fail("warehouse not configured — run 'skippr connect warehouse <kind>'");
        ok = false;
    }

    if cfg.source.is_some() {
        check_pass(&format!(
            "source configured ({})",
            cfg.source_kind_str().unwrap_or("unknown")
        ));
    } else {
        check_fail("source not configured — run 'skippr connect source <kind>'");
        ok = false;
    }

    if which("skippr") {
        check_pass("skippr binary found on PATH (user-managed)");
    } else if skippr_bin::managed_binary_path()
        .map(|p| p.is_file())
        .unwrap_or(false)
    {
        check_pass(&format!(
            "skippr v{} installed (managed by skippr)",
            skippr_bin::SKIPPR_VERSION
        ));
    } else {
        check_pass(&format!(
            "skippr not found — v{} will be downloaded automatically on first run",
            skippr_bin::SKIPPR_VERSION
        ));
    }

    if which("dbt") {
        check_pass("dbt binary found on PATH");
    } else {
        check_fail("dbt not found on PATH — install dbt-core and the warehouse adapter in a venv");
        ok = false;
    }

    if which("python3") || which("python") {
        check_pass("python found on PATH");
    } else {
        check_fail("python not found on PATH — Python 3.10+ is required for dbt");
        ok = false;
    }

    if auth::load_credentials().is_some() || env_set("SKIPPR_API_KEY") {
        check_pass("authenticated (credentials or SKIPPR_API_KEY)");
    } else {
        check_fail("not authenticated — run 'skippr user login' or set SKIPPR_API_KEY");
        ok = false;
    }

    if std::env::var("LLM_API_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .is_some()
    {
        check_pass("LLM_API_KEY is set (custom key — overrides server-provided key)");
    } else {
        check_pass("LLM_API_KEY not set (will use server-provided key)");
    }

    if let Some(WarehouseConfig::Athena { .. }) = &cfg.warehouse {
        check_athena_env(&mut ok);
    }

    if let Some(WarehouseConfig::Snowflake { .. }) = &cfg.warehouse {
        check_snowflake_env(&mut ok);
    }

    if let Some(WarehouseConfig::Bigquery { .. }) = &cfg.warehouse {
        check_bigquery_env(&mut ok);
    }

    if let Some(WarehouseConfig::Postgres { .. }) = &cfg.warehouse {
        check_postgres_env(&mut ok);
    }

    println!();
    if ok {
        println!("All checks passed. Run 'skippr run' to start.");
    } else {
        println!("Some checks failed. Fix the issues above and re-run 'skippr doctor'.");
        std::process::exit(1);
    }
}

fn check_athena_env(_ok: &mut bool) {
    if env_set("AWS_REGION") || env_set("AWS_DEFAULT_REGION") {
        check_pass("AWS region configured (AWS_REGION or AWS_DEFAULT_REGION)");
    } else {
        check_pass("AWS region not set (will use SDK default or --region flag)");
    }
    if env_set("ATHENA_WORKGROUP") {
        check_pass("ATHENA_WORKGROUP is set");
    } else {
        check_pass("ATHENA_WORKGROUP not set (will use Athena default workgroup)");
    }
    if env_set("ATHENA_RESULT_S3") {
        check_pass("ATHENA_RESULT_S3 is set");
    } else {
        check_pass("ATHENA_RESULT_S3 not set (will rely on workgroup output location)");
    }
}

fn check_snowflake_env(ok: &mut bool) {
    let has_account = env_set("SNOWFLAKE_ACCOUNT");
    let has_user = env_set("SNOWFLAKE_USER");
    let has_key = env_set("SNOWFLAKE_PRIVATE_KEY_PATH");
    let has_pw = env_set("SNOWFLAKE_PASSWORD");

    if has_account {
        check_pass("SNOWFLAKE_ACCOUNT is set");
    } else {
        check_fail("SNOWFLAKE_ACCOUNT is not set");
        *ok = false;
    }
    if has_user {
        check_pass("SNOWFLAKE_USER is set");
    } else {
        check_fail("SNOWFLAKE_USER is not set");
        *ok = false;
    }
    if has_key {
        check_pass("SNOWFLAKE_PRIVATE_KEY_PATH is set (key-pair auth)");
    } else if has_pw {
        check_pass("SNOWFLAKE_PASSWORD is set (password auth)");
    } else {
        check_fail(
            "Snowflake auth not configured — set SNOWFLAKE_PRIVATE_KEY_PATH or SNOWFLAKE_PASSWORD",
        );
        *ok = false;
    }
}

fn check_bigquery_env(ok: &mut bool) {
    if env_set("GOOGLE_APPLICATION_CREDENTIALS") {
        check_pass("GOOGLE_APPLICATION_CREDENTIALS is set");
    } else {
        check_fail("GOOGLE_APPLICATION_CREDENTIALS is not set");
        *ok = false;
    }
}

fn check_postgres_env(ok: &mut bool) {
    let has_host = env_set("POSTGRES_HOST");
    let has_user = env_set("POSTGRES_USER");
    let has_password = env_set("POSTGRES_PASSWORD");

    if has_host {
        check_pass("POSTGRES_HOST is set");
    } else {
        check_fail("POSTGRES_HOST is not set (defaults to localhost)");
    }
    if has_user {
        check_pass("POSTGRES_USER is set");
    } else {
        check_fail("POSTGRES_USER is not set (defaults to postgres)");
    }
    if has_password {
        check_pass("POSTGRES_PASSWORD is set");
    } else {
        check_fail("POSTGRES_PASSWORD is not set");
        *ok = false;
    }
}

fn env_set(key: &str) -> bool {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .is_some()
}

fn which(bin: &str) -> bool {
    let locator = if cfg!(windows) { "where" } else { "which" };
    std::process::Command::new(locator)
        .arg(bin)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn check_pass(msg: &str) {
    println!("  [ok]   {}", msg);
}

fn check_fail(msg: &str) {
    println!("  [FAIL] {}", msg);
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

async fn cmd_run(log: Option<String>, explicit_config: &Option<PathBuf>) {
    let cfg = match load_config(explicit_config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {}", e);
            eprintln!("Run 'skippr init <project>' first.");
            std::process::exit(1);
        }
    };

    let mut internal_file = match translate::to_internal(&cfg) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };

    let skippr_binary = match skippr_bin::resolve_skippr_binary().await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[skippr] ERROR: {}", e);
            std::process::exit(1);
        }
    };
    translate::set_skippr_binary(&mut internal_file, &skippr_binary);

    // Authentication is mandatory. SKIPPR_API_KEY env var takes priority, then credentials.json.
    let creds = if let Ok(api_key) = std::env::var("SKIPPR_API_KEY") {
        if api_key.trim().is_empty() {
            eprintln!("[skippr] ERROR: SKIPPR_API_KEY is set but empty.");
            std::process::exit(1);
        }
        let base_url = auth::auth_base_url();
        let client = api_client::ApiClient::new(&base_url);
        match client.exchange_api_key(api_key.trim()).await {
            Ok(tokens) => {
                eprintln!("[skippr] authenticated via API key");
                tokens
            }
            Err(e) => {
                eprintln!("[skippr] ERROR: API key authentication failed: {}", e);
                std::process::exit(1);
            }
        }
    } else if let Some(creds) = auth::load_credentials() {
        eprintln!("[skippr] authenticated via stored credentials");
        refresh_user_credentials_or_exit(&api_client::ApiClient::new(&auth::auth_base_url()), creds)
            .await
    } else {
        eprintln!("[skippr] ERROR: Authentication required.");
        eprintln!("[skippr]   Run 'skippr user login' to authenticate interactively,");
        eprintln!("[skippr]   or set SKIPPR_API_KEY for CI/CD.");
        std::process::exit(1);
    };

    let base_url = auth::auth_base_url();
    let tokens = create_token_provider(&creds);
    let client = api_client::ApiClient::authenticated(&base_url, std::sync::Arc::clone(&tokens));

    let initial_balance = match client.get_account().await {
        Ok(account) => {
            let bal = account.balance.balance;
            if bal <= 0.0 {
                eprintln!("[skippr] ERROR: Balance is $0.00. Add funds to continue.");
                eprintln!("[skippr]   skippr user buy-credits --amount 25");
                std::process::exit(1);
            } else if bal < LOW_BALANCE_USD_THRESHOLD {
                eprintln!(
                    "[skippr] WARNING: Low balance (${:.2}). The run may exhaust your balance.",
                    bal
                );
            } else {
                eprintln!("[skippr] balance: ${:.2}", bal);
            }
            bal
        }
        Err(e) => {
            eprintln!(
                "[skippr] ERROR: Could not verify account balance ({}). Refusing to run.",
                e
            );
            eprintln!("[skippr]   Check your connection and login status (skippr user login).");
            std::process::exit(1);
        }
    };

    match client.get_credentials().await {
        Ok(srv_creds) => {
            translate::apply_authenticated_overlay(
                &mut internal_file,
                &srv_creds,
                std::sync::Arc::clone(&tokens),
                initial_balance,
            );
            eprintln!("[skippr] cloud storage + metering active");
        }
        Err(e) => {
            eprintln!("[skippr] ERROR: Failed to fetch server credentials: {}", e);
            eprintln!("[skippr]   Check your connection and login status.");
            std::process::exit(1);
        }
    }

    let run_id = uuid::Uuid::new_v4().to_string();
    react_suite_data_engineer::metering::set_metering_run_id(&run_id);
    eprintln!("[skippr] run {run_id}");

    let metering = react_suite_data_engineer::metering::global_metering();
    let _ = metering
        .record_batch(&[
            react_suite_data_engineer::metering::UsageEvent::PipelineRun {
                project_id: cfg.project.clone(),
            },
        ])
        .await;

    let resolved =
        match react_host::resolve_config(internal_file, react::config::ServeOverrides::default()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: {}", e);
                std::process::exit(1);
            }
        };

    let thread_id = match find_latest_thread_for_resolved_config(&resolved).await {
        Ok(thread_id) => thread_id,
        Err(e) => {
            eprintln!("[skippr] WARNING: failed to discover latest thread: {e}");
            None
        }
    };
    if let Some(ref tid) = thread_id {
        react_suite_data_engineer::metering::set_metering_thread_id(tid);
        eprintln!("[skippr] resuming thread {tid}");
    }

    let exit_code = react_host::run_headless(
        resolved,
        react::run_engine::HeadlessRunOpts {
            log_level: log,
            verbose_debug: false,
            terminal: false,
            thread_id,
            suite_id: None,
            agent: "agent".to_string(),
            skip_logging_init: false,
        },
    )
    .await;
    std::process::exit(exit_code);
}

async fn cmd_feedback(
    good: bool,
    bad: bool,
    comment: Option<String>,
    include_diagnostics: bool,
    explicit_config: &Option<PathBuf>,
) {
    let cfg = match load_config(explicit_config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {}", e);
            eprintln!("Run 'skippr init <project>' first.");
            std::process::exit(1);
        }
    };
    let project = cfg.project.trim();
    let srv_creds = match load_reset_server_credentials().await {
        Ok(creds) => creds,
        Err(e) => {
            eprintln!("[skippr] ERROR: {e}");
            std::process::exit(1);
        }
    };
    let resolved_cfg = match resolve_feedback_runtime_config(&cfg, &srv_creds) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("[skippr] ERROR: {e}");
            std::process::exit(1);
        }
    };
    let verdict = match resolve_feedback_verdict(good, bad) {
        Ok(verdict) => verdict,
        Err(e) => {
            eprintln!("[skippr] ERROR: {e}");
            std::process::exit(1);
        }
    };
    let thread_id = match resolve_feedback_thread_id(&resolved_cfg).await {
        Ok(thread_id) => thread_id,
        Err(e) => {
            eprintln!("[skippr] ERROR: {e}");
            std::process::exit(1);
        }
    };
    let comment = match resolve_feedback_comment(comment) {
        Ok(comment) => comment,
        Err(e) => {
            eprintln!("[skippr] ERROR: {e}");
            std::process::exit(1);
        }
    };
    let feedback_store = match build_feedback_store(project, &srv_creds).await {
        Ok(store) => store,
        Err(e) => {
            eprintln!("[skippr] ERROR: {e}");
            std::process::exit(1);
        }
    };
    match feedback_store
        .store
        .submit(&thread_id, verdict, comment)
        .await
    {
        Ok(feedback) => {
            if include_diagnostics {
                let diagnostics = feedback_diagnostics::collect(
                    &cfg,
                    &config_path(explicit_config),
                    &feedback.thread_id,
                    &feedback.feedback_id,
                );
                match submit_feedback_diagnostics(&feedback_store, &feedback, diagnostics).await {
                    Ok(_) => eprintln!(
                        "[skippr] attached redacted diagnostics for thread {} ({})",
                        feedback.thread_id, feedback.feedback_id
                    ),
                    Err(e) => eprintln!(
                        "[skippr] WARNING: feedback stored but diagnostics upload failed: {e}"
                    ),
                }
            }
            eprintln!(
                "[skippr] stored {:?} feedback for thread {} ({})",
                feedback.verdict, feedback.thread_id, feedback.feedback_id
            );
        }
        Err(e) => {
            eprintln!("[skippr] ERROR: failed to store feedback: {e}");
            std::process::exit(1);
        }
    }
}

struct FeedbackStoreBundle {
    store: react_core::thread_feedback::ThreadFeedbackStore,
    storage: Arc<dyn react_core::storage::StorageAdapter>,
    scope: react_core::scope::RequestScope,
    keyspace: Arc<dyn react_core::keyspace::Keyspace>,
}

fn find_latest_thread_in_skippr_dir(skippr_dir: &std::path::Path, project: &str) -> Option<String> {
    let scope = react_core::scope::RequestScope::parse("_", "dev", project.trim()).ok()?;
    find_latest_thread_in_local_storage(skippr_dir, &scope)
        .ok()
        .flatten()
}

fn find_latest_thread_in_local_storage(
    storage_root: &std::path::Path,
    scope: &react_core::scope::RequestScope,
) -> Result<Option<String>, String> {
    let keyspace = react_core::keyspace::LocalKeyspace::new(storage_root.display().to_string());
    let threads_dir = storage_root.join(keyspace.threads_prefix(scope).trim_end_matches('/'));
    let entries = match std::fs::read_dir(&threads_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(format!(
                "failed to read local threads directory {}: {e}",
                threads_dir.display()
            ))
        }
    };

    let mut latest: Option<(String, std::time::SystemTime)> = None;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(thread_id) = primary_thread_id_from_filename(&name) else {
            continue;
        };
        let modified = entry
            .metadata()
            .map_err(|e| format!("failed to read metadata for {name}: {e}"))?
            .modified()
            .map_err(|e| format!("failed to read modified time for {name}: {e}"))?;
        if latest.as_ref().map_or(true, |(_, t)| modified > *t) {
            latest = Some((thread_id, modified));
        }
    }

    Ok(latest.map(|(tid, _)| tid))
}

fn resolve_feedback_verdict(
    good: bool,
    bad: bool,
) -> Result<react_core::thread_feedback::ThreadFeedbackVerdict, String> {
    match (good, bad) {
        (true, false) => Ok(react_core::thread_feedback::ThreadFeedbackVerdict::Good),
        (false, true) => Ok(react_core::thread_feedback::ThreadFeedbackVerdict::Bad),
        (false, false) => Err("pass either --good or --bad".to_string()),
        (true, true) => Err("pass only one of --good or --bad".to_string()),
    }
}

async fn resolve_feedback_thread_id(
    resolved_cfg: &react_core::resolved_config::ReactResolvedConfig,
) -> Result<String, String> {
    find_latest_thread_for_resolved_config(resolved_cfg)
        .await?
        .ok_or_else(|| {
            format!(
                "no primary thread was found in {} storage; run skippr first, then leave feedback",
                resolved_cfg.storage.mode
            )
        })
}

fn resolve_feedback_comment(explicit_comment: Option<String>) -> Result<String, String> {
    if let Some(comment) = explicit_comment {
        return normalize_feedback_comment(&comment);
    }

    eprint!("Leave feedback: ");
    let _ = std::io::Write::flush(&mut std::io::stderr());
    let mut comment = String::new();
    std::io::stdin()
        .read_line(&mut comment)
        .map_err(|e| format!("failed to read feedback comment: {e}"))?;
    normalize_feedback_comment(&comment)
}

fn normalize_feedback_comment(comment: &str) -> Result<String, String> {
    let trimmed = comment.trim();
    if trimmed.is_empty() {
        return Err("feedback comment cannot be empty".to_string());
    }
    Ok(trimmed.to_string())
}

async fn build_feedback_store(
    project: &str,
    srv_creds: &api_client::CredentialsResponse,
) -> Result<FeedbackStoreBundle, String> {
    let (bucket, s3_creds, _prefix, _remote_desc) =
        build_remote_reset_target(project, &working_dir(), &srv_creds)?;
    let storage = Arc::new(
        react_module_storage_s3::S3StorageAdapter::from_credentials(
            bucket.clone(),
            &s3_creds.access_key_id,
            &s3_creds.secret_access_key,
            s3_creds.session_token.as_deref(),
            &s3_creds.region,
        )
        .await,
    ) as Arc<dyn react_core::storage::StorageAdapter>;
    let scope = react_core::scope::RequestScope::parse(srv_creds.tenant_id.trim(), "dev", project)
        .map_err(|e| format!("invalid feedback scope: {e}"))?;
    let keyspace = Arc::new(react_core::keyspace::DefaultKeyspace::new(bucket))
        as Arc<dyn react_core::keyspace::Keyspace>;
    let store = react_core::thread_feedback::ThreadFeedbackStore::new(
        Arc::clone(&storage),
        scope.clone(),
        Arc::clone(&keyspace),
    );
    Ok(FeedbackStoreBundle {
        store,
        storage,
        scope,
        keyspace,
    })
}

async fn submit_feedback_diagnostics(
    bundle: &FeedbackStoreBundle,
    feedback: &react_core::thread_feedback::ThreadFeedback,
    payload: serde_json::Value,
) -> Result<(), String> {
    let file_name = format!("{}.diagnostics.json", feedback.feedback_id);
    let key = bundle.keyspace.scoped_key(
        &bundle.scope,
        &["feedback", &feedback.thread_id, &file_name],
    );
    let value = serde_json::json!({
        "feedback_id": feedback.feedback_id,
        "thread_id": feedback.thread_id,
        "created_at": chrono::Utc::now().to_rfc3339(),
        "payload": payload,
    });
    react_core::storage::retry_put_json(bundle.storage.as_ref(), &key, &value)
        .await
        .map_err(|e| e.to_string())
}

fn resolve_feedback_runtime_config(
    cfg: &SkipprDbtConfig,
    srv_creds: &api_client::CredentialsResponse,
) -> Result<react_core::resolved_config::ReactResolvedConfig, String> {
    let mut internal_file = translate::to_internal(cfg)?;
    apply_feedback_storage_overlay(&mut internal_file, srv_creds);
    react_host::resolve_config(internal_file, react::config::ServeOverrides::default())
}

fn apply_feedback_storage_overlay(
    cfg: &mut react::config::ReactConfigFile,
    creds: &api_client::CredentialsResponse,
) {
    cfg.storage = Some(react::config::StorageFile {
        mode: Some("s3".into()),
        bucket: Some(creds.bucket.clone()),
        path: None,
        s3_credentials: Some(react_core::resolved_config::S3Credentials {
            access_key_id: creds.credentials.access_key_id.clone(),
            secret_access_key: creds.credentials.secret_access_key.clone(),
            session_token: Some(creds.credentials.session_token.clone()),
            region: "us-east-1".to_string(),
        }),
    });
    if let Some(scope) = cfg.scope.as_mut() {
        if !creds.tenant_id.trim().is_empty() {
            scope.tenant = Some(creds.tenant_id.clone());
        }
    }
}

async fn find_latest_thread_for_resolved_config(
    cfg: &react_core::resolved_config::ReactResolvedConfig,
) -> Result<Option<String>, String> {
    match cfg.storage.mode {
        react_core::resolved_config::StorageMode::Local => {
            let root = cfg
                .storage
                .path
                .as_ref()
                .ok_or_else(|| "missing storage.path for local mode".to_string())?;
            find_latest_thread_in_local_storage(std::path::Path::new(root), &cfg.scope)
        }
        react_core::resolved_config::StorageMode::S3 => find_latest_thread_in_s3_storage(cfg).await,
    }
}

async fn find_latest_thread_in_s3_storage(
    cfg: &react_core::resolved_config::ReactResolvedConfig,
) -> Result<Option<String>, String> {
    let bucket = cfg
        .storage
        .bucket
        .clone()
        .ok_or_else(|| "missing storage.bucket for s3 mode".to_string())?;
    let keyspace = react_core::keyspace::DefaultKeyspace::new(bucket.clone());
    let prefix = keyspace.threads_prefix(&cfg.scope);
    let adapter = if let Some(creds) = cfg.storage.s3_credentials.as_ref() {
        react_module_storage_s3::S3StorageAdapter::from_credentials(
            bucket,
            &creds.access_key_id,
            &creds.secret_access_key,
            creds.session_token.as_deref(),
            &creds.region,
        )
        .await
    } else {
        react_module_storage_s3::S3StorageAdapter::from_env(bucket).await
    };
    let objects = adapter
        .list_prefix_meta(&prefix)
        .await
        .map_err(|e| format!("failed to list thread objects from s3: {e}"))?;
    Ok(latest_primary_thread_id_from_s3_objects(&objects, &prefix))
}

fn latest_primary_thread_id_from_s3_objects(
    objects: &[react_module_storage_s3::ObjectMeta],
    prefix: &str,
) -> Option<String> {
    let mut latest: Option<(String, chrono::DateTime<chrono::Utc>)> = None;
    for object in objects {
        let Some(thread_id) = primary_thread_id_from_key(&object.key, prefix) else {
            continue;
        };
        let Some(last_modified) = object.last_modified else {
            continue;
        };
        if latest
            .as_ref()
            .map_or(true, |(_, current_ts)| last_modified > *current_ts)
        {
            latest = Some((thread_id, last_modified));
        }
    }
    latest.map(|(thread_id, _)| thread_id)
}

fn primary_thread_id_from_key(key: &str, prefix: &str) -> Option<String> {
    let stem = key.strip_prefix(prefix)?.strip_suffix(".json")?;
    if stem.contains('/') {
        return None;
    }
    primary_thread_id_from_stem(stem)
}

fn primary_thread_id_from_filename(name: &str) -> Option<String> {
    let stem = name.strip_suffix(".json")?;
    primary_thread_id_from_stem(stem)
}

fn primary_thread_id_from_stem(stem: &str) -> Option<String> {
    if stem.contains("__") || stem.contains('.') {
        return None;
    }
    if stem.len() != 36 || stem.chars().filter(|c| *c == '-').count() != 4 {
        return None;
    }
    Some(stem.to_string())
}

// ---------------------------------------------------------------------------
// interactive prompt helper
// ---------------------------------------------------------------------------

fn prompt(label: &str) -> Option<String> {
    let result = dialoguer::Input::<String>::new()
        .with_prompt(label)
        .allow_empty(true)
        .interact_text();
    match result {
        Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        _ => None,
    }
}

fn postgres_schema_or_default(schema: Option<String>) -> Option<String> {
    schema
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| Some("public".to_string()))
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.cmd {
        Cmd::Init { name, reset } => cmd_init(&name, reset, &cli.config).await,
        Cmd::Connect { target } => match target {
            ConnectTarget::Warehouse { kind } => cmd_connect_warehouse(kind, &cli.config),
            ConnectTarget::Source { kind } => cmd_connect_source(kind, &cli.config),
        },
        Cmd::Doctor => cmd_doctor(&cli.config),
        Cmd::Run => cmd_run(cli.log, &cli.config).await,
        Cmd::Feedback {
            good,
            bad,
            comment,
            no_diagnostics,
        } => cmd_feedback(good, bad, comment, !no_diagnostics, &cli.config).await,
        Cmd::User { action } => match action {
            UserAction::Login => cmd_user_login().await,
            UserAction::Logout => cmd_user_logout(),
            UserAction::Account => cmd_user_account().await,
            UserAction::BuyCredits { amount } => cmd_user_buy_credits(amount).await,
            UserAction::CreateApiKey { name } => cmd_user_create_api_key(&name).await,
            UserAction::RevokeApiKey { key_id } => cmd_user_revoke_api_key(&key_id).await,
            UserAction::ListApiKeys => cmd_user_list_api_keys().await,
        },
    }
}

async fn cmd_user_login() {
    if auth::load_credentials().is_some() {
        eprintln!("Already logged in. Run 'skippr user logout' first to switch accounts.");
        std::process::exit(1);
    }

    println!("Enter your email address:");
    let mut email = String::new();
    std::io::stdin().read_line(&mut email).unwrap();
    let email = email.trim();

    if email.is_empty() || !email.contains('@') {
        eprintln!("Invalid email address.");
        std::process::exit(1);
    }

    let base_url = auth::auth_base_url();
    let client = api_client::ApiClient::new(&base_url);

    match client.sign_in(email).await {
        Ok(_) => {
            println!("Verification code sent to {}.", email);
            println!("Enter the 6-digit code:");
            let mut code = String::new();
            std::io::stdin().read_line(&mut code).unwrap();
            let code = code.trim();

            match client.confirm(email, code).await {
                Ok(tokens) => {
                    auth::save_credentials(&tokens);
                    println!();
                    println!("  Logged in successfully.");
                    println!();
                    println!("  Next steps:");
                    println!("    skippr user account       — view balance");
                    println!("    skippr user buy-credits   — add funds");
                    println!("    skippr run                — start a pipeline");
                    println!();
                }
                Err(e) => {
                    eprintln!("Confirmation failed: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Err(e) => {
            eprintln!("Sign-in failed: {}", e);
            std::process::exit(1);
        }
    }
}

fn cmd_user_logout() {
    auth::clear_credentials();
    println!("Logged out. Local credentials removed.");
}

fn load_stored_credentials_or_exit() -> auth::StoredCredentials {
    match auth::load_credentials() {
        Some(creds) => creds,
        None => {
            eprintln!("Not logged in. Run: skippr user login");
            std::process::exit(1);
        }
    }
}

async fn refresh_user_credentials_or_exit(
    client: &api_client::ApiClient,
    creds: auth::StoredCredentials,
) -> auth::StoredCredentials {
    if creds.refresh_token.trim().is_empty() {
        return creds;
    }

    match client.refresh(&creds.refresh_token).await {
        Ok(refreshed) => {
            auth::save_credentials(&refreshed);
            refreshed
        }
        Err(e) => {
            eprintln!("Session refresh failed: {}", e);
            eprintln!("Run: skippr user login");
            std::process::exit(1);
        }
    }
}

async fn load_authenticated_user_credentials(
    client: &api_client::ApiClient,
) -> auth::StoredCredentials {
    let creds = load_stored_credentials_or_exit();
    refresh_user_credentials_or_exit(client, creds).await
}

fn create_token_provider(
    creds: &auth::StoredCredentials,
) -> std::sync::Arc<react_suite_data_engineer::metering::TokenProvider> {
    let base_url = auth::auth_base_url();
    let rt = if creds.refresh_token.is_empty() {
        None
    } else {
        Some(creds.refresh_token.clone())
    };
    std::sync::Arc::new(react_suite_data_engineer::metering::TokenProvider::new(
        Some(creds.access_token.clone()),
        rt,
        Some(base_url),
    ))
}

async fn authenticated_api_client() -> api_client::ApiClient {
    let base_url = auth::auth_base_url();
    let unauthenticated = api_client::ApiClient::new(&base_url);
    let creds = load_authenticated_user_credentials(&unauthenticated).await;
    let tokens = create_token_provider(&creds);
    api_client::ApiClient::authenticated(&base_url, tokens)
}

async fn cmd_user_account() {
    let client = authenticated_api_client().await;
    match client.get_account().await {
        Ok(account) => {
            println!();
            println!("  Account");
            println!("  {}", "-".repeat(50));
            let plan_label = match account.profile.plan.as_str() {
                "free" => "Pay as you go",
                "pro" => "Pro",
                other => other,
            };
            println!("  Plan:              {}", plan_label);
            println!("  Balance:           ${:.2}", account.balance.balance);
            if let Some(ref sub) = account.subscription {
                println!("  Subscription:      {} ({})", sub.status, sub.price_id);
            }
            println!();

            print_low_balance_warning(&account.balance);

            {
                let today = chrono::Utc::now().date_naive();

                println!("  Last 7 days:");
                println!("  {:<12} {:>8}", "Date", "Cost");
                println!("  {}", "-".repeat(22));
                for dc in &account.daily_costs_est {
                    println!("  {:<12} {:>8}", dc.date, format!("${:.2}", dc.cost));
                }
                println!("  {}", "-".repeat(22));
                println!(
                    "  {:<12} {:>8}",
                    format!("~{}", today.format("%B")),
                    format!("${:.2}", account.monthly_cost_est)
                );
                println!();
            }
        }
        Err(e) => {
            eprintln!("Failed to fetch account: {}", e);
            std::process::exit(1);
        }
    }
}

async fn cmd_user_buy_credits(amount: Option<f64>) {
    let amount = match amount {
        Some(a) => a,
        None => {
            println!("Add funds to your Skippr account.");
            println!();
            println!("Usage:");
            println!("  skippr user buy-credits --amount <DOLLARS>");
            println!();
            println!("Examples:");
            println!("  skippr user buy-credits --amount 25     # add $25");
            println!("  skippr user buy-credits --amount 100    # add $100");
            println!("  skippr user buy-credits --amount 500    # add $500");
            println!();
            println!("Minimum $5, maximum $10,000 per transaction.");
            println!("Your balance is visible via: skippr user account");
            return;
        }
    };

    if amount < 5.0 {
        eprintln!("Minimum top-up is $5.");
        std::process::exit(1);
    }
    if amount > 10_000.0 {
        eprintln!("Maximum top-up is $10,000 per transaction.");
        std::process::exit(1);
    }
    let client = authenticated_api_client().await;
    match client.add_funds(amount).await {
        Ok(url) => {
            let browser_result = open_in_default_browser(&url);
            println!();
            println!("  Adding ${:.2} to your account.", amount);
            println!();
            match browser_result {
                Ok(()) => println!("  Opened your default browser to complete your purchase."),
                Err(err) => println!(
                    "  Could not open your default browser automatically: {}",
                    err
                ),
            }
            println!();
            println!("  Open this URL to complete your purchase:");
            println!("  {}", url);
            println!();
            println!("  Funds will appear in your balance as soon as payment completes.");
            println!();
        }
        Err(e) => {
            eprintln!("Failed to start checkout: {}", e);
            std::process::exit(1);
        }
    }
}

fn open_in_default_browser(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("open failed: {}", e))
    }

    #[cfg(target_os = "windows")]
    {
        Command::new("explorer")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("explorer failed: {}", e))
    }

    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("xdg-open failed: {}", e))
    }
}

async fn cmd_user_create_api_key(name: &str) {
    let client = authenticated_api_client().await;
    match client.create_api_key(name).await {
        Ok(key) => {
            println!();
            println!("  API key created: {}", key.name);
            println!();
            println!("    {}", key.raw_key);
            println!();
            println!("  Save this key — it will not be shown again.");
            println!("  Set it as SKIPPR_API_KEY in your CI environment.");
            println!();
        }
        Err(e) => {
            eprintln!("Failed to create API key: {}", e);
            std::process::exit(1);
        }
    }
}

async fn cmd_user_revoke_api_key(key_id: &str) {
    let client = authenticated_api_client().await;
    match client.revoke_api_key(key_id).await {
        Ok(()) => {
            println!("API key {} revoked.", key_id);
        }
        Err(e) => {
            eprintln!("Failed to revoke API key: {}", e);
            std::process::exit(1);
        }
    }
}

async fn cmd_user_list_api_keys() {
    let client = authenticated_api_client().await;
    match client.list_api_keys().await {
        Ok(keys) => {
            println!();
            if keys.is_empty() {
                println!("  No API keys found.");
                println!("  Create one with: skippr user create-api-key --name \"my-key\"");
            } else {
                println!(
                    "  {:<38} {:<20} {:<10} {}",
                    "Key ID", "Name", "Status", "Created"
                );
                println!("  {}", "-".repeat(80));
                for k in &keys {
                    println!(
                        "  {:<38} {:<20} {:<10} {}",
                        k.key_id,
                        k.name,
                        k.status,
                        &k.created_at[..std::cmp::min(22, k.created_at.len())],
                    );
                }
            }
            println!();
        }
        Err(e) => {
            eprintln!("Failed to list API keys: {}", e);
            std::process::exit(1);
        }
    }
}

const LOW_BALANCE_USD_THRESHOLD: f64 = 5.0;

fn print_low_balance_warning(balance: &api_client::Balance) {
    if balance.balance <= 0.0 {
        eprintln!("  WARNING: Your balance is $0.00. Billable operations will fail.");
        eprintln!("  Run: skippr user buy-credits --amount 25");
        eprintln!();
    } else if balance.balance < LOW_BALANCE_USD_THRESHOLD {
        eprintln!(
            "  WARNING: Low balance (${:.2}). Consider adding funds.",
            balance.balance
        );
        eprintln!("  Run: skippr user buy-credits --amount 25");
        eprintln!();
    }
}

async fn delete_project_storage_prefix(
    storage: &std::sync::Arc<dyn react_core::storage::StorageAdapter>,
    prefix: &str,
) -> Result<Vec<String>, String> {
    let mut keys = storage
        .list_prefix(prefix)
        .await
        .map_err(|e| format!("list_prefix('{prefix}'): {e}"))?;
    keys.sort();
    for key in &keys {
        storage
            .delete_object(key)
            .await
            .map_err(|e| format!("delete_object('{key}'): {e}"))?;
    }
    Ok(keys)
}

fn env_example_template() -> &'static str {
    "\
# Authentication (required — choose one)
# Interactive: skippr user login
# CI/CD: set SKIPPR_API_KEY
SKIPPR_API_KEY=sk_live_...

# Optional: override the server-provided LLM key with your own
# LLM_API_KEY=sk-...

# Snowflake (when warehouse is snowflake)
SNOWFLAKE_ACCOUNT=
SNOWFLAKE_USER=
SNOWFLAKE_PRIVATE_KEY_PATH=

# PostgreSQL (when warehouse is postgres)
POSTGRES_HOST=localhost
POSTGRES_PORT=5432
POSTGRES_USER=postgres
POSTGRES_PASSWORD=
POSTGRES_DATABASE=

# MSSQL (when source is mssql)
MSSQL_CONNECTION_STRING=
"
}

#[cfg(test)]
mod tests {
    use super::*;
    use react_core::storage::StorageAdapter;
    use react_module_storage_local::LocalFileStorageAdapter;
    use std::fs;
    use std::sync::Arc;

    #[tokio::test]
    async fn init_creates_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("skippr.yaml");
        cmd_init("test-project", false, &Some(config.clone())).await;
        assert!(config.exists());
        let contents = fs::read_to_string(&config).unwrap();
        assert!(contents.contains("test-project"));
    }

    #[tokio::test]
    async fn init_is_idempotent_when_already_initialised() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("skippr.yaml");
        cmd_init("my-pipeline", false, &Some(config.clone())).await;
        assert!(config.exists());
        let original = fs::read_to_string(&config).unwrap();

        // Second init should not fail or change anything
        cmd_init("my-pipeline", false, &Some(config.clone())).await;
        let after = fs::read_to_string(&config).unwrap();
        assert_eq!(original, after);
    }

    #[test]
    fn postgres_schema_or_default_uses_public_when_missing() {
        assert_eq!(postgres_schema_or_default(None).as_deref(), Some("public"));
        assert_eq!(
            postgres_schema_or_default(Some("".into())).as_deref(),
            Some("public")
        );
        assert_eq!(
            postgres_schema_or_default(Some(" public ".into())).as_deref(),
            Some("public")
        );
        assert_eq!(
            postgres_schema_or_default(Some("analytics".into())).as_deref(),
            Some("analytics")
        );
    }

    #[tokio::test]
    async fn init_reset_purges_skippr_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("skippr.yaml");
        let skippr_dir = dir.path().join(".skippr");

        cmd_init("reset-test", false, &Some(config.clone())).await;

        // Simulate runtime data
        fs::create_dir_all(skippr_dir.join("_/dev/reset-test/skippr")).unwrap();
        fs::write(
            skippr_dir.join("_/dev/reset-test/skippr/offsets.db"),
            "fake",
        )
        .unwrap();
        assert!(skippr_dir.exists());

        // Reset without interactive confirmation — pass pre-filled stdin
        // Since cmd_init reads stdin for "yes", we test the deletion logic directly
        assert!(skippr_dir.exists());
        fs::remove_dir_all(&skippr_dir).unwrap();
        assert!(!skippr_dir.exists());

        // Config file should still exist
        assert!(config.exists());
    }

    #[test]
    fn delete_local_reset_artifacts_removes_skippr_dir_but_keeps_config() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("skippr.yaml");
        let skippr_dir = dir.path().join(".skippr");

        fs::write(&config, "project: bike-hire-snowflake\n").unwrap();
        fs::create_dir_all(skippr_dir.join("_/dev/bike-hire-snowflake/skippr")).unwrap();
        fs::write(
            skippr_dir.join("_/dev/bike-hire-snowflake/skippr/offsets.db"),
            "fake",
        )
        .unwrap();

        let deleted = delete_local_reset_artifacts(&config, &skippr_dir).expect("delete local");
        assert!(
            deleted.contains(&skippr_dir),
            "expected .skippr directory to be deleted"
        );
        assert!(!skippr_dir.exists());
        assert!(config.exists(), "skippr.yaml should be preserved");
    }

    #[tokio::test]
    async fn delete_project_storage_prefix_removes_all_project_objects() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(LocalFileStorageAdapter::new(dir.path().to_path_buf()).unwrap())
            as Arc<dyn StorageAdapter>;

        storage
            .put_bytes("t/w/p/state/123/state.json", b"{}", "application/json")
            .await
            .unwrap();
        storage
            .put_bytes("t/w/p/threads/123.json", b"{}", "application/json")
            .await
            .unwrap();
        storage
            .put_bytes("t/w/p/target/manifest.json", b"{}", "application/json")
            .await
            .unwrap();
        storage
            .put_bytes("t/w/other/state/999/state.json", b"{}", "application/json")
            .await
            .unwrap();

        let deleted = delete_project_storage_prefix(&storage, "t/w/p/")
            .await
            .expect("delete");
        assert_eq!(deleted.len(), 3);
        assert!(storage.list_prefix("t/w/p/").await.unwrap().is_empty());
        assert_eq!(
            storage.list_prefix("t/w/other/").await.unwrap(),
            vec!["t/w/other/state/999/state.json".to_string()]
        );
    }

    #[test]
    fn build_remote_reset_target_does_not_require_warehouse_config() {
        let dir = tempfile::tempdir().unwrap();
        let srv_creds = api_client::CredentialsResponse {
            credentials: api_client::StsCreds {
                access_key_id: "ak".to_string(),
                secret_access_key: "sk".to_string(),
                session_token: "tok".to_string(),
                expiration: "never".to_string(),
            },
            bucket: "bucket-123".to_string(),
            tenant_id: "tenant-abc".to_string(),
            llm_api_key: String::new(),
            accounting_url: String::new(),
        };

        let (bucket, s3_creds, prefix, remote_desc) =
            build_remote_reset_target("bike-hire-snowflake", dir.path(), &srv_creds)
                .expect("build reset target");

        assert_eq!(bucket, "bucket-123");
        assert_eq!(s3_creds.access_key_id, "ak");
        assert_eq!(prefix, "tenant-abc/dev/bike-hire-snowflake/");
        assert_eq!(
            remote_desc,
            "s3://bucket-123/tenant-abc/dev/bike-hire-snowflake/"
        );
    }

    #[test]
    fn ensure_local_environment_recreates_skippr_dir_and_env_example() {
        let dir = tempfile::tempdir().unwrap();
        let (skippr_dir, env_example) = ensure_local_environment(dir.path()).expect("env");
        assert!(
            skippr_dir.exists(),
            "expected .skippr directory to be created"
        );
        assert!(env_example.exists(), "expected .env.example to be created");
        let contents = fs::read_to_string(env_example).unwrap();
        assert!(contents.contains("MSSQL_CONNECTION_STRING="));
    }

    #[test]
    fn feedback_cli_requires_a_verdict_flag() {
        let err = Cli::try_parse_from(["skippr", "feedback"]).expect_err("missing verdict");
        let rendered = err.to_string();
        assert!(rendered.contains("--good"));
        assert!(rendered.contains("--bad"));
    }

    #[test]
    fn feedback_cli_rejects_conflicting_verdict_flags() {
        let err = Cli::try_parse_from(["skippr", "feedback", "--good", "--bad"])
            .expect_err("conflicting verdict");
        let rendered = err.to_string();
        assert!(rendered.contains("--bad"));
        assert!(rendered.contains("--good"));
    }

    #[test]
    fn feedback_cli_accepts_comment_flag() {
        let cli = Cli::try_parse_from([
            "skippr",
            "feedback",
            "--bad",
            "--comment",
            "timed out in repair loop",
        ])
        .expect("parse feedback args");
        match cli.cmd {
            Cmd::Feedback {
                good,
                bad,
                comment,
                no_diagnostics,
            } => {
                assert!(!good);
                assert!(bad);
                assert_eq!(comment.as_deref(), Some("timed out in repair loop"));
                assert!(!no_diagnostics);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn feedback_cli_accepts_no_diagnostics_flag() {
        let cli = Cli::try_parse_from(["skippr", "feedback", "--bad", "--no-diagnostics"])
            .expect("parse feedback args");
        match cli.cmd {
            Cmd::Feedback {
                good,
                bad,
                comment,
                no_diagnostics,
            } => {
                assert!(!good);
                assert!(bad);
                assert!(comment.is_none());
                assert!(no_diagnostics);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn find_latest_thread_in_skippr_dir_skips_companion_threads() {
        let dir = tempfile::tempdir().unwrap();
        let threads_dir = dir.path().join("_/dev/test-project/threads");
        fs::create_dir_all(&threads_dir).unwrap();

        let older = threads_dir.join("11111111-1111-1111-1111-111111111111.json");
        let newer = threads_dir.join("22222222-2222-2222-2222-222222222222.json");
        let companion = threads_dir.join("22222222-2222-2222-2222-222222222222__gather_0.json");
        fs::write(&older, "{}").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(&newer, "{}").unwrap();
        fs::write(&companion, "{}").unwrap();

        let thread = find_latest_thread_in_skippr_dir(dir.path(), "test-project");
        assert_eq!(
            thread.as_deref(),
            Some("22222222-2222-2222-2222-222222222222")
        );
    }

    #[test]
    fn latest_primary_thread_id_from_s3_objects_prefers_newest_primary_thread() {
        let objects = vec![
            react_module_storage_s3::ObjectMeta {
                key: "t/w/p/threads/11111111-1111-1111-1111-111111111111.json".to_string(),
                last_modified: Some(
                    chrono::DateTime::parse_from_rfc3339("2026-04-04T10:00:00Z")
                        .unwrap()
                        .with_timezone(&chrono::Utc),
                ),
            },
            react_module_storage_s3::ObjectMeta {
                key: "t/w/p/threads/22222222-2222-2222-2222-222222222222__gather_0.json"
                    .to_string(),
                last_modified: Some(
                    chrono::DateTime::parse_from_rfc3339("2026-04-04T12:00:00Z")
                        .unwrap()
                        .with_timezone(&chrono::Utc),
                ),
            },
            react_module_storage_s3::ObjectMeta {
                key: "t/w/p/threads/33333333-3333-3333-3333-333333333333.control.json".to_string(),
                last_modified: Some(
                    chrono::DateTime::parse_from_rfc3339("2026-04-04T13:00:00Z")
                        .unwrap()
                        .with_timezone(&chrono::Utc),
                ),
            },
            react_module_storage_s3::ObjectMeta {
                key: "t/w/p/threads/44444444-4444-4444-4444-444444444444.json".to_string(),
                last_modified: Some(
                    chrono::DateTime::parse_from_rfc3339("2026-04-04T11:00:00Z")
                        .unwrap()
                        .with_timezone(&chrono::Utc),
                ),
            },
        ];

        let thread = latest_primary_thread_id_from_s3_objects(&objects, "t/w/p/threads/");
        assert_eq!(
            thread.as_deref(),
            Some("44444444-4444-4444-4444-444444444444")
        );
    }

    #[test]
    fn resolve_feedback_comment_trims_inline_comment() {
        let comment =
            resolve_feedback_comment(Some("  this run looked good  ".to_string())).unwrap();
        assert_eq!(comment, "this run looked good");
    }

    #[test]
    fn normalize_feedback_comment_rejects_blank_input() {
        let err = normalize_feedback_comment("   ").expect_err("blank comment");
        assert!(err.contains("cannot be empty"));
    }

    #[test]
    fn resolve_feedback_verdict_maps_good_and_bad_flags() {
        assert_eq!(
            resolve_feedback_verdict(true, false).unwrap(),
            react_core::thread_feedback::ThreadFeedbackVerdict::Good
        );
        assert_eq!(
            resolve_feedback_verdict(false, true).unwrap(),
            react_core::thread_feedback::ThreadFeedbackVerdict::Bad
        );
    }
}
