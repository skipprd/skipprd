mod api_client;
mod auth;
mod feedback_diagnostics;
mod public_config;
mod react_host;
mod translate;

use std::{collections::HashMap, path::PathBuf, process::Command, sync::Arc};

use clap::{Parser, Subcommand};
use react::config::{LlmFile, ReactConfigFile, ScopeFile, StorageFile};
use react_core::keyspace::Keyspace;

use public_config::{DbtConfig, S3Transform, SkipprDbtConfig, SourceConfig, WarehouseConfig};

const SKIPPR_EULA_VERSION: &str = "skippr-eula-2026-04-29";
const SKIPPR_EULA_URL: &str = "https://skippr.io/terms/eula";
const S3_CREDENTIAL_REFRESH_MARGIN_SECONDS: i64 = 300;

#[derive(Clone)]
struct AuthS3CredentialsProvider {
    client: api_client::ApiClient,
    cached: Arc<tokio::sync::Mutex<Option<react_core::resolved_config::S3Credentials>>>,
}

impl AuthS3CredentialsProvider {
    fn new(
        client: api_client::ApiClient,
        initial: Option<react_core::resolved_config::S3Credentials>,
    ) -> Self {
        Self {
            client,
            cached: Arc::new(tokio::sync::Mutex::new(initial)),
        }
    }

    fn is_fresh(credentials: &react_core::resolved_config::S3Credentials) -> bool {
        let Some(expires_at) = credentials.expires_at.as_ref() else {
            return false;
        };
        chrono::Utc::now() + chrono::Duration::seconds(S3_CREDENTIAL_REFRESH_MARGIN_SECONDS)
            < *expires_at
    }
}

#[async_trait::async_trait]
impl react_core::resolved_config::S3CredentialsProvider for AuthS3CredentialsProvider {
    async fn s3_credentials(&self) -> Result<react_core::resolved_config::S3Credentials, String> {
        let mut cached = self.cached.lock().await;
        if let Some(credentials) = cached
            .as_ref()
            .filter(|credentials| Self::is_fresh(credentials))
        {
            return Ok(credentials.clone());
        }

        let response = self
            .client
            .get_credentials()
            .await
            .map_err(|e| format!("failed to refresh hosted S3 credentials: {e}"))?;
        let credentials = translate::s3_credentials_from_auth(&response);
        *cached = Some(credentials.clone());
        Ok(credentials)
    }
}

fn attach_s3_credentials_provider(
    resolved: &mut react_core::resolved_config::ReactResolvedConfig,
    client: api_client::ApiClient,
) {
    if let Some(credentials) = resolved.storage.s3_credentials.as_mut() {
        let provider = Arc::new(AuthS3CredentialsProvider::new(
            client,
            Some(credentials.clone()),
        ));
        credentials.provider = Some(provider);
    }
}

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

    /// Discover schemas and persist pipeline metadata.
    Discover(EngineDiscoverArgs),

    /// Extract and load data into the configured destination.
    Sync(EngineSyncArgs),

    /// Run the data-engineer modeling workflow.
    Model(ModelArgs),

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

#[derive(Parser, Debug, Clone)]
struct EngineDiscoverArgs {
    /// The pipeline to use.
    #[arg(short, long)]
    pipeline: Option<String>,
    /// Output mode: progress, json, or text.
    #[arg(long, default_value = "progress")]
    output: String,
}

#[derive(Parser, Debug, Clone)]
struct EngineSyncArgs {
    /// The pipeline to use.
    #[arg(short, long)]
    pipeline: Option<String>,
    /// Output mode: progress, json, or text.
    #[arg(long, default_value = "progress")]
    output: String,
    /// Run a single sync pass and exit.
    #[arg(long, default_value_t = false)]
    once: bool,
}

#[derive(Parser, Debug, Clone)]
struct ModelArgs {
    /// Start a fresh modeling thread instead of resuming the latest project thread.
    #[arg(long, default_value_t = false)]
    no_resume: bool,
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

fn load_engine_config(explicit: &Option<PathBuf>) -> Result<serde_yaml::Value, String> {
    let path = config_path(explicit);
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("failed to read {}: {}", path.display(), e))?;
    serde_yaml::from_str(&raw).map_err(|e| format!("failed to parse {}: {}", path.display(), e))
}

fn save_engine_config(explicit: &Option<PathBuf>, value: &serde_yaml::Value) -> Result<(), String> {
    let path = config_path(explicit);
    let raw = serde_yaml::to_string(value)
        .map_err(|e| format!("failed to serialize {}: {}", path.display(), e))?;
    std::fs::write(&path, raw).map_err(|e| format!("failed to write {}: {}", path.display(), e))
}

fn yaml_key(key: &str) -> serde_yaml::Value {
    serde_yaml::Value::String(key.to_string())
}

fn engine_project_name(value: &serde_yaml::Value) -> Result<String, String> {
    value
        .get("skippr")
        .and_then(|skippr| skippr.get("workspace"))
        .and_then(|workspace| workspace.as_str())
        .or_else(|| value.get("project").and_then(|project| project.as_str()))
        .map(str::trim)
        .filter(|project| !project.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| "skippr.yaml must set skippr.workspace".to_string())
}

fn default_react_providers_config() -> serde_json::Value {
    serde_json::json!({
        "catalog": {
            "enabled": true,
            "refresh_secs": 3600,
            "max_concurrency": 8,
        },
        "dbt": {
            "enabled": true,
            "runner": "host",
            "target": "dev",
        },
        "vector": {
            "enabled": true,
        }
    })
}

fn ref_name(reference: &str, prefix: &str) -> Option<String> {
    reference
        .trim()
        .strip_prefix(prefix)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn first_mapping_key(map: &serde_yaml::Mapping) -> Option<String> {
    map.keys()
        .filter_map(|key| key.as_str())
        .map(str::trim)
        .find(|key| !key.is_empty())
        .map(str::to_string)
}

fn selected_pipeline<'a>(
    engine_cfg: &'a serde_yaml::Value,
    project: &str,
) -> Option<&'a serde_yaml::Value> {
    let pipelines = engine_cfg.get("pipelines")?.as_mapping()?;
    if let Some(p) = pipelines.get(yaml_key(project)) {
        return Some(p);
    }
    let first = first_mapping_key(pipelines)?;
    pipelines.get(yaml_key(&first))
}

fn selected_data_sink_config(
    engine_cfg: &serde_yaml::Value,
    project: &str,
) -> Option<serde_json::Value> {
    let pipeline = selected_pipeline(engine_cfg, project)?;
    let sink_ref = pipeline.get("data_sink")?.as_str()?;
    let sink_name = ref_name(sink_ref, "data_sinks.")?;
    let sink = engine_cfg.get("data_sinks")?.get(&sink_name)?;
    let sink_map = sink.as_mapping()?;
    let plugin = first_mapping_key(sink_map)?;
    let plugin_cfg = sink.get(&plugin)?;
    let mut json_cfg = serde_json::to_value(plugin_cfg).ok()?;
    if let Some(obj) = json_cfg.as_object_mut() {
        obj.entry("kind".to_string())
            .or_insert_with(|| serde_json::Value::String(plugin.to_ascii_lowercase()));
    }
    Some(json_cfg)
}

fn merge_selected_sink_into_react_warehouse(
    mut providers: serde_json::Value,
    engine_cfg: &serde_yaml::Value,
    project: &str,
) -> serde_json::Value {
    let Some(sink_cfg) = selected_data_sink_config(engine_cfg, project) else {
        return providers;
    };
    let Some(sink_obj) = sink_cfg.as_object() else {
        return providers;
    };
    let Some(providers_obj) = providers.as_object_mut() else {
        return providers;
    };
    let warehouse = providers_obj
        .entry("warehouse".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let Some(warehouse_obj) = warehouse.as_object_mut() else {
        return providers;
    };
    let existing_kind = warehouse_obj
        .get("kind")
        .and_then(|v| v.as_str())
        .map(str::to_ascii_lowercase);
    let sink_kind = sink_obj
        .get("kind")
        .and_then(|v| v.as_str())
        .map(str::to_ascii_lowercase);
    if existing_kind.is_some() && sink_kind.is_some() && existing_kind != sink_kind {
        return providers;
    }
    for (key, value) in sink_obj {
        if value.is_null() {
            continue;
        }
        warehouse_obj
            .entry(key.clone())
            .or_insert_with(|| value.clone());
    }
    providers
}

fn react_config_from_engine_config(value: &serde_yaml::Value) -> Result<ReactConfigFile, String> {
    let project = engine_project_name(value)?;
    let providers_yaml = value
        .get("react")
        .and_then(|react| react.get("providers"))
        .or_else(|| value.get("providers"));
    let providers = match providers_yaml {
        Some(providers) => serde_json::to_value(providers)
            .map_err(|e| format!("failed to convert react.providers to JSON: {}", e))?,
        None => default_react_providers_config(),
    };
    let providers = merge_selected_sink_into_react_warehouse(providers, value, &project);

    Ok(ReactConfigFile {
        version: Some(1),
        server: None,
        storage: Some(StorageFile {
            mode: Some("local".into()),
            bucket: None,
            path: Some("./.skippr".into()),
            s3_credentials: None,
        }),
        scope: Some(ScopeFile {
            tenant: Some("_".into()),
            workspace: Some("dev".into()),
            project_id: Some(project),
        }),
        llm: Some(LlmFile {
            provider: Some("OPENAI_COMPAT".into()),
            base_url: Some("https://api.openai.com".into()),
            reason_model: Some("gpt-5.4".into()),
            task_model: Some("gpt-5.4".into()),
            embed_model: Some("text-embedding-3-small".into()),
            context_length: Some(8192),
            http_timeout_secs: Some(120),
            max_tokens: Some(8192),
            temperature: Some(0.2),
            top_p: Some(1.0),
            ..Default::default()
        }),
        providers: Some(providers),
    })
}

fn section_mapping_mut<'a>(
    value: &'a mut serde_yaml::Value,
    section: &str,
) -> &'a mut serde_yaml::Mapping {
    if !value.is_mapping() {
        *value = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
    }
    let root = value.as_mapping_mut().expect("root is mapping");
    root.entry(yaml_key(section))
        .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
    root.get_mut(&yaml_key(section))
        .expect("section exists")
        .as_mapping_mut()
        .expect("section is mapping")
}

fn yaml_plugin_entry(plugin: &str, config: serde_json::Value) -> serde_yaml::Value {
    serde_yaml::to_value(serde_json::json!({ plugin: config })).expect("plugin entry is yaml")
}

fn set_plugin_section(
    value: &mut serde_yaml::Value,
    section: &str,
    name: &str,
    plugin: &str,
    config: serde_json::Value,
) {
    section_mapping_mut(value, section).insert(yaml_key(name), yaml_plugin_entry(plugin, config));
}

fn set_primary_pipeline_refs(
    value: &mut serde_yaml::Value,
    source: Option<&str>,
    sink: Option<&str>,
) {
    let project = engine_project_name(value).unwrap_or_else(|_| "default".to_string());
    let pipelines = section_mapping_mut(value, "pipelines");
    pipelines
        .entry(yaml_key(&project))
        .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
    let pipeline = pipelines
        .get_mut(&yaml_key(&project))
        .expect("pipeline exists")
        .as_mapping_mut()
        .expect("pipeline is mapping");
    if let Some(source) = source {
        pipeline.insert(
            yaml_key("data_source"),
            yaml_key(&format!("data_sources.{source}")),
        );
    }
    if let Some(sink) = sink {
        pipeline.insert(
            yaml_key("data_sink"),
            yaml_key(&format!("data_sinks.{sink}")),
        );
    }
}

fn json_object(fields: Vec<(&str, Option<serde_json::Value>)>) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (key, value) in fields {
        if let Some(value) = value {
            if !value.is_null() {
                map.insert(key.to_string(), value);
            }
        }
    }
    serde_json::Value::Object(map)
}

fn str_json(value: Option<String>) -> Option<serde_json::Value> {
    value.map(serde_json::Value::String)
}

fn u16_json(value: Option<u16>) -> Option<serde_json::Value> {
    value.map(|value| serde_json::Value::Number(value.into()))
}

fn u8_json(value: Option<u8>) -> Option<serde_json::Value> {
    value.map(|value| serde_json::Value::Number(value.into()))
}

fn u32_json(value: Option<u32>) -> Option<serde_json::Value> {
    value.map(|value| serde_json::Value::Number(value.into()))
}

fn u64_json(value: Option<u64>) -> Option<serde_json::Value> {
    value.map(|value| serde_json::Value::Number(value.into()))
}

fn i64_json(value: Option<i64>) -> Option<serde_json::Value> {
    value.map(|value| serde_json::Value::Number(value.into()))
}

fn strings_json(value: Option<Vec<String>>) -> Option<serde_json::Value> {
    value.map(|values| serde_json::json!(values))
}

fn map_json(value: Option<Vec<(String, String)>>) -> Option<serde_json::Value> {
    pairs_to_hash_map(value).map(|map| serde_json::json!(map))
}

fn warehouse_plugin_and_config(kind: WarehouseKind) -> (&'static str, serde_json::Value) {
    match kind {
        WarehouseKind::Athena {
            workgroup,
            region,
            result_s3,
            schema,
        } => (
            "Athena",
            json_object(vec![
                ("workgroup", str_json(workgroup)),
                ("region", str_json(region)),
                ("result_s3", str_json(result_s3)),
                ("schema", str_json(schema)),
            ]),
        ),
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
        } => (
            "Snowflake",
            json_object(vec![
                ("account", str_json(account)),
                ("user", str_json(user)),
                ("password", str_json(password)),
                ("private_key_path", str_json(private_key_path)),
                ("stage", str_json(stage)),
                ("staging_uri", str_json(staging_uri)),
                (
                    "staging_storage_integration",
                    str_json(staging_storage_integration),
                ),
                ("staging_azure_sas_token", str_json(staging_azure_sas_token)),
                (
                    "staging_azure_account_key",
                    str_json(staging_azure_account_key),
                ),
                (
                    "staging_gcs_service_account_key_path",
                    str_json(staging_gcs_service_account_key_path),
                ),
                ("database", str_json(database)),
                ("schema", str_json(schema)),
                ("warehouse", str_json(warehouse)),
                ("role", str_json(role)),
            ]),
        ),
        WarehouseKind::Bigquery {
            project,
            dataset,
            location,
        } => (
            "Bigquery",
            json_object(vec![
                ("project", str_json(project)),
                ("dataset", str_json(dataset)),
                ("location", str_json(location)),
            ]),
        ),
        WarehouseKind::Postgres { database, schema } => (
            "Postgres",
            json_object(vec![
                ("database", str_json(database)),
                ("schema", str_json(schema)),
            ]),
        ),
        WarehouseKind::Databricks {
            workspace_url,
            token,
            warehouse_id,
            catalog,
            schema,
        } => (
            "Databricks",
            json_object(vec![
                ("workspace_url", str_json(workspace_url)),
                ("token", str_json(token)),
                ("warehouse_id", str_json(warehouse_id)),
                ("catalog", str_json(catalog)),
                ("schema", str_json(schema)),
            ]),
        ),
        WarehouseKind::Synapse {
            connection_string,
            schema,
        } => (
            "Synapse",
            json_object(vec![
                ("connection_string", str_json(connection_string)),
                ("schema", str_json(schema)),
            ]),
        ),
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
        } => (
            "Redshift",
            json_object(vec![
                ("database", str_json(database)),
                ("cluster_identifier", str_json(cluster_identifier)),
                ("workgroup_name", str_json(workgroup_name)),
                ("db_user", str_json(db_user)),
                ("schema", str_json(schema)),
                ("region", str_json(region)),
                ("staging_s3_bucket", str_json(staging_s3_bucket)),
                ("staging_s3_prefix", str_json(staging_s3_prefix)),
                ("iam_role_arn", str_json(iam_role_arn)),
            ]),
        ),
        WarehouseKind::Clickhouse {
            url,
            database,
            user,
            password,
        } => (
            "Clickhouse",
            json_object(vec![
                ("url", str_json(url)),
                ("database", str_json(database)),
                ("user", str_json(user)),
                ("password", str_json(password)),
            ]),
        ),
        WarehouseKind::Motherduck {
            motherduck_token,
            database,
            schema,
        } => (
            "Motherduck",
            json_object(vec![
                ("motherduck_token", str_json(motherduck_token)),
                ("database", str_json(database)),
                ("schema", str_json(schema)),
            ]),
        ),
    }
}

fn source_plugin_and_config(kind: SourceKind) -> (&'static str, serde_json::Value) {
    match kind {
        SourceKind::Mssql { connection_string } => (
            "Mssql",
            json_object(vec![("connection_string", str_json(connection_string))]),
        ),
        SourceKind::S3 {
            bucket,
            prefix,
            namespace_fields,
        } => (
            "S3",
            json_object(vec![
                ("bucket", str_json(bucket)),
                ("prefix", str_json(prefix)),
                ("namespace_fields", str_json(namespace_fields)),
            ]),
        ),
        SourceKind::Mysql {
            connection_string,
            tables,
        } => (
            "Mysql",
            json_object(vec![
                ("connection_string", str_json(connection_string)),
                ("tables", strings_json(tables)),
            ]),
        ),
        SourceKind::PostgresSource {
            host,
            port,
            user,
            password,
            database,
            connection_string,
            tables,
            query,
        } => (
            "Postgres",
            json_object(vec![
                ("host", str_json(host)),
                ("port", u16_json(port)),
                ("user", str_json(user)),
                ("password", str_json(password)),
                ("database", str_json(database)),
                ("connection_string", str_json(connection_string)),
                ("tables", strings_json(tables)),
                ("query", str_json(query)),
            ]),
        ),
        SourceKind::RedshiftSource {
            cluster_identifier,
            workgroup_name,
            database,
            db_user,
            tables,
            region,
        } => (
            "Redshift",
            json_object(vec![
                ("cluster_identifier", str_json(cluster_identifier)),
                ("workgroup_name", str_json(workgroup_name)),
                ("database", str_json(database)),
                ("db_user", str_json(db_user)),
                ("tables", strings_json(tables)),
                ("region", str_json(region)),
            ]),
        ),
        SourceKind::Mongodb {
            connection_string,
            database,
            collection,
            filter,
        } => (
            "Mongodb",
            json_object(vec![
                ("connection_string", str_json(connection_string)),
                ("database", str_json(database)),
                ("collection", str_json(collection)),
                ("filter", str_json(filter)),
            ]),
        ),
        SourceKind::Dynamodb {
            table_name,
            region,
            endpoint_url,
        } => (
            "Dynamodb",
            json_object(vec![
                ("table_name", str_json(table_name)),
                ("region", str_json(region)),
                ("endpoint_url", str_json(endpoint_url)),
            ]),
        ),
        SourceKind::ClickhouseSource {
            url,
            database,
            user,
            password,
            tables,
            query,
        } => (
            "Clickhouse",
            json_object(vec![
                ("url", str_json(url)),
                ("database", str_json(database)),
                ("user", str_json(user)),
                ("password", str_json(password)),
                ("tables", strings_json(tables)),
                ("query", str_json(query)),
            ]),
        ),
        SourceKind::MotherduckSource {
            motherduck_token,
            database,
            tables,
            query,
        } => (
            "Motherduck",
            json_object(vec![
                ("motherduck_token", str_json(motherduck_token)),
                ("database", str_json(database)),
                ("tables", strings_json(tables)),
                ("query", str_json(query)),
            ]),
        ),
        SourceKind::Sftp {
            host,
            port,
            username,
            password,
            private_key_path,
            remote_path,
        } => (
            "Sftp",
            json_object(vec![
                ("host", str_json(host)),
                ("port", u16_json(port)),
                ("username", str_json(username)),
                ("password", str_json(password)),
                ("private_key_path", str_json(private_key_path)),
                ("remote_path", str_json(remote_path)),
            ]),
        ),
        SourceKind::File { path } => ("File", json_object(vec![("path", str_json(path))])),
        SourceKind::DeltaLake {
            table_uri,
            storage_options,
            version,
            filter,
        } => (
            "DeltaLake",
            json_object(vec![
                ("table_uri", str_json(table_uri)),
                ("storage_options", map_json(storage_options)),
                ("version", i64_json(version)),
                ("filter", str_json(filter)),
            ]),
        ),
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
        } => (
            "Kafka",
            json_object(vec![
                ("brokers", str_json(brokers)),
                ("topic", str_json(topic)),
                ("group_id", str_json(group_id)),
                ("auto_offset_reset", str_json(auto_offset_reset)),
                ("security_protocol", str_json(security_protocol)),
                ("sasl_mechanism", str_json(sasl_mechanism)),
                ("sasl_username", str_json(sasl_username)),
                ("sasl_password", str_json(sasl_password)),
                ("mode", str_json(mode)),
            ]),
        ),
        SourceKind::Sqs {
            queue_url,
            region,
            endpoint_url,
            mode,
        } => (
            "Sqs",
            json_object(vec![
                ("queue_url", str_json(queue_url)),
                ("region", str_json(region)),
                ("endpoint_url", str_json(endpoint_url)),
                ("mode", str_json(mode)),
            ]),
        ),
        SourceKind::Kinesis {
            stream_name,
            region,
            endpoint_url,
            mode,
        } => (
            "Kinesis",
            json_object(vec![
                ("stream_name", str_json(stream_name)),
                ("region", str_json(region)),
                ("endpoint_url", str_json(endpoint_url)),
                ("mode", str_json(mode)),
            ]),
        ),
        SourceKind::Amqp {
            connection_string,
            queue,
            exchange,
            routing_key,
            prefetch_count,
            mode,
        } => (
            "Amqp",
            json_object(vec![
                ("connection_string", str_json(connection_string)),
                ("queue", str_json(queue)),
                ("exchange", str_json(exchange)),
                ("routing_key", str_json(routing_key)),
                ("prefetch_count", u32_json(prefetch_count)),
                ("mode", str_json(mode)),
            ]),
        ),
        SourceKind::Sns {
            topic_arn,
            sqs_queue_url,
            region,
            endpoint_url,
        } => (
            "Sns",
            json_object(vec![
                ("topic_arn", str_json(topic_arn)),
                ("sqs_queue_url", str_json(sqs_queue_url)),
                ("region", str_json(region)),
                ("endpoint_url", str_json(endpoint_url)),
            ]),
        ),
        SourceKind::Eventbridge {
            event_bus_name,
            sqs_queue_url,
            region,
            endpoint_url,
        } => (
            "Eventbridge",
            json_object(vec![
                ("event_bus_name", str_json(event_bus_name)),
                ("sqs_queue_url", str_json(sqs_queue_url)),
                ("region", str_json(region)),
                ("endpoint_url", str_json(endpoint_url)),
            ]),
        ),
        SourceKind::Mqtt {
            broker_url,
            port,
            topic,
            client_id,
            qos,
            username,
            password,
            mode,
        } => (
            "Mqtt",
            json_object(vec![
                ("broker_url", str_json(broker_url)),
                ("port", u16_json(port)),
                ("topic", str_json(topic)),
                ("client_id", str_json(client_id)),
                ("qos", u8_json(qos)),
                ("username", str_json(username)),
                ("password", str_json(password)),
                ("mode", str_json(mode)),
            ]),
        ),
        SourceKind::Websocket { url, headers, mode } => (
            "Websocket",
            json_object(vec![
                ("url", str_json(url)),
                ("headers", map_json(headers)),
                ("mode", str_json(mode)),
            ]),
        ),
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
        } => (
            "HttpClient",
            json_object(vec![
                ("url", str_json(url)),
                ("method", str_json(method)),
                ("headers", map_json(headers)),
                ("body", str_json(body)),
                ("auth_strategy", str_json(auth_strategy)),
                ("auth_user", str_json(auth_user)),
                ("auth_password", str_json(auth_password)),
                ("auth_token", str_json(auth_token)),
                ("scrape_interval_seconds", u64_json(scrape_interval_seconds)),
            ]),
        ),
        SourceKind::HttpServer {
            listen_address,
            path,
            auth_token,
        } => (
            "HttpServer",
            json_object(vec![
                ("listen_address", str_json(listen_address)),
                ("path", str_json(path)),
                ("auth_token", str_json(auth_token)),
            ]),
        ),
        SourceKind::Socket {
            mode,
            address,
            framing,
        } => (
            "Socket",
            json_object(vec![
                ("mode", str_json(mode)),
                ("address", str_json(address)),
                ("framing", str_json(framing)),
            ]),
        ),
        SourceKind::Statsd { listen_address } => (
            "Statsd",
            json_object(vec![("listen_address", str_json(listen_address))]),
        ),
        SourceKind::Stdin { mode } => ("Stdin", json_object(vec![("mode", str_json(mode))])),
    }
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
    let authenticated_with_api_key = std::env::var("SKIPPR_API_KEY")
        .ok()
        .is_some_and(|value| !value.trim().is_empty());
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
    ensure_eula_accepted(&client, !authenticated_with_api_key)
        .await
        .map_err(|e| format!("EULA acceptance required: {e}"))?;
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

    let s3_creds = translate::s3_credentials_from_auth(srv_creds);
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

    let raw = format!(
        r#"skippr:
  workspace: {name}
  tenant: _
  skipprd_el_storage_mode: local

pipelines:
  {name}:
    data_source: data_sources.source
    data_sink: data_sinks.warehouse

data_sources: {{}}
data_sinks: {{}}
schema_sinks: {{}}

react:
  providers:
    catalog:
      enabled: true
      refresh_secs: 3600
      max_concurrency: 8
    dbt:
      enabled: true
      runner: host
      target: dev
    vector:
      enabled: true
"#
    );
    let cfg: serde_yaml::Value = serde_yaml::from_str(&raw).expect("valid default skippr.yaml");
    if let Err(e) = save_engine_config(explicit_config, &cfg) {
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
    println!("  skippr discover --pipeline {name}");
    println!("  skippr sync --pipeline {name} --once");
    println!("  skippr model");
}

// ---------------------------------------------------------------------------
// connect warehouse
// ---------------------------------------------------------------------------

fn cmd_connect_warehouse(kind: WarehouseKind, explicit_config: &Option<PathBuf>) {
    if let Ok(mut cfg) = load_engine_config(explicit_config) {
        let (plugin, config) = warehouse_plugin_and_config(kind);
        set_plugin_section(&mut cfg, "data_sinks", "warehouse", plugin, config);
        set_primary_pipeline_refs(&mut cfg, None, Some("warehouse"));
        if let Err(e) = save_engine_config(explicit_config, &cfg) {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
        println!("Configured warehouse data sink 'warehouse' ({plugin}) in skippr.yaml");
        return;
    }

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
    if let Ok(mut cfg) = load_engine_config(explicit_config) {
        let (plugin, config) = source_plugin_and_config(kind);
        set_plugin_section(&mut cfg, "data_sources", "source", plugin, config);
        set_primary_pipeline_refs(&mut cfg, Some("source"), None);
        if let Err(e) = save_engine_config(explicit_config, &cfg) {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
        println!("Configured data source 'source' ({plugin}) in skippr.yaml");
        return;
    }

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

    let cfg = match load_engine_config(explicit_config) {
        Ok(c) => {
            check_pass("skippr.yaml found");
            c
        }
        Err(_) => {
            check_fail("skippr.yaml not found — run 'skippr init <project>'");
            std::process::exit(1);
        }
    };

    if cfg
        .get("pipelines")
        .and_then(|pipelines| pipelines.as_mapping())
        .map(|pipelines| !pipelines.is_empty())
        .unwrap_or(false)
    {
        check_pass("pipelines configured");
    } else {
        check_fail("no pipelines configured in skippr.yaml");
        ok = false;
    }

    if cfg
        .get("data_sources")
        .and_then(|sources| sources.as_mapping())
        .map(|sources| !sources.is_empty())
        .unwrap_or(false)
    {
        check_pass("data sources configured");
    } else {
        check_fail("data source not configured — run 'skippr connect source <kind>'");
        ok = false;
    }

    if cfg
        .get("data_sinks")
        .and_then(|sinks| sinks.as_mapping())
        .map(|sinks| !sinks.is_empty())
        .unwrap_or(false)
    {
        check_pass("data sinks configured");
    } else {
        check_fail("warehouse/data sink not configured — run 'skippr connect warehouse <kind>'");
        ok = false;
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
        check_pass("Custom LLM key override is set");
    } else {
        check_pass("LLM credentials managed by Skippr");
    }

    let cfg_raw = serde_yaml::to_string(&cfg)
        .unwrap_or_default()
        .to_lowercase();
    if cfg_raw.contains("athena") {
        check_athena_env(&mut ok);
    }

    if cfg_raw.contains("snowflake") {
        check_snowflake_env(&mut ok);
    }

    if cfg_raw.contains("bigquery") {
        check_bigquery_env(&mut ok);
    }

    if cfg_raw.contains("postgres") {
        check_postgres_env(&mut ok);
    }

    println!();
    if ok {
        println!("All checks passed. Run 'skippr discover', 'skippr sync', or 'skippr model'.");
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
// engine commands
// ---------------------------------------------------------------------------

async fn prepare_engine_command(
    log: Option<String>,
    explicit_config: &Option<PathBuf>,
    mode: skipprd::cli::Mode,
    pipeline: Option<&str>,
) {
    let path = config_path(explicit_config);
    if !path.exists() {
        eprintln!("error: {} not found", path.display());
        eprintln!("Run 'skippr init <project>' first.");
        std::process::exit(1);
    }

    std::env::set_var("SKIPPR_CONFIG_FILE", &path);
    skipprd::helpers::logging::init_logging(log);
    skipprd::helpers::configuration::Config::build_config();
    skipprd::cli::CLI_MODE.write().clone_from(&mode);
    if let Some(pipeline) = pipeline {
        skipprd::helpers::configuration::PIPELINE_NAME
            .write()
            .clear();
        skipprd::helpers::configuration::PIPELINE_NAME
            .write()
            .push_str(pipeline);
    }
    skipprd::helpers::configuration::Config::init().await;
}

async fn cmd_discover(
    log: Option<String>,
    explicit_config: &Option<PathBuf>,
    args: EngineDiscoverArgs,
) {
    let mode = skipprd::cli::Mode::Discover(skipprd::cli::DisocverOptions {
        pipeline: args.pipeline.clone(),
        output: args.output.clone(),
    });
    prepare_engine_command(log, explicit_config, mode, args.pipeline.as_deref()).await;
    if let Err(err) = skipprd::engine::run_discover(&args.output).await {
        eprintln!("[skippr] discover failed: {}", err);
        std::process::exit(1);
    };
}

async fn cmd_sync(log: Option<String>, explicit_config: &Option<PathBuf>, args: EngineSyncArgs) {
    let mode = skipprd::cli::Mode::Sync(skipprd::cli::SyncOptions {
        pipeline: args.pipeline.clone(),
        output: args.output.clone(),
        once: args.once,
    });
    prepare_engine_command(log, explicit_config, mode, args.pipeline.as_deref()).await;
    skipprd::metrics::Metrics::init_send_loop();
    if let Err(err) = skipprd::engine::run_sync(&args.output).await {
        eprintln!("[skippr] sync failed: {}", err);
        std::process::exit(1);
    }
}

async fn cmd_model(log: Option<String>, explicit_config: &Option<PathBuf>, args: ModelArgs) {
    let engine_cfg = match load_engine_config(explicit_config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {}", e);
            eprintln!("Run 'skippr init <project>' first.");
            std::process::exit(1);
        }
    };
    let project = match engine_project_name(&engine_cfg) {
        Ok(project) => project,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };
    let mut internal_file = match react_config_from_engine_config(&engine_cfg) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };

    // Authentication is mandatory. SKIPPR_API_KEY env var takes priority, then credentials.json.
    let authenticated_with_api_key = std::env::var("SKIPPR_API_KEY")
        .ok()
        .is_some_and(|value| !value.trim().is_empty());
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

    if let Err(e) = ensure_eula_accepted(&client, !authenticated_with_api_key).await {
        eprintln!("[skippr] ERROR: {}", e);
        std::process::exit(1);
    }

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
            )
            .unwrap_or_else(|e| {
                eprintln!("[skippr] ERROR: {e}");
                eprintln!("[skippr]   Check your login status and try again.");
                std::process::exit(1);
            });
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
                project_id: project.clone(),
            },
        ])
        .await;

    let mut resolved =
        match react_host::resolve_config(internal_file, react::config::ServeOverrides::default()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: {}", e);
                std::process::exit(1);
            }
        };
    attach_s3_credentials_provider(&mut resolved, client.clone());

    eprintln!(
        "[skippr] model config: project={} tenant={} storage={:?}",
        resolved.scope.project_id, resolved.scope.tenant, resolved.storage.mode
    );

    let thread_id = if args.no_resume {
        eprintln!("[skippr] not resuming previous thread (--no-resume)");
        None
    } else {
        match find_latest_thread_for_resolved_config(&resolved).await {
            Ok(thread_id) => thread_id,
            Err(e) => {
                eprintln!("[skippr] WARNING: failed to discover latest thread: {e}");
                None
            }
        }
    };
    if let Some(ref tid) = thread_id {
        react_suite_data_engineer::metering::set_metering_thread_id(tid);
        eprintln!("[skippr] resuming thread {tid}");
    } else {
        eprintln!("[skippr] starting new modeling thread");
    }

    let status_cfg = resolved.clone();
    let run_thread_id = thread_id.clone();
    eprintln!("[skippr] starting headless data-engineer workflow");
    let headless = react_host::run_headless_detailed(
        resolved,
        react::run_engine::HeadlessRunOpts {
            log_level: log,
            verbose_debug: false,
            terminal: false,
            thread_id,
            suite_id: Some("data_engineer".to_string()),
            agent: "agent".to_string(),
            skip_logging_init: false,
        },
    )
    .await;
    let exit_code = headless.exit_code;
    eprintln!("[skippr] headless data-engineer workflow exited with code {exit_code}");
    if let Some(err) = headless.bootstrap_error.as_deref() {
        eprintln!("[skippr] data-engineer bootstrap failed before a thread was created: {err}");
    }
    let status_thread_id = run_thread_id;
    if let Some(tid) = status_thread_id.as_deref() {
        match load_model_thread_status(&status_cfg, tid).await {
            Ok(Some(status)) => {
                eprintln!(
                    "[skippr] data-engineer thread status: current_phase={} repair_status={} failed={} pending_plan_revision={}",
                    status.current_phase,
                    status.repair_status,
                    status.has_failure_context,
                    status.pending_plan_revision
                );
                if let Some(reason) = status.failure_brief.as_deref() {
                    eprintln!("[skippr] resumed thread failure detail: {reason}");
                }
                if exit_code != 0 && status.is_done {
                    eprintln!(
                        "[skippr] resumed thread is already complete; treating model as idempotent success."
                    );
                    std::process::exit(0);
                }
                if exit_code != 0 && !status.has_failure_context {
                    eprintln!(
                        "[skippr] resumed thread did not complete and has no data-engineer failure context; rerun with --no-resume to start a fresh modeling thread."
                    );
                }
            }
            Ok(None) => {
                eprintln!("[skippr] no persisted data-engineer status found for thread {tid}");
            }
            Err(e) => {
                eprintln!("[skippr] WARNING: failed to load data-engineer thread status: {e}");
            }
        }
    } else if exit_code != 0 && headless.bootstrap_error.is_none() {
        eprintln!(
            "[skippr] model run failed before the CLI received a thread id; no stale latest-thread status was used."
        );
    }
    std::process::exit(exit_code);
}

async fn load_model_thread_status(
    cfg: &react_core::resolved_config::ReactResolvedConfig,
    thread_id: &str,
) -> Result<Option<react_suite_data_engineer::DataEngineerThreadStatus>, String> {
    let storage: Arc<dyn react_core::storage::StorageAdapter> = match cfg.storage.mode {
        react_core::resolved_config::StorageMode::Local => {
            let root = cfg
                .storage
                .path
                .as_ref()
                .ok_or_else(|| "missing storage.path for local mode".to_string())?;
            Arc::new(
                react_module_storage_local::LocalFileStorageAdapter::new(root)
                    .map_err(|e| e.to_string())?,
            )
        }
        react_core::resolved_config::StorageMode::S3 => {
            let bucket = cfg
                .storage
                .bucket
                .clone()
                .ok_or_else(|| "missing storage.bucket for s3 mode".to_string())?;
            if let Some(creds) = cfg.storage.s3_credentials.as_ref() {
                Arc::new(
                    react_module_storage_s3::S3StorageAdapter::from_resolved_credentials(
                        bucket, creds,
                    )
                    .await,
                )
            } else {
                Arc::new(react_module_storage_s3::S3StorageAdapter::from_env(bucket).await)
            }
        }
    };
    let keyspace: Arc<dyn react_core::keyspace::Keyspace> = match cfg.storage.mode {
        react_core::resolved_config::StorageMode::Local => {
            let root = cfg
                .storage
                .path
                .as_ref()
                .ok_or_else(|| "missing storage.path for local mode".to_string())?;
            Arc::new(react_core::keyspace::LocalKeyspace::new(root.clone()))
        }
        react_core::resolved_config::StorageMode::S3 => {
            let bucket = cfg
                .storage
                .bucket
                .clone()
                .ok_or_else(|| "missing storage.bucket for s3 mode".to_string())?;
            Arc::new(react_core::keyspace::DefaultKeyspace::new(bucket))
        }
    };
    let control = react_core::session::ControlStateStore::new(storage, cfg.scope.clone(), keyspace);
    react_suite_data_engineer::load_thread_status(&control, thread_id).await
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
    let thread_id = match resolve_feedback_thread_id(&resolved_cfg).await {
        Ok(thread_id) => Some(thread_id),
        Err(e) if include_diagnostics => {
            eprintln!("[skippr] WARNING: {e}. Sending diagnostics without thread-linked feedback.");
            None
        }
        Err(e) => {
            eprintln!("[skippr] ERROR: {e}");
            std::process::exit(1);
        }
    };
    let Some(thread_id) = thread_id else {
        let diagnostics_id = uuid::Uuid::new_v4().to_string();
        let diagnostics = feedback_diagnostics::collect(
            &cfg,
            &config_path(explicit_config),
            "unavailable",
            &diagnostics_id,
        );
        match submit_support_diagnostics(
            &feedback_store,
            &diagnostics_id,
            verdict,
            &comment,
            diagnostics,
        )
        .await
        {
            Ok(_) => {
                eprintln!(
                    "[skippr] uploaded redacted diagnostics without a thread ({diagnostics_id})"
                );
            }
            Err(e) => {
                eprintln!("[skippr] ERROR: failed to upload diagnostics: {e}");
                std::process::exit(1);
            }
        }
        return;
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

async fn submit_support_diagnostics(
    bundle: &FeedbackStoreBundle,
    diagnostics_id: &str,
    verdict: react_core::thread_feedback::ThreadFeedbackVerdict,
    comment: &str,
    payload: serde_json::Value,
) -> Result<(), String> {
    let key = support_diagnostics_key(bundle.keyspace.as_ref(), &bundle.scope, diagnostics_id);
    let value = serde_json::json!({
        "diagnostics_id": diagnostics_id,
        "thread_id": null,
        "verdict": verdict,
        "comment": comment,
        "created_at": chrono::Utc::now().to_rfc3339(),
        "payload": payload,
    });
    react_core::storage::retry_put_json(bundle.storage.as_ref(), &key, &value)
        .await
        .map_err(|e| e.to_string())
}

fn support_diagnostics_key(
    keyspace: &dyn react_core::keyspace::Keyspace,
    scope: &react_core::scope::RequestScope,
    diagnostics_id: &str,
) -> String {
    keyspace.scoped_key(
        scope,
        &["support", "diagnostics", &format!("{diagnostics_id}.json")],
    )
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
        s3_credentials: Some(translate::s3_credentials_from_auth(creds)),
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
        react_module_storage_s3::S3StorageAdapter::from_resolved_credentials(bucket, creds).await
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

fn main() {
    let stack_size = std::env::var("SKIPPR_MAIN_THREAD_STACK_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(32 * 1024 * 1024);

    let handle = std::thread::Builder::new()
        .name("skippr-main".to_string())
        .stack_size(stack_size)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build Tokio runtime");
            runtime.block_on(async_main());
        })
        .expect("failed to spawn skippr main thread");

    if let Err(panic) = handle.join() {
        std::panic::resume_unwind(panic);
    }
}

async fn async_main() {
    let cli = Cli::parse();

    match cli.cmd {
        Cmd::Init { name, reset } => cmd_init(&name, reset, &cli.config).await,
        Cmd::Connect { target } => match target {
            ConnectTarget::Warehouse { kind } => cmd_connect_warehouse(kind, &cli.config),
            ConnectTarget::Source { kind } => cmd_connect_source(kind, &cli.config),
        },
        Cmd::Doctor => cmd_doctor(&cli.config),
        Cmd::Discover(args) => cmd_discover(cli.log, &cli.config, args).await,
        Cmd::Sync(args) => cmd_sync(cli.log, &cli.config, args).await,
        Cmd::Model(args) => cmd_model(cli.log, &cli.config, args).await,
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
                    let token_provider = create_token_provider(&tokens);
                    let authenticated =
                        api_client::ApiClient::authenticated(&base_url, token_provider);
                    if let Err(e) = ensure_eula_accepted(&authenticated, true).await {
                        eprintln!("{}", e);
                        std::process::exit(1);
                    }
                    println!();
                    println!("  Logged in successfully.");
                    println!();
                    println!("  Next steps:");
                    println!("    skippr user account       — view balance");
                    println!("    skippr user buy-credits   — add funds");
                    println!("    skippr discover/sync/model — start a pipeline");
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
    let client = api_client::ApiClient::authenticated(&base_url, tokens);
    if let Err(e) = ensure_eula_accepted(&client, true).await {
        eprintln!("{}", e);
        std::process::exit(1);
    }
    client
}

async fn ensure_eula_accepted(
    client: &api_client::ApiClient,
    allow_prompt: bool,
) -> Result<(), String> {
    let account = client
        .get_account()
        .await
        .map_err(|e| format!("Could not verify EULA acceptance: {e}"))?;
    if account.eula.version.as_deref() == Some(SKIPPR_EULA_VERSION) {
        return Ok(());
    }

    if !allow_prompt {
        return Err(format!(
            "Skippr EULA acceptance is required before using SKIPPR_API_KEY. Run 'skippr user login' interactively once and accept {SKIPPR_EULA_URL}."
        ));
    }

    println!();
    println!("Skippr requires acceptance of the End User License Agreement:");
    println!("  {SKIPPR_EULA_URL}");
    println!();
    print!("Type exactly 'yes' to accept version {SKIPPR_EULA_VERSION}: ");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|e| format!("Failed to read EULA acceptance: {e}"))?;
    if answer.trim() != "yes" {
        return Err("EULA not accepted. Aborting.".to_string());
    }

    client
        .accept_eula(SKIPPR_EULA_VERSION)
        .await
        .map_err(|e| format!("Failed to record EULA acceptance: {e}"))?;
    println!("EULA accepted.");
    Ok(())
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
            if account.eula.version.as_deref() == Some(SKIPPR_EULA_VERSION) {
                let accepted_at = account.eula.accepted_at.as_deref().unwrap_or("recorded");
                let accepted_via = account.eula.accepted_via.as_deref().unwrap_or("account");
                println!(
                    "  EULA:              accepted ({}, via {})",
                    accepted_at, accepted_via
                );
            }
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

# LLM credentials are issued by Skippr at runtime.

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

    #[test]
    fn react_config_merges_selected_data_sink_into_warehouse_provider() {
        let cfg: serde_yaml::Value = serde_yaml::from_str(
            r#"
skippr:
  workspace: cursor_semantic_validation
pipelines:
  cursor_semantic_validation:
    data_source: data_sources.mssql
    data_sink: data_sinks.snowflake
data_sources:
  mssql:
    Mssql:
      connection_string: server=tcp:127.0.0.1,1433;database=testdb
      tables: [dbo.customers]
data_sinks:
  snowflake:
    Snowflake:
      account: ACCT
      user: paul
      database: ANALYTICS
      schema: RAW
      warehouse: COMPUTE_WH
      role: ACCOUNTADMIN
      private_key_path: /tmp/snowflake_key.p8
"#,
        )
        .expect("yaml");

        let internal = react_config_from_engine_config(&cfg).expect("internal config");
        let providers = internal.providers.expect("providers");
        let wh = providers
            .get("warehouse")
            .and_then(|v| v.as_object())
            .expect("warehouse object");

        assert_eq!(wh.get("account").and_then(|v| v.as_str()), Some("ACCT"));
        assert_eq!(wh.get("user").and_then(|v| v.as_str()), Some("paul"));
        assert_eq!(
            wh.get("private_key_path").and_then(|v| v.as_str()),
            Some("/tmp/snowflake_key.p8")
        );
        assert!(providers.get("el").is_none());
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
    fn support_diagnostics_key_is_outside_thread_feedback_prefix() {
        let scope = react_core::scope::RequestScope::parse("tenant", "dev", "project").unwrap();
        let keyspace = react_core::keyspace::DefaultKeyspace::new("bucket".to_string());

        let key = support_diagnostics_key(&keyspace, &scope, "diag-123");

        assert_eq!(key, "tenant/dev/project/support/diagnostics/diag-123.json");
        assert!(!key.contains("/feedback/"));
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
