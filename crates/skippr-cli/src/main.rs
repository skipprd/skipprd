mod api_client;
mod auth;
mod chat_cmd;
mod dbt_cmd;
mod feedback_diagnostics;
mod headless_prep;
mod metadata_cmd;
mod public_config;
mod public_docs_search;
mod react_host;
mod run_results_parse;
mod test_cmd;
mod translate;
mod vector_ingest_docs;
mod workspace_run_lock;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

use clap::{Parser, Subcommand};
use react::config::ReactConfigFile;
use react_core::keyspace::Keyspace;
use react_suite_data_engineer::PipelineName;
use serde::{Deserialize, Serialize};
use sha2::Digest;

use public_config::{
    DbtConfig, S3Transform, SchemaSinkConfig, SkipprProjectConfig, SourceConfig, WarehouseConfig,
};

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

pub(crate) fn attach_s3_credentials_provider(
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
        /// Output mode: json or text. Defaults to text for terminal use.
        #[arg(long, default_value = "text")]
        output: String,
    },

    /// Reset Skippr-owned state for a single configured pipeline.
    Reset(ResetArgs),

    /// Configure a warehouse or source connection.
    Connect {
        /// Output mode: json or text. Defaults to text for terminal use.
        #[arg(long, default_value = "text")]
        output: String,
        #[command(subcommand)]
        target: ConnectTarget,
    },

    /// Check that all prerequisites are in place.
    Doctor {
        /// Output mode: json or text. Defaults to text for terminal use.
        #[arg(long, default_value = "text")]
        output: String,
    },

    /// Inspect Skippr configuration metadata.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },

    /// Discover schemas and persist pipeline metadata.
    Discover(EngineDiscoverArgs),

    /// Show or apply persisted pipeline metadata (schema fields per namespace).
    Metadata {
        #[command(subcommand)]
        action: metadata_cmd::MetadataAction,
    },

    /// Extract and load data into the configured destination.
    Sync(EngineSyncArgs),

    /// Run the data-engineer modeling workflow.
    Model(ModelArgs),

    /// List or run dbt tests for a pipeline (materializes dbt from cloud storage, same runner as modeling).
    Test {
        #[command(subcommand)]
        action: test_cmd::TestSubcommand,
    },

    /// dbt helpers (compile model SQL for IDE query runs).
    Dbt {
        #[command(subcommand)]
        action: dbt_cmd::DbtSubcommand,
    },

    /// Answer a read-only data-engineering question.
    Ask(AskArgs),

    /// Produce a data-engineering plan without applying changes.
    Plan(PlanArgs),

    /// Run a read-only SQL query against the configured warehouse.
    Query(QueryArgs),

    /// Build and inspect the DE-suite catalog lineage graph.
    Lineage {
        #[command(subcommand)]
        action: LineageAction,
    },

    /// Chat with the data-engineer agent (react threads): ask / plan / agent, list threads, docs search.
    Chat {
        #[command(subcommand)]
        action: chat_cmd::ChatAction,
    },

    /// Attach human feedback to a project thread run.
    Feedback {
        /// Pipeline whose thread scope should receive feedback.
        #[arg(long)]
        pipeline: PipelineName,
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

    /// Vector store operations (e.g. documentation ingest to Lance).
    Vector {
        #[command(subcommand)]
        action: VectorAction,
    },

    /// React thread utilities (resolve latest thread from S3/local storage).
    Thread {
        #[command(subcommand)]
        action: ThreadAction,
    },

    /// User account management (signup, login, balance, etc.).
    User {
        /// Output mode: json or text. Defaults to text for terminal use.
        #[arg(long, default_value = "text")]
        output: String,
        #[command(subcommand)]
        action: UserAction,
    },

    /// Workspace run registry helpers (CI / recovery).
    Runs {
        #[command(subcommand)]
        action: RunsAction,
    },
}

#[derive(Subcommand, Debug)]
enum RunsAction {
    /// Release an active workspace heavy lock left behind after crash or SIGKILL.
    ReleaseLock {
        /// Cloud workspace name from skippr.yaml (`skippr.workspace`).
        #[arg(long)]
        workspace: String,
    },
}

#[derive(Subcommand, Debug)]
enum ThreadAction {
    /// Resolve the latest react thread id for a pipeline (storage source of truth).
    Resolve {
        #[arg(long)]
        pipeline: PipelineName,
        #[arg(long, default_value = "json")]
        output: String,
    },
}

#[derive(Subcommand, Debug, Clone)]
enum VectorAction {
    /// Chunk, embed, and upsert declarative doc trees into tenant Lance storage (DE path).
    IngestDocs {
        /// Pipeline entry under `pipelines:` with `vector_source` (default: `vector_ingest`).
        #[arg(long, default_value = "vector_ingest")]
        pipeline: PipelineName,
        /// Override `pipelines.<name>.vector_source` for this run (must match a `vector_sources` key).
        #[arg(long)]
        vector_source: Option<String>,
        /// Override scan root for this run (default: `root` from the selected vector source).
        #[arg(long)]
        src_path: Option<PathBuf>,
        /// Override chunk size in characters.
        #[arg(long)]
        chunk_chars: Option<usize>,
        /// Override chunk overlap in characters.
        #[arg(long)]
        chunk_overlap: Option<usize>,
        /// Extra include glob (repeatable); merged after YAML includes.
        #[arg(long = "include-glob")]
        include_glob: Vec<String>,
        /// Extra exclude glob (repeatable); merged after YAML excludes.
        #[arg(long = "exclude-glob")]
        exclude_glob: Vec<String>,
        /// Resolve files and chunk counts only (no embed / no Lance writes).
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        /// Output mode: text or json.
        #[arg(long, default_value = "text")]
        output: String,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigAction {
    /// Emit the public Skippr config schema. Defaults to JSON for IDE consumption.
    Schema {
        /// Output mode: json or text.
        #[arg(long, default_value = "json")]
        output: String,
    },
    /// Show the active Skippr config summary.
    Show {
        /// Output mode: json or text.
        #[arg(long, default_value = "json")]
        output: String,
    },
}

#[derive(Parser, Debug, Clone)]
struct EngineDiscoverArgs {
    /// The pipeline to use.
    #[arg(short, long)]
    pipeline: String,
    /// Output mode: progress, json, or text.
    #[arg(long, default_value = "progress")]
    output: String,
}

#[derive(Parser, Debug, Clone)]
struct EngineSyncArgs {
    /// The pipeline to use.
    #[arg(short, long)]
    pipeline: PipelineName,
    /// Output mode: progress, json, or text.
    #[arg(long, default_value = "progress")]
    output: String,
    /// Run a single sync pass and exit.
    #[arg(long, default_value_t = false)]
    once: bool,
}

#[derive(Parser, Debug, Clone)]
struct ResetArgs {
    /// The configured pipeline to reset.
    #[arg(short, long)]
    pipeline: PipelineName,
    /// Skip the interactive confirmation prompt.
    #[arg(long, default_value_t = false)]
    yes: bool,
}

#[derive(Parser, Debug, Clone)]
struct ModelArgs {
    /// Pipeline to model. The modeling warehouse is derived from this pipeline's data sink.
    #[arg(long)]
    pipeline: PipelineName,
    /// Start a fresh modeling thread instead of resuming the latest project thread.
    #[arg(long, default_value_t = false)]
    no_resume: bool,
    /// Local dbt project output path for model authoring. Defaults to ./<pipeline>/dbt.
    #[arg(long = "dbt-output-path")]
    dbt_output_path: Option<PathBuf>,
    /// Output mode: text, json, or jsonl.
    #[arg(long, default_value = "text")]
    output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ModelThreadBinding {
    schema_version: u32,
    pipeline: String,
    config_fingerprint: String,
    config_path: String,
    thread_id: String,
    updated_at: String,
}

#[derive(Parser, Debug, Clone)]
struct AskArgs {
    /// Pipeline to inspect.
    #[arg(long)]
    pipeline: PipelineName,
    /// Read-only question to answer.
    #[arg(long)]
    question: String,
    /// Output mode: text, json, or jsonl.
    #[arg(long, default_value = "text")]
    output: String,
}

#[derive(Parser, Debug, Clone)]
struct PlanArgs {
    /// Pipeline to inspect.
    #[arg(long)]
    pipeline: PipelineName,
    /// Optional modeling or data-engineering goal.
    #[arg(long)]
    goal: Option<String>,
    /// Output mode: text, json, or jsonl.
    #[arg(long, default_value = "text")]
    output: String,
}

#[derive(Parser, Debug, Clone)]
struct QueryArgs {
    /// Pipeline whose configured warehouse should execute the query.
    #[arg(long)]
    pipeline: PipelineName,
    /// Read-only SQL to execute. Only SELECT/WITH queries are accepted.
    #[arg(long, allow_hyphen_values = true)]
    sql: String,
    /// Output mode: json or jsonl.
    #[arg(long, default_value = "json")]
    output: String,
}

#[derive(Subcommand, Debug, Clone)]
enum LineageAction {
    /// Rebuild and persist the catalog lineage graph.
    Refresh(LineageRefreshArgs),
    /// Read the persisted lineage graph, optionally sliced around a field node.
    Graph(LineageGraphArgs),
    /// Analyze warehouse query history and merge query/dashboard evidence into lineage.
    ImportQueryHistory(LineageImportQueryHistoryArgs),
}

#[derive(Parser, Debug, Clone)]
struct LineageRefreshArgs {
    /// Pipeline whose configured DE suite scope should hold lineage.
    #[arg(long)]
    pipeline: PipelineName,
    /// Also import recent warehouse query history while refreshing.
    #[arg(long, default_value_t = false)]
    include_query_history: bool,
    /// Query history lower bound. Provider-specific timestamp string.
    #[arg(long)]
    since: Option<String>,
    /// Maximum query history rows to inspect.
    #[arg(long, default_value_t = 100)]
    limit: usize,
    /// Output mode: json or jsonl.
    #[arg(long, default_value = "json")]
    output: String,
}

#[derive(Parser, Debug, Clone)]
struct LineageGraphArgs {
    /// Pipeline whose configured DE suite scope should be read. Omit to merge all persisted pipeline lineage graphs.
    #[arg(long)]
    pipeline: Option<PipelineName>,
    /// Field node id to center the graph on.
    #[arg(long)]
    field_node_id: Option<String>,
    /// Direction: upstream, downstream, or both.
    #[arg(long, default_value = "both")]
    direction: String,
    /// Output mode: json or jsonl.
    #[arg(long, default_value = "json")]
    output: String,
}

#[derive(Parser, Debug, Clone)]
struct LineageImportQueryHistoryArgs {
    /// Pipeline whose configured warehouse query history should be inspected.
    #[arg(long)]
    pipeline: PipelineName,
    /// Query history lower bound. Provider-specific timestamp string.
    #[arg(long)]
    since: Option<String>,
    /// Maximum query history rows to inspect.
    #[arg(long, default_value_t = 100)]
    limit: usize,
    /// Include non-SELECT warehouse statements (DDL, etc.) in query-history lineage.
    #[arg(long, default_value_t = false)]
    include_non_select: bool,
    /// Output mode: json or jsonl.
    #[arg(long, default_value = "json")]
    output: String,
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
    /// Google Analytics 4 source (Data API daily reports).
    GoogleAnalytics {
        /// GA4 property ID (numeric, without the `properties/` prefix).
        #[arg(long)]
        property_id: Option<String>,
        /// First date to sync (YYYY-MM-DD).
        #[arg(long)]
        start_date: Option<String>,
        /// Last date to sync (YYYY-MM-DD). Omit to sync through today.
        #[arg(long)]
        end_date: Option<String>,
        /// Re-fetch this many days before the checkpoint on each run (plugin default: 3).
        #[arg(long)]
        lookback_days: Option<u32>,
        /// Bronze catalog profile: minimal, standard, or full (default: full).
        #[arg(long)]
        stream_profile: Option<String>,
        /// Include dimension rows with zero metrics in API responses (default: true).
        #[arg(long)]
        keep_empty_rows: Option<bool>,
        /// Do not sync the last N calendar days while GA4 is still processing (default: 1).
        #[arg(long)]
        processing_lag_days: Option<u32>,
        /// Days per runReport dateRanges chunk; use 1 for best accuracy (default: 1).
        #[arg(long)]
        window_in_days: Option<u32>,
        /// OAuth access token, or use ${GA4_ACCESS_TOKEN}.
        #[arg(long)]
        access_token: Option<String>,
        #[arg(long)]
        oauth_token_url: Option<String>,
        #[arg(long)]
        oauth_client_id: Option<String>,
        #[arg(long)]
        oauth_client_secret: Option<String>,
        #[arg(long)]
        oauth_refresh_token: Option<String>,
        #[arg(long)]
        service_account_json_path: Option<String>,
        /// Curated namespace(s) to sync; overrides stream_profile when set.
        #[arg(long, value_delimiter = ',')]
        streams: Option<Vec<String>>,
    },
    /// Google Search Console source (Search Analytics daily reports).
    GoogleSearchConsole {
        /// Site URL (`https://example.com/` or `sc-domain:example.com`).
        #[arg(long)]
        site_url: Option<String>,
        #[arg(long)]
        start_date: Option<String>,
        #[arg(long)]
        end_date: Option<String>,
        #[arg(long)]
        lookback_days: Option<u32>,
        #[arg(long)]
        stream_profile: Option<String>,
        #[arg(long)]
        processing_lag_days: Option<u32>,
        #[arg(long)]
        window_in_days: Option<u32>,
        #[arg(long)]
        access_token: Option<String>,
        #[arg(long)]
        oauth_token_url: Option<String>,
        #[arg(long)]
        oauth_client_id: Option<String>,
        #[arg(long)]
        oauth_client_secret: Option<String>,
        #[arg(long)]
        oauth_refresh_token: Option<String>,
        #[arg(long)]
        service_account_json_path: Option<String>,
        #[arg(long, value_delimiter = ',')]
        streams: Option<Vec<String>>,
        #[arg(long)]
        search_type: Option<String>,
        #[arg(long)]
        data_state: Option<String>,
        #[arg(long)]
        row_limit: Option<u32>,
        #[arg(long)]
        url_inspection_enabled: Option<bool>,
        #[arg(long, value_delimiter = ',')]
        url_list: Option<Vec<String>>,
    },
    /// Bing Webmaster Tools source (search performance and crawl reports).
    BingWebmasterTools {
        /// Verified site URL (e.g. https://example.com/).
        #[arg(long)]
        site_url: Option<String>,
        #[arg(long)]
        start_date: Option<String>,
        #[arg(long)]
        end_date: Option<String>,
        #[arg(long)]
        lookback_days: Option<u32>,
        #[arg(long)]
        stream_profile: Option<String>,
        #[arg(long)]
        processing_lag_days: Option<u32>,
        #[arg(long)]
        window_in_days: Option<u32>,
        /// API key from Bing Webmaster Tools Settings → API Access.
        #[arg(long)]
        api_key: Option<String>,
        #[arg(long)]
        access_token: Option<String>,
        #[arg(long)]
        oauth_token_url: Option<String>,
        #[arg(long)]
        oauth_client_id: Option<String>,
        #[arg(long)]
        oauth_client_secret: Option<String>,
        #[arg(long)]
        oauth_refresh_token: Option<String>,
        #[arg(long, value_delimiter = ',')]
        streams: Option<Vec<String>>,
    },
    /// Google PageSpeed Insights source (Lighthouse lab + CrUX field via API v5).
    GooglePageSpeed {
        /// Site origin to sample (e.g. https://example.com).
        #[arg(long)]
        site: Option<String>,
        #[arg(long)]
        api_key: Option<String>,
        /// `tld_sample` (robots + sitemap) or `url_list`.
        #[arg(long)]
        url_mode: Option<String>,
        #[arg(long, value_delimiter = ',')]
        url_list: Option<Vec<String>>,
        #[arg(long)]
        max_urls: Option<u32>,
        #[arg(long, value_delimiter = ',')]
        strategies: Option<Vec<String>>,
        #[arg(long, value_delimiter = ',')]
        categories: Option<Vec<String>>,
        #[arg(long)]
        locale: Option<String>,
        #[arg(long)]
        max_requests_per_run: Option<u32>,
        #[arg(long)]
        requests_per_minute: Option<u32>,
        #[arg(long)]
        respect_robots: Option<bool>,
        #[arg(long)]
        top_audits_per_page: Option<u32>,
        #[arg(long)]
        max_concurrent_requests: Option<u32>,
    },
    /// SEO crawl source (robots, sitemap, static HTML, content blocks, OpenAI AEO scores).
    SeoCrawl {
        #[arg(long)]
        site: Option<String>,
        #[arg(long)]
        max_urls: Option<u32>,
        #[arg(long)]
        max_depth: Option<u32>,
        #[arg(long)]
        crawl_rate_per_second: Option<f64>,
        #[arg(long)]
        respect_robots: Option<bool>,
        #[arg(long)]
        openai_enabled: Option<bool>,
        #[arg(long)]
        openai_model: Option<String>,
        #[arg(long)]
        openai_analyze_blocks: Option<bool>,
        #[arg(long)]
        openai_max_blocks_per_page: Option<u32>,
        #[arg(long)]
        skip_unchanged_content: Option<bool>,
        #[arg(long)]
        user_agent: Option<String>,
    },
    /// Site Quality source (Playwright lab: vitals, axe, Lighthouse).
    SiteQuality {
        #[arg(long)]
        site: Option<String>,
        #[arg(long)]
        url_mode: Option<String>,
        #[arg(long, value_delimiter = ',')]
        url_list: Option<Vec<String>>,
        #[arg(long)]
        max_pages_per_run: Option<u32>,
        #[arg(long)]
        wait_until: Option<String>,
        #[arg(long)]
        navigation_timeout_ms: Option<u32>,
        #[arg(long)]
        lighthouse_enabled: Option<bool>,
        #[arg(long, value_delimiter = ',')]
        lighthouse_categories: Option<Vec<String>>,
        #[arg(long)]
        axe_enabled: Option<bool>,
        #[arg(long, value_delimiter = ',')]
        axe_tags: Option<Vec<String>>,
        #[arg(long)]
        pages_per_minute: Option<u32>,
        #[arg(long)]
        worker_node_path: Option<String>,
        #[arg(long)]
        playwright_executable_path: Option<String>,
        #[arg(long)]
        respect_robots: Option<bool>,
        #[arg(long)]
        skip_heavy_when_unchanged: Option<bool>,
    },
    /// AI citations source (tracked prompts × OpenAI-compatible models).
    AiCitations {
        #[arg(long)]
        site: Option<String>,
        #[arg(long, value_delimiter = ',')]
        brand_names: Option<Vec<String>>,
        #[arg(long, value_delimiter = ',')]
        models: Option<Vec<String>>,
        #[arg(long)]
        requests_per_minute: Option<u32>,
        #[arg(long)]
        max_prompts_per_run: Option<u32>,
        #[arg(long)]
        skip_unchanged_responses: Option<bool>,
        #[arg(long)]
        openai_base_url: Option<String>,
    },
    /// Google organic rank tracking for configured target domains (low-volume Playwright).
    GoogleSerpRanks {
        #[arg(long)]
        target_site: Option<String>,
        #[arg(long, value_delimiter = ',')]
        target_aliases: Option<Vec<String>>,
        #[arg(long, value_delimiter = ',')]
        keywords: Option<Vec<String>>,
        #[arg(long)]
        country: Option<String>,
        #[arg(long)]
        language: Option<String>,
        #[arg(long)]
        device: Option<String>,
        #[arg(long)]
        max_depth: Option<u32>,
        #[arg(long)]
        min_query_interval_ms: Option<u64>,
        #[arg(long)]
        max_queries_per_run: Option<u32>,
        #[arg(long)]
        stop_after_first_target_match: Option<bool>,
        #[arg(long)]
        capture_results: Option<bool>,
        #[arg(long)]
        force_refresh_today: Option<bool>,
        #[arg(long)]
        navigation_timeout_ms: Option<u32>,
        #[arg(long)]
        worker_node_path: Option<String>,
        #[arg(long)]
        playwright_executable_path: Option<String>,
        #[arg(long)]
        user_agent: Option<String>,
    },
    /// Apple Search Ads source (Campaign Management API v5 daily reports).
    AppleSearchAds {
        #[arg(long)]
        org_id: Option<String>,
        #[arg(long)]
        client_id: Option<String>,
        #[arg(long)]
        team_id: Option<String>,
        #[arg(long)]
        key_id: Option<String>,
        #[arg(long)]
        private_key_path: Option<String>,
        #[arg(long)]
        private_key_pem: Option<String>,
        #[arg(long)]
        start_date: Option<String>,
        #[arg(long)]
        end_date: Option<String>,
        #[arg(long)]
        lookback_days: Option<u32>,
        #[arg(long)]
        stream_profile: Option<String>,
        #[arg(long)]
        processing_lag_days: Option<u32>,
        #[arg(long)]
        time_zone: Option<String>,
        #[arg(long)]
        return_records_with_no_metrics: Option<bool>,
        #[arg(long)]
        access_token: Option<String>,
        #[arg(long, value_delimiter = ',')]
        streams: Option<Vec<String>>,
        #[arg(long)]
        max_concurrent_requests: Option<u32>,
    },
    /// Meta Instagram Ads source (Marketing API daily insights).
    MetaInstagramAds {
        #[arg(long)]
        ad_account_id: Option<String>,
        #[arg(long)]
        start_date: Option<String>,
        #[arg(long)]
        end_date: Option<String>,
        #[arg(long)]
        lookback_days: Option<u32>,
        #[arg(long)]
        stream_profile: Option<String>,
        #[arg(long)]
        processing_lag_days: Option<u32>,
        #[arg(long)]
        api_version: Option<String>,
        #[arg(long)]
        access_token: Option<String>,
        #[arg(long)]
        oauth_token_url: Option<String>,
        #[arg(long)]
        oauth_client_id: Option<String>,
        #[arg(long)]
        oauth_client_secret: Option<String>,
        #[arg(long)]
        oauth_refresh_token: Option<String>,
        #[arg(long)]
        instagram_filter: Option<bool>,
        #[arg(long, value_delimiter = ',')]
        streams: Option<Vec<String>>,
    },
    /// DataForSEO Backlinks API v3 (backlinks + page intersection live).
    DataForSeoBacklinks {
        #[arg(long)]
        login: Option<String>,
        #[arg(long)]
        password: Option<String>,
        #[arg(long)]
        site: Option<String>,
        #[arg(long)]
        run_mode: Option<String>,
        #[arg(long)]
        backlink_target: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
        #[arg(long)]
        max_pages: Option<u32>,
        #[arg(long)]
        request_interval_ms: Option<u64>,
    },
    /// DataForSEO SEO opportunities (keyword research, SERP weakness, AI citation).
    DataForSeoSeoOpportunities {
        #[arg(long)]
        login: Option<String>,
        #[arg(long)]
        password: Option<String>,
        #[arg(long)]
        site: Option<String>,
        #[arg(long)]
        location_code: Option<u32>,
        #[arg(long)]
        language_code: Option<String>,
        #[arg(long)]
        device: Option<String>,
        #[arg(long)]
        run_mode: Option<String>,
        #[arg(long, value_delimiter = ',')]
        seed_keywords: Option<Vec<String>>,
        #[arg(long)]
        request_interval_ms: Option<u64>,
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

pub(crate) fn is_json_output(output: &str) -> bool {
    output.eq_ignore_ascii_case("json")
}

pub(crate) fn is_jsonl_output(output: &str) -> bool {
    output.eq_ignore_ascii_case("jsonl")
}

pub(crate) fn print_json<T: Serialize>(value: &T) {
    match serde_json::to_string_pretty(value) {
        Ok(json) => println!("{json}"),
        Err(err) => {
            eprintln!("error: failed to serialize JSON output: {err}");
            std::process::exit(1);
        }
    }
}

pub(crate) fn print_json_line<T: Serialize>(value: &T) {
    match serde_json::to_string(value) {
        Ok(json) => println!("{json}"),
        Err(err) => {
            eprintln!("error: failed to serialize JSONL output: {err}");
            std::process::exit(1);
        }
    }
}

fn event_timestamp() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[derive(Serialize)]
struct DeSuiteEvent<'a> {
    event: &'a str,
    timestamp: String,
    pipeline: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    run_kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thread_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    phase: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repair_status: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pending_plan_revision: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_summary: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    answer: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    plan: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ok: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_preflight: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    changed_files: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    validation: Option<serde_json::Value>,
}

fn emit_de_suite_event(output: &str, event: DeSuiteEvent<'_>) {
    if is_jsonl_output(output) {
        print_json_line(&event);
    }
}

#[derive(Serialize)]
struct InitResult {
    ok: bool,
    config_path: String,
    project: String,
    created_files: Vec<String>,
    next_steps: Vec<String>,
}

#[derive(Serialize)]
struct ConnectResult {
    ok: bool,
    config_path: String,
    connector_kind: String,
    section_updated: String,
    pipeline_references_updated: bool,
    missing_recommended_fields: Vec<String>,
}

#[derive(Serialize)]
struct DoctorResult {
    ok: bool,
    config_path: Option<String>,
    checks: Vec<DoctorCheck>,
}

#[derive(Serialize)]
struct DoctorCheck {
    ok: bool,
    severity: &'static str,
    message: String,
    suggested_fix_command: Option<String>,
}

#[derive(Serialize)]
struct ConfigSchema {
    version: u32,
    default_config_file: &'static str,
    commands: ConfigSchemaCommands,
    sources: Vec<ConnectorSchema>,
    warehouses: Vec<ConnectorSchema>,
}

#[derive(Serialize)]
struct ConfigSchemaCommands {
    init: &'static str,
    connect_source: &'static str,
    connect_warehouse: &'static str,
    doctor: &'static str,
    show: &'static str,
}

#[derive(Serialize)]
struct ConnectorSchema {
    kind: &'static str,
    label: &'static str,
    fields: Vec<FieldSchema>,
}

#[derive(Serialize)]
struct FieldSchema {
    name: &'static str,
    label: &'static str,
    field_type: &'static str,
    required: bool,
    secret: bool,
    repeatable: bool,
    description: &'static str,
    example: Option<&'static str>,
}

#[derive(Serialize)]
struct ConfigShowResult {
    ok: bool,
    config_path: String,
    workspace: Option<String>,
    pipelines: Vec<String>,
    sources: Vec<String>,
    sinks: Vec<String>,
    schema_sinks: Vec<String>,
    connections: ConfigShowConnections,
}

#[derive(Serialize)]
struct ConfigShowConnections {
    sources: Vec<ConfigShowConnection>,
    sinks: Vec<ConfigShowConnection>,
    schema_sinks: Vec<ConfigShowConnection>,
}

#[derive(Serialize)]
struct ConfigShowConnection {
    name: String,
    provider: Option<String>,
    label: String,
    fields: Vec<ConfigShowConnectionField>,
    pipelines: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    schema_sink: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    linked_sinks: Vec<String>,
    supports_sql: bool,
}

#[derive(Serialize)]
struct ConfigShowConnectionField {
    name: String,
    value: String,
    secret: bool,
}

fn working_dir() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Set the process working directory to the folder containing the Skippr manifest.
pub(crate) fn ensure_project_working_dir(explicit: &Option<PathBuf>) {
    let path = config_path(explicit);
    let Some(parent) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) else {
        return;
    };
    if let Err(err) = std::env::set_current_dir(parent) {
        eprintln!(
            "skippr: warning: could not set working directory to {}: {err}",
            parent.display()
        );
    }
}

pub(crate) fn config_path(explicit: &Option<PathBuf>) -> PathBuf {
    explicit.clone().unwrap_or_else(|| {
        let cwd = working_dir();
        let yml = cwd.join("skippr.yml");
        let yaml = cwd.join("skippr.yaml");
        if yml.exists() || !yaml.exists() {
            yml
        } else {
            yaml
        }
    })
}

/// Loads dotenv files next to the Skippr manifest so whole-scalar `${VAR}` references in YAML resolve.
///
/// Reads from the directory containing `skippr.yml` / `skippr.yaml` (the `--config` file when passed):
/// - **`.env`** — [`dotenvy::from_path`]: only sets variables not already present in the process environment.
/// - **`.env.local`** (optional) — [`dotenvy::from_path_override`]: overrides every variable **named in that file**
///   (typical gitignored local overrides).
pub(crate) fn load_dotenv_for_skippr_config_yaml_path(config_yaml_path: &Path) {
    skipprd::helpers::dotenv::load_dotenv_for_config_yaml_path(config_yaml_path);
}

fn load_config(explicit: &Option<PathBuf>) -> Result<SkipprProjectConfig, String> {
    let path = config_path(explicit);
    load_dotenv_for_skippr_config_yaml_path(&path);
    SkipprProjectConfig::load_from(&path)
}

fn save_config(cfg: &SkipprProjectConfig, explicit: &Option<PathBuf>) -> Result<(), String> {
    cfg.save_to(&config_path(explicit))
}

pub(crate) fn load_engine_config(explicit: &Option<PathBuf>) -> Result<serde_yaml::Value, String> {
    let path = config_path(explicit);
    load_dotenv_for_skippr_config_yaml_path(&path);
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("failed to read {}: {}", path.display(), e))?;
    serde_yaml::from_str(&raw).map_err(|e| format!("failed to parse {}: {}", path.display(), e))
}

fn load_resolved_engine_config(explicit: &Option<PathBuf>) -> Result<serde_yaml::Value, String> {
    let path = config_path(explicit);
    let value = load_engine_config(explicit)?;
    let mut json_value = serde_json::to_value(value)
        .map_err(|e| format!("failed to normalize {}: {}", path.display(), e))?;
    skipprd::helpers::configuration::Config::resolve_env_refs_in_json_value(&mut json_value)?;
    serde_yaml::to_value(json_value)
        .map_err(|e| format!("failed to convert resolved {}: {}", path.display(), e))
}

fn save_engine_config(explicit: &Option<PathBuf>, value: &serde_yaml::Value) -> Result<(), String> {
    let path = config_path(explicit);
    let raw = serde_yaml::to_string(value)
        .map_err(|e| format!("failed to serialize {}: {}", path.display(), e))?;
    std::fs::write(&path, raw).map_err(|e| format!("failed to write {}: {}", path.display(), e))
}

fn string_field(
    name: &'static str,
    label: &'static str,
    required: bool,
    secret: bool,
    description: &'static str,
    example: Option<&'static str>,
) -> FieldSchema {
    FieldSchema {
        name,
        label,
        field_type: "string",
        required,
        secret,
        repeatable: false,
        description,
        example,
    }
}

fn list_field(
    name: &'static str,
    label: &'static str,
    description: &'static str,
    example: Option<&'static str>,
) -> FieldSchema {
    FieldSchema {
        name,
        label,
        field_type: "string[]",
        required: false,
        secret: false,
        repeatable: true,
        description,
        example,
    }
}

fn config_schema() -> ConfigSchema {
    ConfigSchema {
        version: 1,
        default_config_file: "skippr.yml",
        commands: ConfigSchemaCommands {
            init: "skippr init <project>",
            connect_source: "skippr connect --output json source <kind>",
            connect_warehouse: "skippr connect --output json warehouse <kind>",
            doctor: "skippr doctor --output json",
            show: "skippr config show",
        },
        sources: vec![
            ConnectorSchema {
                kind: "s3",
                label: "Amazon S3",
                fields: vec![
                    string_field(
                        "bucket",
                        "Bucket",
                        true,
                        false,
                        "Source S3 bucket.",
                        Some("raw-events"),
                    ),
                    string_field(
                        "prefix",
                        "Prefix",
                        false,
                        false,
                        "Source object prefix.",
                        Some("events/"),
                    ),
                    string_field(
                        "namespace_fields",
                        "Namespace fields",
                        false,
                        false,
                        "Comma-separated fields used to namespace incoming events.",
                        Some("event_type"),
                    ),
                ],
            },
            ConnectorSchema {
                kind: "file",
                label: "Local file",
                fields: vec![string_field(
                    "path",
                    "Path",
                    true,
                    false,
                    "Local file or directory path.",
                    Some("./data/input.jsonl"),
                )],
            },
            ConnectorSchema {
                kind: "postgres-source",
                label: "PostgreSQL source",
                fields: vec![
                    string_field(
                        "connection_string",
                        "Connection string",
                        false,
                        true,
                        "Connection string or ${ENV_VAR} reference.",
                        Some("${POSTGRES_CONNECTION_STRING}"),
                    ),
                    string_field(
                        "host",
                        "Host",
                        false,
                        false,
                        "PostgreSQL host.",
                        Some("localhost"),
                    ),
                    string_field("user", "User", false, false, "PostgreSQL user.", None),
                    string_field(
                        "password",
                        "Password",
                        false,
                        true,
                        "PostgreSQL password or ${ENV_VAR} reference.",
                        Some("${POSTGRES_PASSWORD}"),
                    ),
                    string_field("database", "Database", false, false, "Database name.", None),
                    list_field(
                        "tables",
                        "Tables",
                        "Tables to ingest.",
                        Some("public.orders,public.customers"),
                    ),
                ],
            },
            ConnectorSchema {
                kind: "mysql",
                label: "MySQL",
                fields: vec![
                    string_field(
                        "connection_string",
                        "Connection string",
                        false,
                        true,
                        "Connection string or ${ENV_VAR} reference.",
                        Some("${MYSQL_CONNECTION_STRING}"),
                    ),
                    list_field(
                        "tables",
                        "Tables",
                        "Tables to ingest.",
                        Some("orders,customers"),
                    ),
                ],
            },
            ConnectorSchema {
                kind: "mssql",
                label: "Microsoft SQL Server",
                fields: vec![string_field(
                    "connection_string",
                    "Connection string",
                    true,
                    true,
                    "ADO.NET connection string or ${ENV_VAR} reference.",
                    Some("${MSSQL_CONNECTION_STRING}"),
                )],
            },
        ],
        warehouses: vec![
            ConnectorSchema {
                kind: "athena",
                label: "AWS Athena",
                fields: vec![
                    string_field(
                        "workgroup",
                        "Workgroup",
                        false,
                        false,
                        "Athena workgroup.",
                        Some("primary"),
                    ),
                    string_field(
                        "region",
                        "Region",
                        false,
                        false,
                        "AWS region.",
                        Some("eu-west-1"),
                    ),
                    string_field(
                        "result_s3",
                        "Result S3 URI",
                        false,
                        false,
                        "Athena query result location.",
                        Some("s3://bucket/athena-results/"),
                    ),
                    string_field(
                        "schema",
                        "Schema",
                        false,
                        false,
                        "Default Glue database/schema.",
                        Some("analytics"),
                    ),
                ],
            },
            ConnectorSchema {
                kind: "snowflake",
                label: "Snowflake",
                fields: vec![
                    string_field(
                        "account",
                        "Account",
                        false,
                        false,
                        "Snowflake account identifier.",
                        None,
                    ),
                    string_field("user", "User", false, false, "Snowflake user.", None),
                    string_field(
                        "password",
                        "Password",
                        false,
                        true,
                        "Password or ${ENV_VAR} reference.",
                        Some("${SNOWFLAKE_PASSWORD}"),
                    ),
                    string_field(
                        "private_key_path",
                        "Private key path",
                        false,
                        true,
                        "Private key path for key-pair auth.",
                        Some("${SNOWFLAKE_PRIVATE_KEY_PATH}"),
                    ),
                    string_field(
                        "database",
                        "Database",
                        true,
                        false,
                        "Snowflake database.",
                        None,
                    ),
                    string_field("schema", "Schema", true, false, "Snowflake schema.", None),
                    string_field(
                        "warehouse",
                        "Warehouse",
                        true,
                        false,
                        "Snowflake compute warehouse.",
                        None,
                    ),
                    string_field("role", "Role", false, false, "Snowflake role.", None),
                ],
            },
            ConnectorSchema {
                kind: "bigquery",
                label: "BigQuery",
                fields: vec![
                    string_field("project", "Project", true, false, "GCP project.", None),
                    string_field("dataset", "Dataset", true, false, "BigQuery dataset.", None),
                    string_field(
                        "location",
                        "Location",
                        false,
                        false,
                        "BigQuery location.",
                        Some("US"),
                    ),
                ],
            },
            ConnectorSchema {
                kind: "postgres",
                label: "PostgreSQL warehouse",
                fields: vec![
                    string_field("database", "Database", true, false, "Database name.", None),
                    string_field(
                        "schema",
                        "Schema",
                        false,
                        false,
                        "Schema name.",
                        Some("public"),
                    ),
                ],
            },
        ],
    }
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

fn warn_and_normalize_legacy_cli_config(value: &mut serde_yaml::Value) -> Result<(), String> {
    if value.get("react").is_some() {
        return Err(
            "skippr.yml no longer supports a top-level react: section. Remove react: and run `skippr model --pipeline <name>`; model settings are derived from the selected pipeline's data sink and built-in defaults."
                .to_string(),
        );
    }
    if value.get("providers").is_some() {
        return Err(
            "skippr.yml no longer supports top-level providers:. Remove providers: and configure warehouses under data_sinks."
                .to_string(),
        );
    }
    if let Some(mapping) = value.as_mapping_mut() {
        if mapping.remove(yaml_key("dbt")).is_some() {
            eprintln!(
                "[skippr] WARNING: ignoring legacy top-level dbt: config; model dbt settings are now derived from built-in defaults."
            );
        }
        if let Some(skippr) = mapping
            .get_mut(yaml_key("skippr"))
            .and_then(|v| v.as_mapping_mut())
        {
            if skippr.remove(yaml_key("tenant")).is_some() {
                eprintln!(
                    "[skippr] WARNING: ignoring skippr.tenant from config; tenant comes from authenticated credentials."
                );
            }
        }
    }
    Ok(())
}

pub(crate) fn load_cli_execution_config(
    explicit: &Option<PathBuf>,
) -> Result<serde_yaml::Value, String> {
    let mut value = load_resolved_engine_config(explicit)?;
    warn_and_normalize_legacy_cli_config(&mut value)?;
    Ok(value)
}

fn load_cli_raw_config_for_save(explicit: &Option<PathBuf>) -> Result<serde_yaml::Value, String> {
    let mut value = load_engine_config(explicit)?;
    warn_and_normalize_legacy_cli_config(&mut value)?;
    Ok(value)
}

fn set_public_cli_el_storage_default() {
    if std::env::var("SKIPPRD_EL_STORAGE_MODE")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .is_none()
    {
        skipprd::helpers::configuration::Config::setenv("SKIPPRD_EL_STORAGE_MODE", "local");
    }
}

fn plugin_mapping_key(map: &serde_yaml::Mapping) -> Option<String> {
    map.iter()
        .filter_map(|(key, value)| {
            let key = key.as_str()?.trim();
            if key.is_empty() || key == "schema_sink" {
                return None;
            }
            value.as_mapping()?;
            Some(key.to_string())
        })
        .next()
}

/// Prefer known runtime plugin keys (e.g. `GoogleAnalytics`) over incidental mappings like `transform`.
const DATA_SOURCE_RUNTIME_PLUGIN_KEYS: &[&str] = &[
    "SeoCrawl",
    "AppleSearchAds",
    "MetaInstagramAds",
    "DataForSeoBacklinks",
    "DataForSeoSeoOpportunities",
    "GoogleAnalytics",
    "GoogleSearchConsole",
    "BingWebmasterTools",
    "GooglePageSpeed",
    "SiteQuality",
    "AiCitations",
    "GoogleSerpRanks",
    "S3",
    "File",
    "Mssql",
    "Mysql",
    "Postgres",
    "Kafka",
    "HttpClient",
    "HttpServer",
    "Websocket",
    "Kinesis",
    "Sqs",
    "Sns",
    "Amqp",
    "Mqtt",
    "Eventbridge",
    "Socket",
    "Stdin",
    "Statsd",
    "Pcap",
    "Dynamodb",
    "Mongodb",
    "Redshift",
    "Clickhouse",
    "Motherduck",
    "DeltaLake",
    "Sftp",
];

fn data_source_plugin_key(map: &serde_yaml::Mapping) -> Option<String> {
    for name in DATA_SOURCE_RUNTIME_PLUGIN_KEYS {
        if map
            .get(yaml_key(name))
            .and_then(|value| value.as_mapping())
            .is_some()
        {
            return Some(name.to_string());
        }
    }
    plugin_mapping_key(map)
}

pub(crate) fn validate_pipeline_exists(
    engine_cfg: &serde_yaml::Value,
    pipeline: &str,
) -> Result<(), String> {
    let pipelines = engine_cfg
        .get("pipelines")
        .and_then(|pipelines| pipelines.as_mapping())
        .ok_or_else(|| "skippr.yml must define pipelines".to_string())?;
    if pipelines.get(yaml_key(pipeline)).is_some() {
        return Ok(());
    }
    let known = pipelines
        .keys()
        .filter_map(|key| key.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "skippr.yml does not define pipeline '{}'. Known pipelines: {}",
        pipeline,
        if known.is_empty() { "<none>" } else { &known }
    ))
}

fn pipeline_data_sink_name(
    engine_cfg: &serde_yaml::Value,
    pipeline: &str,
) -> Result<String, String> {
    let pipeline_cfg = pipeline_config(engine_cfg, pipeline)?;
    let data_sink_ref = pipeline_cfg
        .get("data_sink")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("pipelines.{pipeline}.data_sink is required for `skippr model`"))?;
    Ok(data_sink_ref
        .strip_prefix("data_sinks.")
        .unwrap_or(data_sink_ref)
        .to_string())
}

fn pipeline_data_source_name(
    engine_cfg: &serde_yaml::Value,
    pipeline: &str,
) -> Result<Option<String>, String> {
    let pipeline_cfg = pipeline_config(engine_cfg, pipeline)?;
    let Some(data_source_ref) = pipeline_cfg
        .get("data_source")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    Ok(Some(
        data_source_ref
            .strip_prefix("data_sources.")
            .unwrap_or(data_source_ref)
            .to_string(),
    ))
}

fn selected_data_source_mapping<'a>(
    engine_cfg: &'a serde_yaml::Value,
    data_source_name: &str,
) -> Result<&'a serde_yaml::Mapping, String> {
    engine_cfg
        .get("data_sources")
        .and_then(|data_sources| data_sources.get(data_source_name))
        .ok_or_else(|| {
            format!(
                "skippr.yml does not define data source '{}'. Use one of the data_sources keys.",
                data_source_name
            )
        })?
        .as_mapping()
        .ok_or_else(|| format!("data_sources.{} must be a mapping", data_source_name))
}

fn selected_data_sink_mapping<'a>(
    engine_cfg: &'a serde_yaml::Value,
    data_sink_name: &str,
) -> Result<&'a serde_yaml::Mapping, String> {
    engine_cfg
        .get("data_sinks")
        .and_then(|data_sinks| data_sinks.get(data_sink_name))
        .ok_or_else(|| {
            format!(
                "skippr.yml does not define data sink '{}'. Use one of the data_sinks keys.",
                data_sink_name
            )
        })?
        .as_mapping()
        .ok_or_else(|| format!("data_sinks.{} must be a mapping", data_sink_name))
}

fn schema_sink_config_for_data_sink(
    engine_cfg: &serde_yaml::Value,
    data_sink_name: &str,
) -> Result<Option<SchemaSinkConfig>, String> {
    let schema_sink_ref = engine_cfg
        .get("data_sinks")
        .and_then(|data_sinks| data_sinks.get(data_sink_name))
        .and_then(|sink| sink.get("schema_sink"))
        .and_then(|value| value.as_str())
        .map(str::trim);
    let Some(schema_sink_ref) = schema_sink_ref.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let schema_sink_name = schema_sink_ref
        .strip_prefix("schema_sinks.")
        .unwrap_or(schema_sink_ref)
        .trim();
    let schema_sink = engine_cfg
        .get("schema_sinks")
        .and_then(|schema_sinks| schema_sinks.get(schema_sink_name))
        .ok_or_else(|| format!("data_sinks.{data_sink_name}.schema_sink references unknown schema_sinks.{schema_sink_name}"))?;
    if let Some(glue) = schema_sink.get("Glue") {
        let glue_database_name = glue
            .get("glue_database_name")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                format!("schema_sinks.{schema_sink_name}.Glue.glue_database_name is required")
            })?
            .to_string();
        return Ok(Some(SchemaSinkConfig::Glue { glue_database_name }));
    }
    Err(format!(
        "schema_sinks.{schema_sink_name} must contain a supported schema sink config such as Glue"
    ))
}

fn yaml_str(map: &serde_yaml::Mapping, key: &str) -> Option<String> {
    map.get(yaml_key(key))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn yaml_u32(map: &serde_yaml::Mapping, key: &str) -> Option<u32> {
    map.get(yaml_key(key)).and_then(|value| {
        value
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .or_else(|| {
                value
                    .as_i64()
                    .and_then(|n| u32::try_from(n).ok().filter(|_| n >= 0))
            })
            .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
    })
}

fn yaml_u64(map: &serde_yaml::Mapping, key: &str) -> Option<u64> {
    map.get(yaml_key(key)).and_then(|value| {
        value
            .as_u64()
            .or_else(|| {
                value
                    .as_i64()
                    .and_then(|n| u64::try_from(n).ok().filter(|_| n >= 0))
            })
            .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
    })
}

fn yaml_google_serp_targets(
    map: &serde_yaml::Mapping,
    key: &str,
) -> Option<Vec<crate::public_config::GoogleSerpTargetConfig>> {
    let value = map.get(yaml_key(key))?;
    let seq = value.as_sequence()?;
    let mut targets = Vec::new();
    for entry in seq {
        let mapping = entry.as_mapping()?;
        let site = mapping
            .get(yaml_key("site"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())?
            .to_string();
        let aliases = mapping
            .get(yaml_key("aliases"))
            .and_then(|v| v.as_sequence())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        item.as_str()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        targets.push(crate::public_config::GoogleSerpTargetConfig { site, aliases });
    }
    (!targets.is_empty()).then_some(targets)
}

fn yaml_bool(map: &serde_yaml::Mapping, key: &str) -> Option<bool> {
    map.get(yaml_key(key)).and_then(|value| {
        value.as_bool().or_else(|| {
            value.as_str().and_then(|s| match s.trim().to_ascii_lowercase().as_str() {
                "true" | "yes" | "1" => Some(true),
                "false" | "no" | "0" => Some(false),
                _ => None,
            })
        })
    })
}

fn yaml_tracked_prompt_list(
    map: &serde_yaml::Mapping,
    key: &str,
) -> Option<Vec<public_config::TrackedPromptEntry>> {
    map.get(&yaml_key(key))
        .and_then(|v| serde_yaml::from_value(v.clone()).ok())
}

fn yaml_string_vec(map: &serde_yaml::Mapping, key: &str) -> Option<Vec<String>> {
    let value = map.get(yaml_key(key))?;
    if let Some(seq) = value.as_sequence() {
        let items: Vec<String> = seq
            .iter()
            .filter_map(|entry| {
                entry
                    .as_str()
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                    .map(str::to_string)
            })
            .collect();
        return (!items.is_empty()).then_some(items);
    }
    let single = value.as_str().map(str::trim).filter(|item| !item.is_empty())?;
    let items: Vec<String> = single
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect();
    (!items.is_empty()).then_some(items)
}

fn result_s3_from_athena_results_bucket(bucket: Option<String>) -> Option<String> {
    bucket.map(|bucket| {
        let bucket = bucket.trim();
        if bucket.starts_with("s3://") {
            bucket.trim_end_matches('/').to_string() + "/"
        } else {
            format!("s3://{}/", bucket.trim_matches('/'))
        }
    })
}

fn source_config_from_data_source(
    engine_cfg: &serde_yaml::Value,
    data_source_name: &str,
) -> Result<SourceConfig, String> {
    let source = selected_data_source_mapping(engine_cfg, data_source_name)?;
    let plugin = data_source_plugin_key(source).ok_or_else(|| {
        format!(
            "data_sources.{} must contain a runtime plugin config such as S3:",
            data_source_name
        )
    })?;
    let plugin_cfg = source
        .get(yaml_key(&plugin))
        .and_then(|value| value.as_mapping())
        .ok_or_else(|| {
            format!(
                "data_sources.{} did not contain mapping config '{}'",
                data_source_name, plugin
            )
        })?;
    match plugin.as_str() {
        "S3" => Ok(SourceConfig::S3 {
            s3_bucket: yaml_str(plugin_cfg, "s3_bucket"),
            s3_prefix: yaml_str(plugin_cfg, "s3_prefix"),
            transform: None,
        }),
        "File" => Ok(SourceConfig::File {
            path: yaml_str(plugin_cfg, "path"),
        }),
        "Mssql" => Ok(SourceConfig::Mssql {
            connection_string: yaml_str(plugin_cfg, "connection_string"),
        }),
        "GoogleAnalytics" => Ok(SourceConfig::GoogleAnalytics {
            property_id: yaml_str(plugin_cfg, "property_id"),
            start_date: yaml_str(plugin_cfg, "start_date"),
            end_date: yaml_str(plugin_cfg, "end_date"),
            lookback_days: yaml_u32(plugin_cfg, "lookback_days"),
            stream_profile: yaml_str(plugin_cfg, "stream_profile"),
            keep_empty_rows: yaml_bool(plugin_cfg, "keep_empty_rows"),
            processing_lag_days: yaml_u32(plugin_cfg, "processing_lag_days"),
            window_in_days: yaml_u32(plugin_cfg, "window_in_days"),
            access_token: yaml_str(plugin_cfg, "access_token"),
            oauth_token_url: yaml_str(plugin_cfg, "oauth_token_url"),
            oauth_client_id: yaml_str(plugin_cfg, "oauth_client_id"),
            oauth_client_secret: yaml_str(plugin_cfg, "oauth_client_secret"),
            oauth_refresh_token: yaml_str(plugin_cfg, "oauth_refresh_token"),
            service_account_json_path: yaml_str(plugin_cfg, "service_account_json_path"),
            streams: yaml_string_vec(plugin_cfg, "streams"),
        }),
        "GoogleSearchConsole" => Ok(SourceConfig::GoogleSearchConsole {
            site_url: yaml_str(plugin_cfg, "site_url"),
            start_date: yaml_str(plugin_cfg, "start_date"),
            end_date: yaml_str(plugin_cfg, "end_date"),
            lookback_days: yaml_u32(plugin_cfg, "lookback_days"),
            stream_profile: yaml_str(plugin_cfg, "stream_profile"),
            processing_lag_days: yaml_u32(plugin_cfg, "processing_lag_days"),
            window_in_days: yaml_u32(plugin_cfg, "window_in_days"),
            access_token: yaml_str(plugin_cfg, "access_token"),
            oauth_token_url: yaml_str(plugin_cfg, "oauth_token_url"),
            oauth_client_id: yaml_str(plugin_cfg, "oauth_client_id"),
            oauth_client_secret: yaml_str(plugin_cfg, "oauth_client_secret"),
            oauth_refresh_token: yaml_str(plugin_cfg, "oauth_refresh_token"),
            service_account_json_path: yaml_str(plugin_cfg, "service_account_json_path"),
            streams: yaml_string_vec(plugin_cfg, "streams"),
            search_type: yaml_str(plugin_cfg, "search_type"),
            data_state: yaml_str(plugin_cfg, "data_state"),
            row_limit: yaml_u32(plugin_cfg, "row_limit"),
            url_inspection_enabled: yaml_bool(plugin_cfg, "url_inspection_enabled"),
            url_list: yaml_string_vec(plugin_cfg, "url_list"),
        }),
        "BingWebmasterTools" => Ok(SourceConfig::BingWebmasterTools {
            site_url: yaml_str(plugin_cfg, "site_url"),
            api_key: yaml_str(plugin_cfg, "api_key"),
            start_date: yaml_str(plugin_cfg, "start_date"),
            end_date: yaml_str(plugin_cfg, "end_date"),
            lookback_days: yaml_u32(plugin_cfg, "lookback_days"),
            stream_profile: yaml_str(plugin_cfg, "stream_profile"),
            processing_lag_days: yaml_u32(plugin_cfg, "processing_lag_days"),
            window_in_days: yaml_u32(plugin_cfg, "window_in_days"),
            access_token: yaml_str(plugin_cfg, "access_token"),
            oauth_token_url: yaml_str(plugin_cfg, "oauth_token_url"),
            oauth_client_id: yaml_str(plugin_cfg, "oauth_client_id"),
            oauth_client_secret: yaml_str(plugin_cfg, "oauth_client_secret"),
            oauth_refresh_token: yaml_str(plugin_cfg, "oauth_refresh_token"),
            streams: yaml_string_vec(plugin_cfg, "streams"),
        }),
        "GooglePageSpeed" => Ok(SourceConfig::GooglePageSpeed {
            site: yaml_str(plugin_cfg, "site"),
            api_key: yaml_str(plugin_cfg, "api_key"),
            url_mode: yaml_str(plugin_cfg, "url_mode"),
            url_list: yaml_string_vec(plugin_cfg, "url_list"),
            max_urls: yaml_u32(plugin_cfg, "max_urls"),
            strategies: yaml_string_vec(plugin_cfg, "strategies"),
            categories: yaml_string_vec(plugin_cfg, "categories"),
            locale: yaml_str(plugin_cfg, "locale"),
            max_requests_per_run: yaml_u32(plugin_cfg, "max_requests_per_run"),
            requests_per_minute: yaml_u32(plugin_cfg, "requests_per_minute"),
            respect_robots: yaml_bool(plugin_cfg, "respect_robots"),
            top_audits_per_page: yaml_u32(plugin_cfg, "top_audits_per_page"),
            max_concurrent_requests: yaml_u32(plugin_cfg, "max_concurrent_requests"),
        }),
        "SeoCrawl" => Ok(SourceConfig::SeoCrawl {
            site: yaml_str(plugin_cfg, "site"),
            max_urls: yaml_u32(plugin_cfg, "max_urls"),
            max_depth: yaml_u32(plugin_cfg, "max_depth"),
            crawl_rate_per_second: plugin_cfg
                .get("crawl_rate_per_second")
                .and_then(|v| v.as_f64()),
            respect_robots: yaml_bool(plugin_cfg, "respect_robots"),
            openai_enabled: yaml_bool(plugin_cfg, "openai_enabled"),
            openai_model: yaml_str(plugin_cfg, "openai_model"),
            openai_analyze_blocks: yaml_bool(plugin_cfg, "openai_analyze_blocks"),
            openai_max_blocks_per_page: yaml_u32(plugin_cfg, "openai_max_blocks_per_page"),
            skip_unchanged_content: yaml_bool(plugin_cfg, "skip_unchanged_content"),
            user_agent: yaml_str(plugin_cfg, "user_agent"),
        }),
        "SiteQuality" => Ok(SourceConfig::SiteQuality {
            site: yaml_str(plugin_cfg, "site"),
            url_mode: yaml_str(plugin_cfg, "url_mode"),
            url_list: yaml_string_vec(plugin_cfg, "url_list"),
            max_pages_per_run: yaml_u32(plugin_cfg, "max_pages_per_run"),
            wait_until: yaml_str(plugin_cfg, "wait_until"),
            navigation_timeout_ms: yaml_u32(plugin_cfg, "navigation_timeout_ms"),
            lighthouse_enabled: yaml_bool(plugin_cfg, "lighthouse_enabled"),
            lighthouse_categories: yaml_string_vec(plugin_cfg, "lighthouse_categories"),
            axe_enabled: yaml_bool(plugin_cfg, "axe_enabled"),
            axe_tags: yaml_string_vec(plugin_cfg, "axe_tags"),
            pages_per_minute: yaml_u32(plugin_cfg, "pages_per_minute"),
            worker_node_path: yaml_str(plugin_cfg, "worker_node_path"),
            playwright_executable_path: yaml_str(plugin_cfg, "playwright_executable_path"),
            respect_robots: yaml_bool(plugin_cfg, "respect_robots"),
            skip_heavy_when_unchanged: yaml_bool(plugin_cfg, "skip_heavy_when_unchanged"),
        }),
        "AiCitations" => Ok(SourceConfig::AiCitations {
            site: yaml_str(plugin_cfg, "site"),
            brand_names: yaml_string_vec(plugin_cfg, "brand_names"),
            prompt_list: yaml_tracked_prompt_list(plugin_cfg, "prompt_list"),
            models: yaml_string_vec(plugin_cfg, "models"),
            requests_per_minute: yaml_u32(plugin_cfg, "requests_per_minute"),
            max_prompts_per_run: yaml_u32(plugin_cfg, "max_prompts_per_run"),
            skip_unchanged_responses: yaml_bool(plugin_cfg, "skip_unchanged_responses"),
            openai_base_url: yaml_str(plugin_cfg, "openai_base_url"),
        }),
        "GoogleSerpRanks" => Ok(SourceConfig::GoogleSerpRanks {
            targets: yaml_google_serp_targets(plugin_cfg, "targets"),
            keywords: yaml_string_vec(plugin_cfg, "keywords"),
            country: yaml_str(plugin_cfg, "country"),
            language: yaml_str(plugin_cfg, "language"),
            device: yaml_str(plugin_cfg, "device"),
            max_depth: yaml_u32(plugin_cfg, "max_depth"),
            min_query_interval_ms: yaml_u64(plugin_cfg, "min_query_interval_ms"),
            max_queries_per_run: yaml_u32(plugin_cfg, "max_queries_per_run"),
            stop_after_first_target_match: yaml_bool(plugin_cfg, "stop_after_first_target_match"),
            capture_results: yaml_bool(plugin_cfg, "capture_results"),
            force_refresh_today: yaml_bool(plugin_cfg, "force_refresh_today"),
            navigation_timeout_ms: yaml_u32(plugin_cfg, "navigation_timeout_ms"),
            worker_node_path: yaml_str(plugin_cfg, "worker_node_path"),
            playwright_executable_path: yaml_str(plugin_cfg, "playwright_executable_path"),
            user_agent: yaml_str(plugin_cfg, "user_agent"),
        }),
        "AppleSearchAds" => Ok(SourceConfig::AppleSearchAds {
            org_id: yaml_str(plugin_cfg, "org_id"),
            client_id: yaml_str(plugin_cfg, "client_id"),
            team_id: yaml_str(plugin_cfg, "team_id"),
            key_id: yaml_str(plugin_cfg, "key_id"),
            private_key_path: yaml_str(plugin_cfg, "private_key_path"),
            private_key_pem: yaml_str(plugin_cfg, "private_key_pem"),
            start_date: yaml_str(plugin_cfg, "start_date"),
            end_date: yaml_str(plugin_cfg, "end_date"),
            lookback_days: yaml_u32(plugin_cfg, "lookback_days"),
            stream_profile: yaml_str(plugin_cfg, "stream_profile"),
            processing_lag_days: yaml_u32(plugin_cfg, "processing_lag_days"),
            access_token: yaml_str(plugin_cfg, "access_token"),
            streams: yaml_string_vec(plugin_cfg, "streams"),
            time_zone: yaml_str(plugin_cfg, "time_zone"),
            return_records_with_no_metrics: yaml_bool(plugin_cfg, "return_records_with_no_metrics"),
            max_concurrent_requests: yaml_u32(plugin_cfg, "max_concurrent_requests"),
        }),
        "MetaInstagramAds" => Ok(SourceConfig::MetaInstagramAds {
            ad_account_id: yaml_str(plugin_cfg, "ad_account_id"),
            start_date: yaml_str(plugin_cfg, "start_date"),
            end_date: yaml_str(plugin_cfg, "end_date"),
            lookback_days: yaml_u32(plugin_cfg, "lookback_days"),
            stream_profile: yaml_str(plugin_cfg, "stream_profile"),
            processing_lag_days: yaml_u32(plugin_cfg, "processing_lag_days"),
            api_version: yaml_str(plugin_cfg, "api_version"),
            access_token: yaml_str(plugin_cfg, "access_token"),
            oauth_token_url: yaml_str(plugin_cfg, "oauth_token_url"),
            oauth_client_id: yaml_str(plugin_cfg, "oauth_client_id"),
            oauth_client_secret: yaml_str(plugin_cfg, "oauth_client_secret"),
            oauth_refresh_token: yaml_str(plugin_cfg, "oauth_refresh_token"),
            instagram_filter: yaml_bool(plugin_cfg, "instagram_filter"),
            streams: yaml_string_vec(plugin_cfg, "streams"),
        }),
        other => Err(format!(
            "data_sources.{data_source_name}.{other} is not supported by lineage config translation"
        )),
    }
}

fn warehouse_config_from_data_sink(
    engine_cfg: &serde_yaml::Value,
    data_sink_name: &str,
) -> Result<(WarehouseConfig, Option<SchemaSinkConfig>), String> {
    let sink = selected_data_sink_mapping(engine_cfg, data_sink_name)?;
    let plugin = plugin_mapping_key(sink).ok_or_else(|| {
        format!(
            "data_sinks.{} must contain a runtime plugin config such as Snowflake:",
            data_sink_name
        )
    })?;
    let plugin_cfg = sink
        .get(yaml_key(&plugin))
        .and_then(|value| value.as_mapping())
        .ok_or_else(|| {
            format!(
                "data_sinks.{} did not contain mapping config '{}'",
                data_sink_name, plugin
            )
        })?;
    let schema_sink = schema_sink_config_for_data_sink(engine_cfg, data_sink_name)?;
    let warehouse = match plugin.to_ascii_lowercase().as_str() {
        "athena" => {
            let schema_from_sink = match &schema_sink {
                Some(SchemaSinkConfig::Glue { glue_database_name }) => {
                    Some(glue_database_name.clone())
                }
                None => None,
            };
            WarehouseConfig::Athena {
                workgroup: yaml_str(plugin_cfg, "workgroup")
                    .or_else(|| yaml_str(plugin_cfg, "athena_workgroup_name")),
                region: yaml_str(plugin_cfg, "region"),
                result_s3: yaml_str(plugin_cfg, "result_s3").or_else(|| {
                    result_s3_from_athena_results_bucket(yaml_str(
                        plugin_cfg,
                        "athena_results_s3_bucket",
                    ))
                }),
                schema: yaml_str(plugin_cfg, "schema").or(schema_from_sink),
            }
        }
        "snowflake" => WarehouseConfig::Snowflake {
            account: yaml_str(plugin_cfg, "account"),
            user: yaml_str(plugin_cfg, "user"),
            password: yaml_str(plugin_cfg, "password"),
            private_key_path: yaml_str(plugin_cfg, "private_key_path"),
            stage: yaml_str(plugin_cfg, "stage"),
            staging_uri: yaml_str(plugin_cfg, "staging_uri"),
            staging_storage_integration: yaml_str(plugin_cfg, "staging_storage_integration"),
            staging_azure_sas_token: yaml_str(plugin_cfg, "staging_azure_sas_token"),
            staging_azure_account_key: yaml_str(plugin_cfg, "staging_azure_account_key"),
            staging_gcs_service_account_key_path: yaml_str(
                plugin_cfg,
                "staging_gcs_service_account_key_path",
            ),
            database: yaml_str(plugin_cfg, "database"),
            schema: yaml_str(plugin_cfg, "schema"),
            warehouse: yaml_str(plugin_cfg, "warehouse"),
            role: yaml_str(plugin_cfg, "role"),
        },
        "bigquery" => WarehouseConfig::Bigquery {
            project: yaml_str(plugin_cfg, "project"),
            dataset: yaml_str(plugin_cfg, "dataset"),
            location: yaml_str(plugin_cfg, "location"),
        },
        "postgres" => WarehouseConfig::Postgres {
            database: yaml_str(plugin_cfg, "database"),
            schema: yaml_str(plugin_cfg, "schema"),
        },
        "databricks" => WarehouseConfig::Databricks {
            workspace_url: yaml_str(plugin_cfg, "workspace_url"),
            token: yaml_str(plugin_cfg, "token"),
            warehouse_id: yaml_str(plugin_cfg, "warehouse_id"),
            catalog: yaml_str(plugin_cfg, "catalog"),
            schema: yaml_str(plugin_cfg, "schema"),
        },
        "synapse" => WarehouseConfig::Synapse {
            connection_string: yaml_str(plugin_cfg, "connection_string"),
            schema: yaml_str(plugin_cfg, "schema"),
        },
        "redshift" => WarehouseConfig::Redshift {
            database: yaml_str(plugin_cfg, "database"),
            cluster_identifier: yaml_str(plugin_cfg, "cluster_identifier"),
            workgroup_name: yaml_str(plugin_cfg, "workgroup_name"),
            db_user: yaml_str(plugin_cfg, "db_user"),
            schema: yaml_str(plugin_cfg, "schema"),
            region: yaml_str(plugin_cfg, "region"),
            staging_s3_bucket: yaml_str(plugin_cfg, "staging_s3_bucket"),
            staging_s3_prefix: yaml_str(plugin_cfg, "staging_s3_prefix"),
            iam_role_arn: yaml_str(plugin_cfg, "iam_role_arn"),
        },
        "clickhouse" => WarehouseConfig::Clickhouse {
            url: yaml_str(plugin_cfg, "url"),
            database: yaml_str(plugin_cfg, "database"),
            user: yaml_str(plugin_cfg, "user"),
            password: yaml_str(plugin_cfg, "password"),
        },
        "motherduck" => WarehouseConfig::Motherduck {
            motherduck_token: yaml_str(plugin_cfg, "motherduck_token"),
            database: yaml_str(plugin_cfg, "database"),
            schema: yaml_str(plugin_cfg, "schema"),
        },
        other => {
            return Err(format!(
                "data_sinks.{data_sink_name}.{other} is not supported by `skippr model`"
            ));
        }
    };
    Ok((warehouse, schema_sink))
}

fn dbt_schema_name(project: &str) -> String {
    let schema = project
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string();
    if schema.is_empty() {
        "skippr".to_string()
    } else {
        schema
    }
}

pub(crate) fn react_config_from_pipeline_config(
    value: &serde_yaml::Value,
    pipeline: &str,
) -> Result<ReactConfigFile, String> {
    let data_sink_name = pipeline_data_sink_name(value, pipeline)?;
    let (warehouse, schema_sink) = warehouse_config_from_data_sink(value, &data_sink_name)?;
    let source = pipeline_data_source_name(value, pipeline)?
        .as_deref()
        .map(|source_name| source_config_from_data_source(value, source_name))
        .transpose()?;
    let cfg = SkipprProjectConfig {
        project: pipeline.to_string(),
        warehouse: Some(warehouse),
        source,
        dbt: Some(DbtConfig {
            target_schema: Some(dbt_schema_name(pipeline)),
            silver_suffix: Some("silver".to_string()),
            gold_suffix: Some("gold".to_string()),
        }),
        schema_sink,
        ..Default::default()
    };
    let workspace = yaml_string_at(value, &["skippr", "workspace"]).unwrap_or("dev");
    translate::to_internal(&cfg, Some(workspace))
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

fn bool_json(value: Option<bool>) -> Option<serde_json::Value> {
    value.map(serde_json::Value::Bool)
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

fn google_serp_targets_json(
    target_site: Option<String>,
    target_aliases: Option<Vec<String>>,
) -> Option<serde_json::Value> {
    let site = target_site?;
    Some(serde_json::json!([{
        "site": site,
        "aliases": target_aliases.unwrap_or_default(),
    }]))
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
        SourceKind::GoogleAnalytics {
            property_id,
            start_date,
            end_date,
            lookback_days,
            stream_profile,
            keep_empty_rows,
            processing_lag_days,
            window_in_days,
            access_token,
            oauth_token_url,
            oauth_client_id,
            oauth_client_secret,
            oauth_refresh_token,
            service_account_json_path,
            streams,
        } => (
            "GoogleAnalytics",
            json_object(vec![
                ("property_id", str_json(property_id)),
                ("start_date", str_json(start_date)),
                ("end_date", str_json(end_date)),
                ("lookback_days", u32_json(lookback_days)),
                ("stream_profile", str_json(stream_profile)),
                ("keep_empty_rows", bool_json(keep_empty_rows)),
                ("processing_lag_days", u32_json(processing_lag_days)),
                ("window_in_days", u32_json(window_in_days)),
                ("access_token", str_json(access_token)),
                ("oauth_token_url", str_json(oauth_token_url)),
                ("oauth_client_id", str_json(oauth_client_id)),
                ("oauth_client_secret", str_json(oauth_client_secret)),
                ("oauth_refresh_token", str_json(oauth_refresh_token)),
                (
                    "service_account_json_path",
                    str_json(service_account_json_path),
                ),
                ("streams", strings_json(streams)),
            ]),
        ),
        SourceKind::GoogleSearchConsole {
            site_url,
            start_date,
            end_date,
            lookback_days,
            stream_profile,
            processing_lag_days,
            window_in_days,
            access_token,
            oauth_token_url,
            oauth_client_id,
            oauth_client_secret,
            oauth_refresh_token,
            service_account_json_path,
            streams,
            search_type,
            data_state,
            row_limit,
            url_inspection_enabled,
            url_list,
        } => (
            "GoogleSearchConsole",
            json_object(vec![
                ("site_url", str_json(site_url)),
                ("start_date", str_json(start_date)),
                ("end_date", str_json(end_date)),
                ("lookback_days", u32_json(lookback_days)),
                ("stream_profile", str_json(stream_profile)),
                ("processing_lag_days", u32_json(processing_lag_days)),
                ("window_in_days", u32_json(window_in_days)),
                ("access_token", str_json(access_token)),
                ("oauth_token_url", str_json(oauth_token_url)),
                ("oauth_client_id", str_json(oauth_client_id)),
                ("oauth_client_secret", str_json(oauth_client_secret)),
                ("oauth_refresh_token", str_json(oauth_refresh_token)),
                (
                    "service_account_json_path",
                    str_json(service_account_json_path),
                ),
                ("streams", strings_json(streams)),
                ("search_type", str_json(search_type)),
                ("data_state", str_json(data_state)),
                ("row_limit", u32_json(row_limit)),
                (
                    "url_inspection_enabled",
                    bool_json(url_inspection_enabled),
                ),
                ("url_list", strings_json(url_list)),
            ]),
        ),
        SourceKind::BingWebmasterTools {
            site_url,
            start_date,
            end_date,
            lookback_days,
            stream_profile,
            processing_lag_days,
            window_in_days,
            api_key,
            access_token,
            oauth_token_url,
            oauth_client_id,
            oauth_client_secret,
            oauth_refresh_token,
            streams,
        } => (
            "BingWebmasterTools",
            json_object(vec![
                ("site_url", str_json(site_url)),
                ("start_date", str_json(start_date)),
                ("end_date", str_json(end_date)),
                ("lookback_days", u32_json(lookback_days)),
                ("stream_profile", str_json(stream_profile)),
                ("processing_lag_days", u32_json(processing_lag_days)),
                ("window_in_days", u32_json(window_in_days)),
                ("api_key", str_json(api_key)),
                ("access_token", str_json(access_token)),
                ("oauth_token_url", str_json(oauth_token_url)),
                ("oauth_client_id", str_json(oauth_client_id)),
                ("oauth_client_secret", str_json(oauth_client_secret)),
                ("oauth_refresh_token", str_json(oauth_refresh_token)),
                ("streams", strings_json(streams)),
            ]),
        ),
        SourceKind::GooglePageSpeed {
            site,
            api_key,
            url_mode,
            url_list,
            max_urls,
            strategies,
            categories,
            locale,
            max_requests_per_run,
            requests_per_minute,
            respect_robots,
            top_audits_per_page,
            max_concurrent_requests,
        } => (
            "GooglePageSpeed",
            json_object(vec![
                ("site", str_json(site)),
                ("api_key", str_json(api_key)),
                ("url_mode", str_json(url_mode)),
                ("url_list", strings_json(url_list)),
                ("max_urls", u32_json(max_urls)),
                ("strategies", strings_json(strategies)),
                ("categories", strings_json(categories)),
                ("locale", str_json(locale)),
                ("max_requests_per_run", u32_json(max_requests_per_run)),
                ("requests_per_minute", u32_json(requests_per_minute)),
                ("respect_robots", bool_json(respect_robots)),
                ("top_audits_per_page", u32_json(top_audits_per_page)),
                ("max_concurrent_requests", u32_json(max_concurrent_requests)),
            ]),
        ),
        SourceKind::SeoCrawl {
            site,
            max_urls,
            max_depth,
            crawl_rate_per_second,
            respect_robots,
            openai_enabled,
            openai_model,
            openai_analyze_blocks,
            openai_max_blocks_per_page,
            skip_unchanged_content,
            user_agent,
        } => (
            "SeoCrawl",
            json_object(vec![
                ("site", str_json(site)),
                ("max_urls", u32_json(max_urls)),
                ("max_depth", u32_json(max_depth)),
                (
                    "crawl_rate_per_second",
                    crawl_rate_per_second.map(serde_json::Value::from),
                ),
                ("respect_robots", bool_json(respect_robots)),
                ("openai_enabled", bool_json(openai_enabled)),
                ("openai_model", str_json(openai_model)),
                ("openai_analyze_blocks", bool_json(openai_analyze_blocks)),
                ("openai_max_blocks_per_page", u32_json(openai_max_blocks_per_page)),
                ("skip_unchanged_content", bool_json(skip_unchanged_content)),
                ("user_agent", str_json(user_agent)),
            ]),
        ),
        SourceKind::SiteQuality {
            site,
            url_mode,
            url_list,
            max_pages_per_run,
            wait_until,
            navigation_timeout_ms,
            lighthouse_enabled,
            lighthouse_categories,
            axe_enabled,
            axe_tags,
            pages_per_minute,
            worker_node_path,
            playwright_executable_path,
            respect_robots,
            skip_heavy_when_unchanged,
        } => (
            "SiteQuality",
            json_object(vec![
                ("site", str_json(site)),
                ("url_mode", str_json(url_mode)),
                ("url_list", strings_json(url_list)),
                ("max_pages_per_run", u32_json(max_pages_per_run)),
                ("wait_until", str_json(wait_until)),
                ("navigation_timeout_ms", u32_json(navigation_timeout_ms)),
                ("lighthouse_enabled", bool_json(lighthouse_enabled)),
                ("lighthouse_categories", strings_json(lighthouse_categories)),
                ("axe_enabled", bool_json(axe_enabled)),
                ("axe_tags", strings_json(axe_tags)),
                ("pages_per_minute", u32_json(pages_per_minute)),
                ("worker_node_path", str_json(worker_node_path)),
                (
                    "playwright_executable_path",
                    str_json(playwright_executable_path),
                ),
                ("respect_robots", bool_json(respect_robots)),
                (
                    "skip_heavy_when_unchanged",
                    bool_json(skip_heavy_when_unchanged),
                ),
            ]),
        ),
        SourceKind::AiCitations {
            site,
            brand_names,
            models,
            requests_per_minute,
            max_prompts_per_run,
            skip_unchanged_responses,
            openai_base_url,
        } => (
            "AiCitations",
            json_object(vec![
                ("site", str_json(site)),
                ("brand_names", strings_json(brand_names)),
                ("models", strings_json(models)),
                ("requests_per_minute", u32_json(requests_per_minute)),
                ("max_prompts_per_run", u32_json(max_prompts_per_run)),
                (
                    "skip_unchanged_responses",
                    bool_json(skip_unchanged_responses),
                ),
                ("openai_base_url", str_json(openai_base_url)),
            ]),
        ),
        SourceKind::GoogleSerpRanks {
            target_site,
            target_aliases,
            keywords,
            country,
            language,
            device,
            max_depth,
            min_query_interval_ms,
            max_queries_per_run,
            stop_after_first_target_match,
            capture_results,
            force_refresh_today,
            navigation_timeout_ms,
            worker_node_path,
            playwright_executable_path,
            user_agent,
        } => (
            "GoogleSerpRanks",
            json_object(vec![
                (
                    "targets",
                    google_serp_targets_json(target_site, target_aliases),
                ),
                ("keywords", strings_json(keywords)),
                ("country", str_json(country)),
                ("language", str_json(language)),
                ("device", str_json(device)),
                ("max_depth", u32_json(max_depth)),
                ("min_query_interval_ms", u64_json(min_query_interval_ms)),
                ("max_queries_per_run", u32_json(max_queries_per_run)),
                (
                    "stop_after_first_target_match",
                    bool_json(stop_after_first_target_match),
                ),
                ("capture_results", bool_json(capture_results)),
                ("force_refresh_today", bool_json(force_refresh_today)),
                ("navigation_timeout_ms", u32_json(navigation_timeout_ms)),
                ("worker_node_path", str_json(worker_node_path)),
                (
                    "playwright_executable_path",
                    str_json(playwright_executable_path),
                ),
                ("user_agent", str_json(user_agent)),
            ]),
        ),
        SourceKind::AppleSearchAds {
            org_id,
            client_id,
            team_id,
            key_id,
            private_key_path,
            private_key_pem,
            start_date,
            end_date,
            lookback_days,
            stream_profile,
            processing_lag_days,
            time_zone,
            return_records_with_no_metrics,
            access_token,
            streams,
            max_concurrent_requests,
        } => (
            "AppleSearchAds",
            json_object(vec![
                ("org_id", str_json(org_id)),
                ("client_id", str_json(client_id)),
                ("team_id", str_json(team_id)),
                ("key_id", str_json(key_id)),
                ("private_key_path", str_json(private_key_path)),
                ("private_key_pem", str_json(private_key_pem)),
                ("start_date", str_json(start_date)),
                ("end_date", str_json(end_date)),
                ("lookback_days", u32_json(lookback_days)),
                ("stream_profile", str_json(stream_profile)),
                ("processing_lag_days", u32_json(processing_lag_days)),
                ("time_zone", str_json(time_zone)),
                (
                    "return_records_with_no_metrics",
                    bool_json(return_records_with_no_metrics),
                ),
                ("access_token", str_json(access_token)),
                ("streams", strings_json(streams)),
                ("max_concurrent_requests", u32_json(max_concurrent_requests)),
            ]),
        ),
        SourceKind::MetaInstagramAds {
            ad_account_id,
            start_date,
            end_date,
            lookback_days,
            stream_profile,
            processing_lag_days,
            api_version,
            access_token,
            oauth_token_url,
            oauth_client_id,
            oauth_client_secret,
            oauth_refresh_token,
            instagram_filter,
            streams,
        } => (
            "MetaInstagramAds",
            json_object(vec![
                ("ad_account_id", str_json(ad_account_id)),
                ("start_date", str_json(start_date)),
                ("end_date", str_json(end_date)),
                ("lookback_days", u32_json(lookback_days)),
                ("stream_profile", str_json(stream_profile)),
                ("processing_lag_days", u32_json(processing_lag_days)),
                ("api_version", str_json(api_version)),
                ("access_token", str_json(access_token)),
                ("oauth_token_url", str_json(oauth_token_url)),
                ("oauth_client_id", str_json(oauth_client_id)),
                ("oauth_client_secret", str_json(oauth_client_secret)),
                ("oauth_refresh_token", str_json(oauth_refresh_token)),
                ("instagram_filter", bool_json(instagram_filter)),
                ("streams", strings_json(streams)),
            ]),
        ),
        SourceKind::DataForSeoBacklinks {
            login,
            password,
            site,
            run_mode,
            backlink_target,
            limit,
            max_pages,
            request_interval_ms,
        } => {
            let mut backlink_jobs = Vec::new();
            if let Some(target) = backlink_target.filter(|t| !t.trim().is_empty()) {
                backlink_jobs.push(serde_json::json!({
                    "target": target,
                    "limit": limit.unwrap_or(1000),
                    "max_pages": max_pages.unwrap_or(5),
                }));
            }
            (
                "DataForSeoBacklinks",
                json_object(vec![
                    ("login", str_json(login)),
                    ("password", str_json(password)),
                    ("site", str_json(site)),
                    (
                        "run_mode",
                        str_json(run_mode.or_else(|| Some("both".to_string()))),
                    ),
                    (
                        "backlink_jobs",
                        Some(serde_json::Value::Array(backlink_jobs)),
                    ),
                    (
                        "intersection_jobs",
                        Some(serde_json::json!([])),
                    ),
                    ("request_interval_ms", u64_json(request_interval_ms)),
                ]),
            )
        }
        SourceKind::DataForSeoSeoOpportunities {
            login,
            password,
            site,
            location_code,
            language_code,
            device,
            run_mode,
            seed_keywords,
            request_interval_ms,
        } => (
            "DataForSeoSeoOpportunities",
            json_object(vec![
                ("login", str_json(login)),
                ("password", str_json(password)),
                ("site", str_json(site)),
                ("location_code", u32_json(location_code.or(Some(2840)))),
                ("language_code", str_json(language_code.or_else(|| Some("en".to_string())))),
                ("device", str_json(device.or_else(|| Some("desktop".to_string())))),
                ("run_mode", str_json(run_mode.or_else(|| Some("mvp".to_string())))),
                ("seed_keywords", strings_json(seed_keywords)),
                ("request_interval_ms", u64_json(request_interval_ms)),
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

async fn load_cli_auth_credentials() -> Result<(auth::StoredCredentials, bool), String> {
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
            "Authentication required. Run 'skippr user login' or set SKIPPR_API_KEY.".to_string(),
        );
    };
    Ok((creds, authenticated_with_api_key))
}

async fn load_reset_server_credentials() -> Result<api_client::CredentialsResponse, String> {
    let (creds, authenticated_with_api_key) = load_cli_auth_credentials().await.map_err(|err| {
        err.replace(
            "Authentication required",
            "Authentication required to reset cloud project data",
        )
    })?;

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

async fn load_cli_server_credentials() -> Result<api_client::CredentialsResponse, String> {
    load_reset_server_credentials().await.map_err(|err| {
        err.replace(
            "Authentication required to reset cloud project data",
            "Authentication required to run this command",
        )
    })
}

fn authenticated_storage_bucket_and_credentials(
    srv_creds: &api_client::CredentialsResponse,
) -> Result<(String, react_core::resolved_config::S3Credentials), String> {
    let bucket = srv_creds.bucket.trim().to_string();
    if bucket.is_empty() {
        return Err("missing storage.bucket for reset".to_string());
    }

    let s3_creds = translate::s3_credentials_from_auth(srv_creds);
    Ok((bucket, s3_creds))
}

async fn react_s3_storage_from_credentials(
    bucket: &str,
    s3_creds: &react_core::resolved_config::S3Credentials,
) -> Arc<dyn react_core::storage::StorageAdapter> {
    Arc::new(
        react_module_storage_s3::S3StorageAdapter::from_credentials(
            bucket.to_string(),
            &s3_creds.access_key_id,
            &s3_creds.secret_access_key,
            s3_creds.session_token.as_deref(),
            &s3_creds.region,
        )
        .await,
    ) as Arc<dyn react_core::storage::StorageAdapter>
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

async fn cmd_init(name: &str, explicit_config: &Option<PathBuf>, output: &str) {
    let path = config_path(explicit_config);
    let project_root = project_root_from_config_path(&path);

    if path.exists() {
        if is_json_output(output) {
            print_json(&InitResult {
                ok: true,
                config_path: path.display().to_string(),
                project: name.to_string(),
                created_files: Vec::new(),
                next_steps: vec![
                    "skippr connect warehouse snowflake".to_string(),
                    "skippr connect source mssql".to_string(),
                    "skippr doctor".to_string(),
                ],
            });
        } else {
            println!("Project already initialised — {}", path.display());
        }
        return;
    }

    let raw = format!(
        r#"skippr:
  workspace: {name}

pipelines:
  {name}:
    data_source: data_sources.source
    data_sink: data_sinks.warehouse

data_sources: {{}}
data_sinks: {{}}
schema_sinks: {{}}
"#
    );
    let cfg: serde_yaml::Value = serde_yaml::from_str(&raw).expect("valid default skippr.yaml");
    if let Err(e) = save_engine_config(explicit_config, &cfg) {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }

    if !is_json_output(output) {
        eprintln!("[skippr] creating local environment...");
    }
    if let Err(e) = ensure_local_environment(&project_root) {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }
    if !is_json_output(output) {
        eprintln!("[skippr] environment created.");
    }

    let created_files = vec![
        path.display().to_string(),
        skippr_dir_from_project_root(&project_root)
            .display()
            .to_string(),
        env_example_path_from_project_root(&project_root)
            .display()
            .to_string(),
    ];
    let next_steps = vec![
        "skippr connect warehouse snowflake".to_string(),
        "skippr connect source mssql".to_string(),
        "skippr doctor".to_string(),
        format!("skippr discover --pipeline {name}"),
        format!("skippr sync --pipeline {name} --once"),
        format!("skippr model --pipeline {name}"),
    ];
    if is_json_output(output) {
        print_json(&InitResult {
            ok: true,
            config_path: path.display().to_string(),
            project: name.to_string(),
            created_files,
            next_steps,
        });
    } else {
        println!("Initialised project '{}' — {}", name, path.display());
        println!();
        println!("Next steps:");
        for step in next_steps {
            println!("  {step}");
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PipelineResetTarget {
    pipeline: String,
    skipprd_tenant: String,
    model_tenant: String,
    workspace: String,
    local_runtime_dir: PathBuf,
    local_model_dirs: Vec<PathBuf>,
    skipprd_bucket: Option<String>,
    skipprd_prefix: String,
    model_bucket: String,
    model_prefix: String,
}

#[derive(Debug, Default)]
struct PipelineResetReport {
    deleted_local: Vec<PathBuf>,
    deleted_skipprd_remote: Vec<String>,
    deleted_model_remote: Vec<String>,
}

pub(crate) fn yaml_string_at<'a>(value: &'a serde_yaml::Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = value;
    for key in path {
        cur = cur.get(*key)?;
    }
    cur.as_str().map(str::trim).filter(|v| !v.is_empty())
}

fn pipeline_config<'a>(
    engine_cfg: &'a serde_yaml::Value,
    pipeline: &str,
) -> Result<&'a serde_yaml::Value, String> {
    validate_pipeline_exists(engine_cfg, pipeline)?;
    engine_cfg
        .get("pipelines")
        .and_then(|pipelines| pipelines.get(pipeline))
        .ok_or_else(|| format!("skippr.yml does not define pipeline '{pipeline}'"))
}

fn configured_skippr_s3_bucket(engine_cfg: &serde_yaml::Value) -> Option<String> {
    yaml_string_at(engine_cfg, &["skippr", "skippr_s3_bucket"])
        .map(ToOwned::to_owned)
        .or_else(|| {
            std::env::var("SKIPPR_S3_BUCKET")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        })
}

fn cli_pipeline_data_dir(
    project_root: &std::path::Path,
    tenant: &str,
    pipeline: &str,
    workspace: &str,
    pipeline_cfg: &serde_yaml::Value,
) -> PathBuf {
    let has_custom_data_dir = pipeline_cfg
        .get("data_dir")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .is_some_and(|value| !value.is_empty());
    if has_custom_data_dir {
        data_root_for_pipeline(project_root, pipeline_cfg).join(format!("{workspace}_{pipeline}"))
    } else {
        skippr_dir_from_project_root(project_root)
            .join(tenant)
            .join(pipeline)
    }
}

fn data_root_for_pipeline(
    project_root: &std::path::Path,
    pipeline_cfg: &serde_yaml::Value,
) -> PathBuf {
    let configured = pipeline_cfg
        .get("data_dir")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            std::env::var("DATA_DIR")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        })
        .unwrap_or_else(|| "./data".to_string());
    let root = PathBuf::from(configured.trim_end_matches('/'));
    if root.is_absolute() {
        root
    } else {
        project_root.join(root)
    }
}

fn dedupe_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for path in paths {
        if !out.iter().any(|existing| existing == &path) {
            out.push(path);
        }
    }
    out
}

fn derive_pipeline_reset_target(
    engine_cfg: &serde_yaml::Value,
    config_path: &std::path::Path,
    pipeline: &str,
    srv_creds: &api_client::CredentialsResponse,
) -> Result<PipelineResetTarget, String> {
    let pipeline = pipeline.trim();
    if pipeline.is_empty() {
        return Err("pipeline cannot be empty".to_string());
    }
    let pipeline_cfg = pipeline_config(engine_cfg, pipeline)?;
    let project_root = project_root_from_config_path(config_path);
    let model_tenant = srv_creds.tenant_id.trim();
    if model_tenant.is_empty() {
        return Err("missing tenant_id for reset".to_string());
    }
    let workspace = yaml_string_at(engine_cfg, &["skippr", "workspace"])
        .unwrap_or("default")
        .to_string();
    let skipprd_tenant = model_tenant.to_string();
    let model_bucket = srv_creds.bucket.trim();
    if model_bucket.is_empty() {
        return Err("missing storage.bucket for reset".to_string());
    }

    let local_runtime_dir = cli_pipeline_data_dir(
        &project_root,
        &skipprd_tenant,
        pipeline,
        &workspace,
        pipeline_cfg,
    );
    let skippr_dir = skippr_dir_from_project_root(&project_root);
    let local_model_dirs = dedupe_paths(vec![
        skippr_dir.join(format!("_/dev/{pipeline}")),
        skippr_dir.join(format!("{model_tenant}/dev/{pipeline}")),
    ]);

    let scope = react_core::scope::RequestScope::parse(model_tenant, "dev", pipeline)
        .map_err(|e| format!("invalid reset scope: {e}"))?;
    let keyspace = react_core::keyspace::DefaultKeyspace::new(model_bucket.to_string());
    let model_prefix = keyspace.scoped_prefix(&scope, &[]);

    Ok(PipelineResetTarget {
        pipeline: pipeline.to_string(),
        skipprd_tenant: skipprd_tenant.clone(),
        model_tenant: model_tenant.to_string(),
        workspace: workspace.clone(),
        local_runtime_dir,
        local_model_dirs,
        skipprd_bucket: configured_skippr_s3_bucket(engine_cfg),
        skipprd_prefix: format!("{skipprd_tenant}/{workspace}/{pipeline}/"),
        model_bucket: model_bucket.to_string(),
        model_prefix,
    })
}

fn safe_remove_dir_all(path: &std::path::Path) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }
    if path.as_os_str().is_empty() || path.parent().is_none() || path == std::path::Path::new("/") {
        return Err(format!("refusing to remove unsafe path {}", path.display()));
    }
    std::fs::remove_dir_all(path)
        .map_err(|e| format!("failed to remove {}: {}", path.display(), e))?;
    Ok(true)
}

fn confirm_pipeline_reset(target: &PipelineResetTarget) -> Result<(), String> {
    eprintln!(
        "WARNING: This will delete Skippr-owned state for pipeline '{}'",
        target.pipeline
    );
    eprintln!("  local runtime: {}", target.local_runtime_dir.display());
    for path in &target.local_model_dirs {
        eprintln!("  local model: {}", path.display());
    }
    if let Some(bucket) = &target.skipprd_bucket {
        eprintln!(
            "  skipprd remote: s3://{}/{}",
            bucket, target.skipprd_prefix
        );
    } else {
        eprintln!("  skipprd remote: <none; no skippr_s3_bucket configured>");
    }
    eprintln!(
        "  model remote: s3://{}/{}",
        target.model_bucket, target.model_prefix
    );
    eprintln!("This does not delete source, sink, schema sink, or warehouse table data.");
    eprintln!();
    eprint!("Type 'yes' to confirm: ");
    let _ = std::io::Write::flush(&mut std::io::stderr());
    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .map_err(|e| format!("failed to read confirmation: {e}"))?;
    if input.trim() != "yes" {
        return Err("Aborted.".to_string());
    }
    Ok(())
}

async fn delete_pipeline_reset_target_with_storage(
    target: &PipelineResetTarget,
    skipprd_storage: Option<&Arc<dyn skipprd::adapters::storage::StorageAdapter>>,
    model_storage: &Arc<dyn react_core::storage::StorageAdapter>,
) -> Result<PipelineResetReport, String> {
    let mut report = PipelineResetReport::default();

    if safe_remove_dir_all(&target.local_runtime_dir)? {
        report.deleted_local.push(target.local_runtime_dir.clone());
    }
    for path in &target.local_model_dirs {
        if safe_remove_dir_all(path)? {
            report.deleted_local.push(path.clone());
        }
    }

    if let (Some(_bucket), Some(storage)) = (&target.skipprd_bucket, skipprd_storage) {
        report.deleted_skipprd_remote =
            delete_skipprd_storage_prefix(storage, &target.skipprd_prefix).await?;
    }
    report.deleted_model_remote =
        delete_storage_prefix(model_storage, &target.model_prefix).await?;

    Ok(report)
}

async fn cmd_reset(explicit_config: &Option<PathBuf>, args: ResetArgs) {
    let path = config_path(explicit_config);
    if !path.exists() {
        eprintln!("error: {} not found", path.display());
        eprintln!("Run 'skippr init <project>' first.");
        std::process::exit(1);
    }
    let engine_cfg = match load_resolved_engine_config(explicit_config) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };
    let srv_creds = match load_reset_server_credentials().await {
        Ok(creds) => creds,
        Err(e) => {
            eprintln!("[skippr] ERROR: {e}");
            std::process::exit(1);
        }
    };
    let target = match derive_pipeline_reset_target(&engine_cfg, &path, &args.pipeline, &srv_creds)
    {
        Ok(target) => target,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    if !args.yes {
        if let Err(e) = confirm_pipeline_reset(&target) {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }

    let (_auth_bucket, s3_creds) = match authenticated_storage_bucket_and_credentials(&srv_creds) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[skippr] ERROR: {e}");
            std::process::exit(1);
        }
    };
    let model_storage = react_s3_storage_from_credentials(&target.model_bucket, &s3_creds).await;
    let skipprd_storage = match target.skipprd_bucket.as_deref() {
        Some(bucket) => {
            skipprd::helpers::configuration::Config::setenv("SKIPPR_S3_BUCKET", bucket);
            Some(Arc::new(skipprd::adapters::storage::S3StorageAdapter)
                as Arc<dyn skipprd::adapters::storage::StorageAdapter>)
        }
        None => None,
    };

    match delete_pipeline_reset_target_with_storage(
        &target,
        skipprd_storage.as_ref(),
        &model_storage,
    )
    .await
    {
        Ok(report) => {
            for path in &report.deleted_local {
                println!("Removed {}", path.display());
            }
            if let Some(bucket) = &target.skipprd_bucket {
                println!(
                    "Deleted {} skipprd objects from s3://{}/{}",
                    report.deleted_skipprd_remote.len(),
                    bucket,
                    target.skipprd_prefix
                );
            }
            println!(
                "Deleted {} model objects from s3://{}/{}",
                report.deleted_model_remote.len(),
                target.model_bucket,
                target.model_prefix
            );
            println!("Reset pipeline '{}'.", target.pipeline);
        }
        Err(e) => {
            eprintln!("[skippr] ERROR: {e}");
            std::process::exit(1);
        }
    }
}

// ---------------------------------------------------------------------------
// connect warehouse
// ---------------------------------------------------------------------------

fn emit_connect_result(
    output: &str,
    explicit_config: &Option<PathBuf>,
    connector_kind: &str,
    section_updated: &str,
) {
    if is_json_output(output) {
        print_json(&ConnectResult {
            ok: true,
            config_path: config_path(explicit_config).display().to_string(),
            connector_kind: connector_kind.to_string(),
            section_updated: section_updated.to_string(),
            pipeline_references_updated: true,
            missing_recommended_fields: Vec::new(),
        });
    }
}

fn cmd_connect_warehouse(kind: WarehouseKind, explicit_config: &Option<PathBuf>, output: &str) {
    match load_cli_raw_config_for_save(explicit_config) {
        Ok(mut cfg) => {
            let (plugin, config) = warehouse_plugin_and_config(kind);
            set_plugin_section(&mut cfg, "data_sinks", "warehouse", plugin, config);
            set_primary_pipeline_refs(&mut cfg, None, Some("warehouse"));
            if let Err(e) = save_engine_config(explicit_config, &cfg) {
                eprintln!("error: {}", e);
                std::process::exit(1);
            }
            if is_json_output(output) {
                emit_connect_result(output, explicit_config, plugin, "data_sinks.warehouse");
            } else {
                println!("Configured warehouse data sink 'warehouse' ({plugin}) in skippr.yml");
            }
            return;
        }
        Err(e) if config_path(explicit_config).exists() => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
        Err(_) => {}
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

    if is_json_output(output) {
        emit_connect_result(output, explicit_config, kind_label, "warehouse");
    } else {
        println!("Warehouse ({}) configured.", kind_label);
    }
}

// ---------------------------------------------------------------------------
// connect source
// ---------------------------------------------------------------------------

fn cmd_connect_source(mut kind: SourceKind, explicit_config: &Option<PathBuf>, output: &str) {
    if let SourceKind::GoogleAnalytics {
        ref mut property_id,
        ref mut start_date,
        ref mut access_token,
        ..
    } = &mut kind
    {
        if property_id.is_none() {
            *property_id = prompt("GA4 property ID (numeric, e.g. 123456789)");
        }
        if start_date.is_none() {
            *start_date = prompt("Start date for first sync (YYYY-MM-DD)");
        }
        if access_token.is_none() {
            *access_token = Some("${GA4_ACCESS_TOKEN}".to_string());
        }
    }

    if let SourceKind::GoogleSearchConsole {
        ref mut site_url,
        ref mut start_date,
        ref mut access_token,
        ref mut oauth_client_id,
        ref mut oauth_client_secret,
        ref mut oauth_refresh_token,
        ..
    } = &mut kind
    {
        if site_url.is_none() {
            *site_url = prompt(
                "Search Console site URL (https://example.com/ or sc-domain:example.com)",
            );
        }
        if start_date.is_none() {
            *start_date = prompt("Start date for first sync (YYYY-MM-DD)");
        }
        if access_token.is_none() {
            *access_token = Some("${GSC_ACCESS_TOKEN}".to_string());
        }
        if oauth_client_id.is_none() {
            *oauth_client_id = Some("${GSC_OAUTH_CLIENT_ID}".to_string());
        }
        if oauth_client_secret.is_none() {
            *oauth_client_secret = Some("${GSC_OAUTH_CLIENT_SECRET}".to_string());
        }
        if oauth_refresh_token.is_none() {
            *oauth_refresh_token = Some("${GSC_OAUTH_REFRESH_TOKEN}".to_string());
        }
    }

    if let SourceKind::BingWebmasterTools {
        ref mut site_url,
        ref mut start_date,
        ref mut api_key,
        ref mut oauth_client_id,
        ref mut oauth_client_secret,
        ref mut oauth_refresh_token,
        ..
    } = &mut kind
    {
        if site_url.is_none() {
            *site_url = prompt("Bing Webmaster site URL (https://example.com/)");
        }
        if start_date.is_none() {
            *start_date = prompt("Start date for first sync (YYYY-MM-DD)");
        }
        if api_key.is_none() {
            *api_key = Some("${BING_WEBMASTER_TOOLS_API_KEY}".to_string());
        }
        if oauth_client_id.is_none() {
            *oauth_client_id = Some("${BING_WEBMASTER_OAUTH_CLIENT_ID}".to_string());
        }
        if oauth_client_secret.is_none() {
            *oauth_client_secret = Some("${BING_WEBMASTER_OAUTH_CLIENT_SECRET}".to_string());
        }
        if oauth_refresh_token.is_none() {
            *oauth_refresh_token = Some("${BING_WEBMASTER_OAUTH_REFRESH_TOKEN}".to_string());
        }
    }

    if let SourceKind::GooglePageSpeed {
        ref mut site,
        ref mut api_key,
        ref mut url_mode,
        ref mut strategies,
        ..
    } = &mut kind
    {
        if site.is_none() {
            *site = prompt("Site URL to sample (e.g. https://example.com)");
        }
        if api_key.is_none() {
            *api_key = Some("${PAGESPEED_API_KEY}".to_string());
        }
        if url_mode.is_none() {
            *url_mode = Some("tld_sample".to_string());
        }
        if strategies.is_none() {
            *strategies = Some(vec!["mobile".into(), "desktop".into()]);
        }
    }

    if let SourceKind::SiteQuality {
        ref mut site,
        ref mut url_mode,
        ..
    } = &mut kind
    {
        if site.is_none() {
            *site = prompt("Site URL to measure (e.g. https://example.com)");
        }
        if url_mode.is_none() {
            *url_mode = Some("tld_sample".to_string());
        }
    }

    if let SourceKind::GoogleSerpRanks {
        ref mut target_site,
        ref mut keywords,
        ref mut country,
        ref mut language,
        ..
    } = &mut kind
    {
        if target_site.is_none() {
            *target_site = prompt("Target site domain to track (e.g. example.com)");
        }
        if keywords.as_ref().is_none_or(|k| k.is_empty()) {
            let raw = prompt("Keywords to track (comma-separated)");
            *keywords = raw.map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                    .map(str::to_string)
                    .collect()
            });
        }
        if country.is_none() {
            *country = Some("uk".to_string());
        }
        if language.is_none() {
            *language = Some("en".to_string());
        }
    }

    if let SourceKind::AiCitations {
        ref mut site,
        ref mut models,
        ..
    } = &mut kind
    {
        if site.is_none() {
            *site = prompt("Target site URL (e.g. https://example.com)");
        }
        if models.is_none() {
            *models = Some(vec!["gpt-4.1-mini".to_string()]);
        }
    }

    if let SourceKind::AppleSearchAds {
        ref mut org_id,
        ref mut client_id,
        ref mut team_id,
        ref mut key_id,
        ref mut private_key_path,
        ref mut start_date,
        ..
    } = &mut kind
    {
        if org_id.is_none() {
            *org_id = prompt("Apple Search Ads org ID");
        }
        if client_id.is_none() {
            *client_id = Some("${APPLE_SEARCH_ADS_CLIENT_ID}".to_string());
        }
        if team_id.is_none() {
            *team_id = Some("${APPLE_SEARCH_ADS_TEAM_ID}".to_string());
        }
        if key_id.is_none() {
            *key_id = Some("${APPLE_SEARCH_ADS_KEY_ID}".to_string());
        }
        if private_key_path.is_none() {
            *private_key_path = Some("${APPLE_SEARCH_ADS_PRIVATE_KEY_PATH}".to_string());
        }
        if start_date.is_none() {
            *start_date = prompt("Start date for first sync (YYYY-MM-DD)");
        }
    }

    if let SourceKind::MetaInstagramAds {
        ref mut ad_account_id,
        ref mut start_date,
        ref mut access_token,
        ..
    } = &mut kind
    {
        if ad_account_id.is_none() {
            *ad_account_id = prompt("Meta ad account ID (numeric, without act_ prefix)");
        }
        if start_date.is_none() {
            *start_date = prompt("Start date for first sync (YYYY-MM-DD)");
        }
        if access_token.is_none() {
            *access_token = Some("${META_INSTAGRAM_ADS_ACCESS_TOKEN}".to_string());
        }
    }

    if let SourceKind::DataForSeoBacklinks {
        ref mut login,
        ref mut password,
        ref mut site,
        ref mut run_mode,
        ref mut backlink_target,
        ..
    } = &mut kind
    {
        if login.is_none() {
            *login = Some("${DATAFORSEO_API_USER}".to_string());
        }
        if password.is_none() {
            *password = Some("${DATAFORSEO_API_PASS}".to_string());
        }
        if site.is_none() {
            if let Some(target) = backlink_target.as_deref().filter(|t| !t.trim().is_empty()) {
                *site = Some(target.to_string());
            } else {
                *site = prompt("Site label for bronze rows (e.g. example.com)");
            }
        }
        if run_mode.is_none() {
            *run_mode = Some("both".to_string());
        }
        if backlink_target.is_none() {
            *backlink_target = prompt("Primary backlink target domain (e.g. example.com)");
        }
    }

    if let SourceKind::DataForSeoSeoOpportunities {
        ref mut login,
        ref mut password,
        ref mut site,
        ref mut seed_keywords,
        ..
    } = &mut kind
    {
        if login.is_none() {
            *login = Some("${DATAFORSEO_API_USER}".to_string());
        }
        if password.is_none() {
            *password = Some("${DATAFORSEO_API_PASS}".to_string());
        }
        if site.is_none() {
            *site = prompt("Site domain for bronze rows (e.g. example.com)");
        }
        if seed_keywords.is_none() {
            if let Some(seed) = prompt(
                "Seed keyword for opportunity analysis (e.g. meal planning app)",
            ) {
                *seed_keywords = Some(vec![seed]);
            }
        }
    }

    match load_cli_raw_config_for_save(explicit_config) {
        Ok(mut cfg) => {
            let (plugin, config) = source_plugin_and_config(kind);
            set_plugin_section(&mut cfg, "data_sources", "source", plugin, config);
            set_primary_pipeline_refs(&mut cfg, Some("source"), None);
            if let Err(e) = save_engine_config(explicit_config, &cfg) {
                eprintln!("error: {}", e);
                std::process::exit(1);
            }
            if is_json_output(output) {
                emit_connect_result(output, explicit_config, plugin, "data_sources.source");
            } else {
                println!("Configured data source 'source' ({plugin}) in skippr.yml");
            }
            return;
        }
        Err(e) if config_path(explicit_config).exists() => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
        Err(_) => {}
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
        SourceKind::GoogleAnalytics {
            property_id,
            start_date,
            end_date,
            lookback_days,
            stream_profile,
            keep_empty_rows,
            processing_lag_days,
            window_in_days,
            access_token,
            oauth_token_url,
            oauth_client_id,
            oauth_client_secret,
            oauth_refresh_token,
            service_account_json_path,
            streams,
        } => SourceConfig::GoogleAnalytics {
            property_id,
            start_date,
            end_date,
            lookback_days,
            stream_profile,
            keep_empty_rows,
            processing_lag_days,
            window_in_days,
            access_token,
            oauth_token_url,
            oauth_client_id,
            oauth_client_secret,
            oauth_refresh_token,
            service_account_json_path,
            streams,
        },
        SourceKind::GoogleSearchConsole {
            site_url,
            start_date,
            end_date,
            lookback_days,
            stream_profile,
            processing_lag_days,
            window_in_days,
            access_token,
            oauth_token_url,
            oauth_client_id,
            oauth_client_secret,
            oauth_refresh_token,
            service_account_json_path,
            streams,
            search_type,
            data_state,
            row_limit,
            url_inspection_enabled,
            url_list,
        } => SourceConfig::GoogleSearchConsole {
            site_url,
            start_date,
            end_date,
            lookback_days,
            stream_profile,
            processing_lag_days,
            window_in_days,
            access_token,
            oauth_token_url,
            oauth_client_id,
            oauth_client_secret,
            oauth_refresh_token,
            service_account_json_path,
            streams,
            search_type,
            data_state,
            row_limit,
            url_inspection_enabled,
            url_list,
        },
        SourceKind::BingWebmasterTools {
            site_url,
            start_date,
            end_date,
            lookback_days,
            stream_profile,
            processing_lag_days,
            window_in_days,
            api_key,
            access_token,
            oauth_token_url,
            oauth_client_id,
            oauth_client_secret,
            oauth_refresh_token,
            streams,
        } => SourceConfig::BingWebmasterTools {
            site_url,
            api_key,
            start_date,
            end_date,
            lookback_days,
            stream_profile,
            processing_lag_days,
            window_in_days,
            access_token,
            oauth_token_url,
            oauth_client_id,
            oauth_client_secret,
            oauth_refresh_token,
            streams,
        },
        SourceKind::GooglePageSpeed {
            site,
            api_key,
            url_mode,
            url_list,
            max_urls,
            strategies,
            categories,
            locale,
            max_requests_per_run,
            requests_per_minute,
            respect_robots,
            top_audits_per_page,
            max_concurrent_requests,
        } => SourceConfig::GooglePageSpeed {
            site,
            api_key,
            url_mode,
            url_list,
            max_urls,
            strategies,
            categories,
            locale,
            max_requests_per_run,
            requests_per_minute,
            respect_robots,
            top_audits_per_page,
            max_concurrent_requests,
        },
        SourceKind::SeoCrawl {
            site,
            max_urls,
            max_depth,
            crawl_rate_per_second,
            respect_robots,
            openai_enabled,
            openai_model,
            openai_analyze_blocks,
            openai_max_blocks_per_page,
            skip_unchanged_content,
            user_agent,
        } => SourceConfig::SeoCrawl {
            site,
            max_urls,
            max_depth,
            crawl_rate_per_second,
            respect_robots,
            openai_enabled,
            openai_model,
            openai_analyze_blocks,
            openai_max_blocks_per_page,
            skip_unchanged_content,
            user_agent,
        },
        SourceKind::SiteQuality {
            site,
            url_mode,
            url_list,
            max_pages_per_run,
            wait_until,
            navigation_timeout_ms,
            lighthouse_enabled,
            lighthouse_categories,
            axe_enabled,
            axe_tags,
            pages_per_minute,
            worker_node_path,
            playwright_executable_path,
            respect_robots,
            skip_heavy_when_unchanged,
        } => SourceConfig::SiteQuality {
            site,
            url_mode,
            url_list,
            max_pages_per_run,
            wait_until,
            navigation_timeout_ms,
            lighthouse_enabled,
            lighthouse_categories,
            axe_enabled,
            axe_tags,
            pages_per_minute,
            worker_node_path,
            playwright_executable_path,
            respect_robots,
            skip_heavy_when_unchanged,
        },
        SourceKind::AiCitations {
            site,
            brand_names,
            models,
            requests_per_minute,
            max_prompts_per_run,
            skip_unchanged_responses,
            openai_base_url,
        } => SourceConfig::AiCitations {
            site,
            brand_names,
            prompt_list: None,
            models,
            requests_per_minute,
            max_prompts_per_run,
            skip_unchanged_responses,
            openai_base_url,
        },
        SourceKind::GoogleSerpRanks {
            target_site,
            target_aliases,
            keywords,
            country,
            language,
            device,
            max_depth,
            min_query_interval_ms,
            max_queries_per_run,
            stop_after_first_target_match,
            capture_results,
            force_refresh_today,
            navigation_timeout_ms,
            worker_node_path,
            playwright_executable_path,
            user_agent,
        } => SourceConfig::GoogleSerpRanks {
            targets: target_site.map(|site| {
                vec![crate::public_config::GoogleSerpTargetConfig {
                    site,
                    aliases: target_aliases.unwrap_or_default(),
                }]
            }),
            keywords,
            country,
            language,
            device,
            max_depth,
            min_query_interval_ms,
            max_queries_per_run,
            stop_after_first_target_match,
            capture_results,
            force_refresh_today,
            navigation_timeout_ms,
            worker_node_path,
            playwright_executable_path,
            user_agent,
        },
        SourceKind::AppleSearchAds {
            org_id,
            client_id,
            team_id,
            key_id,
            private_key_path,
            private_key_pem,
            start_date,
            end_date,
            lookback_days,
            stream_profile,
            processing_lag_days,
            time_zone,
            return_records_with_no_metrics,
            access_token,
            streams,
            max_concurrent_requests,
        } => SourceConfig::AppleSearchAds {
            org_id,
            client_id,
            team_id,
            key_id,
            private_key_path,
            private_key_pem,
            start_date,
            end_date,
            lookback_days,
            stream_profile,
            processing_lag_days,
            time_zone,
            access_token,
            streams,
            return_records_with_no_metrics,
            max_concurrent_requests,
        },
        SourceKind::MetaInstagramAds {
            ad_account_id,
            start_date,
            end_date,
            lookback_days,
            stream_profile,
            processing_lag_days,
            api_version,
            access_token,
            oauth_token_url,
            oauth_client_id,
            oauth_client_secret,
            oauth_refresh_token,
            instagram_filter,
            streams,
        } => SourceConfig::MetaInstagramAds {
            ad_account_id,
            start_date,
            end_date,
            lookback_days,
            stream_profile,
            processing_lag_days,
            api_version,
            access_token,
            oauth_token_url,
            oauth_client_id,
            oauth_client_secret,
            oauth_refresh_token,
            instagram_filter,
            streams,
        },
        SourceKind::DataForSeoBacklinks {
            login,
            password,
            site,
            run_mode,
            backlink_target,
            limit,
            max_pages,
            request_interval_ms,
        } => SourceConfig::DataForSeoBacklinks {
            login,
            password,
            site,
            run_mode,
            backlink_target,
            limit,
            max_pages,
            request_interval_ms,
        },
        SourceKind::DataForSeoSeoOpportunities {
            login,
            password,
            site,
            location_code,
            language_code,
            device,
            run_mode,
            seed_keywords,
            request_interval_ms,
        } => SourceConfig::DataForSeoSeoOpportunities {
            login,
            password,
            site,
            location_code,
            language_code,
            device,
            run_mode,
            seed_keywords,
            request_interval_ms,
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
        SourceConfig::GoogleAnalytics { .. } => "google_analytics",
        SourceConfig::GoogleSearchConsole { .. } => "google_search_console",
        SourceConfig::BingWebmasterTools { .. } => "bing_webmaster_tools",
        SourceConfig::GooglePageSpeed { .. } => "google_pagespeed",
        SourceConfig::AiCitations { .. } => "ai_citations",
        SourceConfig::GoogleSerpRanks { .. } => "google_serp_ranks",
        SourceConfig::SiteQuality { .. } => "site_quality",
        SourceConfig::SeoCrawl { .. } => "seo_crawl",
        SourceConfig::AppleSearchAds { .. } => "apple_search_ads",
        SourceConfig::MetaInstagramAds { .. } => "meta_instagram_ads",
        SourceConfig::DataForSeoBacklinks { .. } => "dataforseo_backlinks",
        SourceConfig::DataForSeoSeoOpportunities { .. } => "dataforseo_seo_opportunities",
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

    if is_json_output(output) {
        emit_connect_result(output, explicit_config, kind_label, "source");
    } else {
        println!("Source ({}) configured.", kind_label);
    }
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

/// Walk YAML for whole-scalar `${VAR}` references that are unset or empty (mirrors
/// `skipprd::helpers::configuration::Config::resolve_env_ref` rules) for diagnostics only.
fn collect_missing_env_scalar_messages_yaml(
    value: &serde_yaml::Value,
    path: &str,
    out: &mut Vec<String>,
) {
    match value {
        serde_yaml::Value::String(s) => {
            if let Some(msg) = missing_env_scalar_message_if_unset(s, path) {
                out.push(msg);
            }
        }
        serde_yaml::Value::Mapping(map) => {
            for (k, v) in map.iter() {
                let key = k.as_str().unwrap_or("?");
                let child_path = if path.is_empty() {
                    key.to_string()
                } else {
                    format!("{}.{}", path, key)
                };
                collect_missing_env_scalar_messages_yaml(v, &child_path, out);
            }
        }
        serde_yaml::Value::Sequence(seq) => {
            for (i, v) in seq.iter().enumerate() {
                let child_path = if path.is_empty() {
                    format!("[{}]", i)
                } else {
                    format!("{}[{}]", path, i)
                };
                collect_missing_env_scalar_messages_yaml(v, &child_path, out);
            }
        }
        serde_yaml::Value::Null | serde_yaml::Value::Bool(_) | serde_yaml::Value::Number(_) => {}
        serde_yaml::Value::Tagged(t) => {
            collect_missing_env_scalar_messages_yaml(&t.value, path, out);
        }
    }
}

fn missing_env_scalar_message_if_unset(value: &str, path: &str) -> Option<String> {
    let trimmed = value.trim();
    if !(trimmed.starts_with("${") && trimmed.ends_with('}')) {
        return None;
    }
    if trimmed.len() <= 3 || trimmed[2..trimmed.len() - 1].contains("${") {
        return None;
    }
    if trimmed != value {
        return None;
    }
    let var_name = &trimmed[2..trimmed.len() - 1];
    if var_name.trim().is_empty() {
        return None;
    }
    match std::env::var(var_name) {
        Ok(v) if !v.trim().is_empty() => None,
        Ok(_) => Some(format!(
            "skippr.yml references ${{{}}} at {}, but that environment variable is empty",
            var_name, path
        )),
        Err(_) => Some(format!(
            "skippr.yml references ${{{}}} at {}, but that environment variable is not set",
            var_name, path
        )),
    }
}

fn load_doctor_config(
    explicit_config: &Option<PathBuf>,
) -> Result<(PathBuf, serde_yaml::Value), String> {
    let path = config_path(explicit_config);
    if !path.exists() {
        return Err(format!(
            "{} not found — run 'skippr init <project>'",
            path.display()
        ));
    }
    let mut cfg = load_engine_config(explicit_config)?;
    warn_and_normalize_legacy_cli_config(&mut cfg)?;
    Ok((path.clone(), cfg))
}

fn push_doctor_check(
    checks: &mut Vec<DoctorCheck>,
    ok: bool,
    message: impl Into<String>,
    suggested_fix_command: Option<&str>,
) {
    checks.push(DoctorCheck {
        ok,
        severity: if ok { "info" } else { "error" },
        message: message.into(),
        suggested_fix_command: suggested_fix_command.map(str::to_string),
    });
}

fn emit_doctor_check(
    output: &str,
    checks: &mut Vec<DoctorCheck>,
    ok: bool,
    message: impl Into<String>,
    fix: Option<&str>,
) {
    let message = message.into();
    if !is_json_output(output) {
        if ok {
            check_pass(&message);
        } else {
            check_fail(&message);
        }
    }
    push_doctor_check(checks, ok, message, fix);
}

fn cmd_doctor(explicit_config: &Option<PathBuf>, output: &str) {
    let mut ok = true;
    let mut checks = Vec::new();

    let (cfg_path, cfg) = match load_doctor_config(explicit_config) {
        Ok((path, c)) => {
            emit_doctor_check(
                output,
                &mut checks,
                true,
                format!("config file found: {}", path.display()),
                None,
            );
            (path, c)
        }
        Err(e) => {
            emit_doctor_check(
                output,
                &mut checks,
                false,
                &e,
                Some("skippr init <project>"),
            );
            if is_json_output(output) {
                print_json(&DoctorResult {
                    ok: false,
                    config_path: Some(config_path(explicit_config).display().to_string()),
                    checks,
                });
            }
            std::process::exit(1);
        }
    };
    let cfg_label = cfg_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config file");

    let mut missing_env_refs = Vec::new();
    collect_missing_env_scalar_messages_yaml(&cfg, "", &mut missing_env_refs);
    for msg in missing_env_refs {
        emit_doctor_check(output, &mut checks, false, msg, None);
        ok = false;
    }

    if cfg
        .get("pipelines")
        .and_then(|pipelines| pipelines.as_mapping())
        .map(|pipelines| !pipelines.is_empty())
        .unwrap_or(false)
    {
        emit_doctor_check(output, &mut checks, true, "pipelines configured", None);
    } else {
        emit_doctor_check(
            output,
            &mut checks,
            false,
            format!("no pipelines configured in {cfg_label}"),
            Some("skippr init <project>"),
        );
        ok = false;
    }

    if cfg
        .get("data_sources")
        .and_then(|sources| sources.as_mapping())
        .map(|sources| !sources.is_empty())
        .unwrap_or(false)
    {
        emit_doctor_check(output, &mut checks, true, "data sources configured", None);
    } else {
        emit_doctor_check(
            output,
            &mut checks,
            false,
            "data source not configured — run 'skippr connect source <kind>'",
            Some("skippr connect source <kind>"),
        );
        ok = false;
    }

    if cfg
        .get("data_sinks")
        .and_then(|sinks| sinks.as_mapping())
        .map(|sinks| !sinks.is_empty())
        .unwrap_or(false)
    {
        emit_doctor_check(output, &mut checks, true, "data sinks configured", None);
    } else {
        emit_doctor_check(
            output,
            &mut checks,
            false,
            "warehouse/data sink not configured — run 'skippr connect warehouse <kind>'",
            Some("skippr connect warehouse <kind>"),
        );
        ok = false;
    }

    if which("dbt") {
        emit_doctor_check(output, &mut checks, true, "dbt binary found on PATH", None);
    } else {
        emit_doctor_check(
            output,
            &mut checks,
            false,
            "dbt not found on PATH — install dbt-core and the warehouse adapter in a venv",
            None,
        );
        ok = false;
    }

    if which("python3") || which("python") {
        emit_doctor_check(output, &mut checks, true, "python found on PATH", None);
    } else {
        emit_doctor_check(
            output,
            &mut checks,
            false,
            "python not found on PATH — Python 3.10+ is required for dbt",
            None,
        );
        ok = false;
    }

    if auth::load_credentials().is_some() || env_set("SKIPPR_API_KEY") {
        emit_doctor_check(
            output,
            &mut checks,
            true,
            "authenticated (credentials or SKIPPR_API_KEY)",
            None,
        );
    } else {
        emit_doctor_check(
            output,
            &mut checks,
            false,
            "not authenticated — run 'skippr user login' or set SKIPPR_API_KEY",
            Some("skippr user login"),
        );
        ok = false;
    }

    if std::env::var("LLM_API_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .is_some()
    {
        emit_doctor_check(
            output,
            &mut checks,
            true,
            "Custom LLM key override is set",
            None,
        );
    } else {
        emit_doctor_check(
            output,
            &mut checks,
            true,
            "LLM credentials managed by Skippr",
            None,
        );
    }

    let cfg_raw = serde_yaml::to_string(&cfg)
        .unwrap_or_default()
        .to_lowercase();
    if !is_json_output(output) && cfg_raw.contains("athena") {
        check_athena_env(&mut ok);
    }

    if !is_json_output(output) && cfg_raw.contains("snowflake") {
        check_snowflake_env(&mut ok);
    }

    if !is_json_output(output) && cfg_raw.contains("bigquery") {
        check_bigquery_env(&mut ok);
    }

    if !is_json_output(output) && cfg_raw.contains("postgres") {
        check_postgres_env(&mut ok);
    }

    if !is_json_output(output)
        && (cfg_raw.contains("googlepagespeed") || cfg_raw.contains("google_pagespeed"))
    {
        check_pagespeed_env(output, &mut checks, &mut ok);
    }

    if !is_json_output(output)
        && (cfg_raw.contains("sitequality") || cfg_raw.contains("site_quality"))
    {
        check_site_quality_env(output, &mut checks, &mut ok);
    }

    if !is_json_output(output)
        && (cfg_raw.contains("aicitations") || cfg_raw.contains("ai_citations"))
    {
        check_ai_citations_env(output, &mut checks, &mut ok);
    }

    if cfg_raw.contains("dataforseo") {
        check_dataforseo_backlinks_credentials(output, &mut checks, &mut ok);
    }

    if is_json_output(output) {
        print_json(&DoctorResult {
            ok,
            config_path: Some(cfg_path.display().to_string()),
            checks,
        });
        if !ok {
            std::process::exit(1);
        }
    } else if ok {
        println!();
        println!("All checks passed. Run 'skippr discover', 'skippr sync', or 'skippr model'.");
    } else {
        println!();
        println!("Some checks failed. Fix the issues above and re-run 'skippr doctor'.");
        std::process::exit(1);
    }
}

fn yaml_mapping_keys(value: Option<&serde_yaml::Value>) -> Vec<String> {
    let mut keys: Vec<String> = value
        .and_then(|value| value.as_mapping())
        .map(|mapping| {
            mapping
                .keys()
                .filter_map(|key| key.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    keys.sort();
    keys
}

fn yaml_string(value: &serde_yaml::Value) -> Option<&str> {
    value.as_str()
}

fn yaml_mapping(value: Option<&serde_yaml::Value>) -> Option<&serde_yaml::Mapping> {
    value.and_then(|value| value.as_mapping())
}

fn mapping_get<'a>(mapping: &'a serde_yaml::Mapping, key: &str) -> Option<&'a serde_yaml::Value> {
    mapping.get(serde_yaml::Value::String(key.to_string()))
}

fn registry_ref_name(value: Option<&serde_yaml::Value>, section: &str) -> Option<String> {
    let raw = value.and_then(yaml_string)?.trim();
    raw.strip_prefix(&format!("{section}."))
        .filter(|name| !name.trim().is_empty())
        .map(str::to_string)
}

fn plugin_config_mapping<'a>(
    entry: &'a serde_yaml::Value,
    provider: &str,
) -> Option<&'a serde_yaml::Mapping> {
    entry
        .as_mapping()
        .and_then(|mapping| mapping_get(mapping, provider).and_then(|value| value.as_mapping()))
}

fn provider_label(provider: Option<&str>) -> String {
    let Some(provider) = provider.filter(|value| !value.trim().is_empty()) else {
        return "Unknown".to_string();
    };
    provider
        .chars()
        .enumerate()
        .flat_map(|(idx, ch)| {
            if idx > 0 && ch.is_uppercase() {
                vec![' ', ch]
            } else {
                vec![ch]
            }
        })
        .collect()
}

fn is_secret_field_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("password")
        || lower.contains("secret")
        || lower.contains("token")
        || lower.contains("credential")
        || lower.contains("private_key")
        || lower.contains("connection_string")
        || lower.contains("dsn")
}

fn display_config_value(value: &serde_yaml::Value) -> String {
    match value {
        serde_yaml::Value::Null => "null".to_string(),
        serde_yaml::Value::Bool(value) => value.to_string(),
        serde_yaml::Value::Number(value) => value.to_string(),
        serde_yaml::Value::String(value) => {
            if value.starts_with("${") && value.ends_with('}') {
                value.clone()
            } else {
                value.chars().take(120).collect()
            }
        }
        serde_yaml::Value::Sequence(values) => format!("{} item(s)", values.len()),
        serde_yaml::Value::Mapping(values) => format!("{} field(s)", values.len()),
        serde_yaml::Value::Tagged(tagged) => display_config_value(&tagged.value),
    }
}

fn redacted_config_fields(
    entry: &serde_yaml::Value,
    provider: Option<&str>,
) -> Vec<ConfigShowConnectionField> {
    let Some(provider) = provider else {
        return Vec::new();
    };
    let Some(mapping) = plugin_config_mapping(entry, provider) else {
        return Vec::new();
    };
    let mut fields: Vec<ConfigShowConnectionField> = mapping
        .iter()
        .filter_map(|(key, value)| {
            let name = key.as_str()?.to_string();
            let secret = is_secret_field_name(&name);
            Some(ConfigShowConnectionField {
                name,
                value: if secret {
                    "********".to_string()
                } else {
                    display_config_value(value)
                },
                secret,
            })
        })
        .collect();
    fields.sort_by(|left, right| left.name.cmp(&right.name));
    fields
}

fn supports_sql_provider(provider: Option<&str>) -> bool {
    matches!(
        provider.map(|value| value.to_ascii_lowercase()).as_deref(),
        Some("athena")
            | Some("snowflake")
            | Some("bigquery")
            | Some("postgres")
            | Some("databricks")
            | Some("synapse")
            | Some("redshift")
            | Some("clickhouse")
            | Some("motherduck")
    )
}

fn direct_pipeline_refs(cfg: &serde_yaml::Value, section: &str, name: &str) -> Vec<String> {
    let Some(pipelines) = yaml_mapping(cfg.get("pipelines")) else {
        return Vec::new();
    };
    let data_key = match section {
        "data_sources" => "data_source",
        "data_sinks" => "data_sink",
        _ => return Vec::new(),
    };
    let mut refs: Vec<String> = pipelines
        .iter()
        .filter_map(|(pipeline_name, pipeline_value)| {
            let pipeline_name = pipeline_name.as_str()?;
            let pipeline_mapping = pipeline_value.as_mapping()?;
            let ref_name = registry_ref_name(mapping_get(pipeline_mapping, data_key), section)?;
            (ref_name == name).then(|| pipeline_name.to_string())
        })
        .collect();
    refs.sort();
    refs
}

fn linked_sinks_for_schema_sink(cfg: &serde_yaml::Value, schema_sink_name: &str) -> Vec<String> {
    let Some(sinks) = yaml_mapping(cfg.get("data_sinks")) else {
        return Vec::new();
    };
    let mut linked: Vec<String> = sinks
        .iter()
        .filter_map(|(sink_name, sink_value)| {
            let sink_name = sink_name.as_str()?;
            let mapping = sink_value.as_mapping()?;
            let schema_sink =
                registry_ref_name(mapping_get(mapping, "schema_sink"), "schema_sinks")?;
            (schema_sink == schema_sink_name).then(|| sink_name.to_string())
        })
        .collect();
    linked.sort();
    linked
}

fn schema_sink_pipeline_refs(cfg: &serde_yaml::Value, schema_sink_name: &str) -> Vec<String> {
    let mut pipelines: Vec<String> = linked_sinks_for_schema_sink(cfg, schema_sink_name)
        .into_iter()
        .flat_map(|sink_name| direct_pipeline_refs(cfg, "data_sinks", &sink_name))
        .collect();
    pipelines.sort();
    pipelines.dedup();
    pipelines
}

fn config_show_section_connections(
    cfg: &serde_yaml::Value,
    section: &str,
    pipeline_section: Option<&str>,
) -> Vec<ConfigShowConnection> {
    let Some(entries) = yaml_mapping(cfg.get(section)) else {
        return Vec::new();
    };
    let mut connections: Vec<ConfigShowConnection> = entries
        .iter()
        .filter_map(|(name, entry)| {
            let name = name.as_str()?.to_string();
            let provider = entry.as_mapping().and_then(plugin_mapping_key);
            let schema_sink = if section == "data_sinks" {
                entry.as_mapping().and_then(|mapping| {
                    registry_ref_name(mapping_get(mapping, "schema_sink"), "schema_sinks")
                })
            } else {
                None
            };
            let linked_sinks = if section == "schema_sinks" {
                linked_sinks_for_schema_sink(cfg, &name)
            } else {
                Vec::new()
            };
            let pipelines = if section == "schema_sinks" {
                schema_sink_pipeline_refs(cfg, &name)
            } else {
                pipeline_section
                    .map(|pipeline_section| direct_pipeline_refs(cfg, pipeline_section, &name))
                    .unwrap_or_default()
            };
            let supports_sql =
                section == "data_sinks" && supports_sql_provider(provider.as_deref());
            Some(ConfigShowConnection {
                name,
                label: provider_label(provider.as_deref()),
                fields: redacted_config_fields(entry, provider.as_deref()),
                provider,
                pipelines,
                schema_sink,
                linked_sinks,
                supports_sql,
            })
        })
        .collect();
    connections.sort_by(|left, right| left.name.cmp(&right.name));
    connections
}

fn config_show_connections(cfg: &serde_yaml::Value) -> ConfigShowConnections {
    ConfigShowConnections {
        sources: config_show_section_connections(cfg, "data_sources", Some("data_sources")),
        sinks: config_show_section_connections(cfg, "data_sinks", Some("data_sinks")),
        schema_sinks: config_show_section_connections(cfg, "schema_sinks", None),
    }
}

fn cmd_config_schema(output: &str) {
    let schema = config_schema();
    if is_json_output(output) {
        print_json(&schema);
    } else {
        println!("Skippr config schema v{}", schema.version);
        println!("Sources:");
        for source in schema.sources {
            println!("  {}", source.kind);
        }
        println!("Warehouses:");
        for warehouse in schema.warehouses {
            println!("  {}", warehouse.kind);
        }
    }
}

fn cmd_config_show(explicit_config: &Option<PathBuf>, output: &str) {
    let path = config_path(explicit_config);
    let cfg = match load_engine_config(explicit_config)
        .and_then(|mut v| warn_and_normalize_legacy_cli_config(&mut v).map(|_| v))
    {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };
    let pipelines = yaml_mapping_keys(cfg.get("pipelines"));
    let connections = config_show_connections(&cfg);
    let result = ConfigShowResult {
        ok: true,
        config_path: path.display().to_string(),
        workspace: engine_project_name(&cfg).ok(),
        pipelines,
        sources: yaml_mapping_keys(cfg.get("data_sources")),
        sinks: yaml_mapping_keys(cfg.get("data_sinks")),
        schema_sinks: yaml_mapping_keys(cfg.get("schema_sinks")),
        connections,
    };
    if is_json_output(output) {
        print_json(&result);
    } else {
        println!("Config: {}", result.config_path);
        if let Some(workspace) = &result.workspace {
            println!("Workspace: {workspace}");
        }
        println!("Pipelines: {}", result.pipelines.join(", "));
        println!("Sources: {}", result.sources.join(", "));
        println!("Sinks: {}", result.sinks.join(", "));
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

fn check_pagespeed_env(output: &str, checks: &mut Vec<DoctorCheck>, ok: &mut bool) {
    if env_set("PAGESPEED_API_KEY") {
        emit_doctor_check(
            output,
            checks,
            true,
            "PAGESPEED_API_KEY is set",
            None,
        );
    } else {
        emit_doctor_check(
            output,
            checks,
            false,
            "PAGESPEED_API_KEY not set — required for Google PageSpeed Insights API calls",
            Some("Create a key in Google Cloud Console and export PAGESPEED_API_KEY"),
        );
        *ok = false;
    }
}

fn check_dataforseo_backlinks_credentials(
    output: &str,
    checks: &mut Vec<DoctorCheck>,
    ok: &mut bool,
) {
    if env_set("SKIPPR_DATAFORSEO_BACKLINKS_FIXTURE_DIR") {
        emit_doctor_check(
            output,
            checks,
            true,
            "SKIPPR_DATAFORSEO_BACKLINKS_FIXTURE_DIR set (offline fixtures)",
            None,
        );
        return;
    }

    let Some((login, password)) =
        skippr_plugin_data_source_dataforseo_backlinks::config::credentials_from_env()
    else {
        emit_doctor_check(
            output,
            checks,
            false,
            "DATAFORSEO_API_USER and DATAFORSEO_API_PASS (or DATAFORSEO_LOGIN / DATAFORSEO_PASSWORD) must be set for DataForSEO Backlinks",
            Some("Export credentials from https://app.dataforseo.com/api-access"),
        );
        *ok = false;
        return;
    };
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            emit_doctor_check(
                output,
                checks,
                false,
                format!("DataForSEO credential probe failed to start runtime: {e}"),
                None,
            );
            *ok = false;
            return;
        }
    };
    match rt.block_on(
        skippr_plugin_data_source_dataforseo_backlinks::client::DataForSeoClient::probe_credentials(
            &login,
            &password,
        ),
    ) {
        Ok(()) => emit_doctor_check(
            output,
            checks,
            true,
            "DataForSEO API credentials verified (backlinks/live probe)",
            None,
        ),
        Err(e) => {
            emit_doctor_check(
                output,
                checks,
                false,
                format!("DataForSEO API credential probe failed: {e}"),
                Some("Check DATAFORSEO_API_USER / DATAFORSEO_API_PASS at https://app.dataforseo.com/api-access"),
            );
            *ok = false;
        }
    }
}

fn check_ai_citations_env(output: &str, checks: &mut Vec<DoctorCheck>, ok: &mut bool) {
    let fixture_mode = std::env::var("SKIPPR_AI_CITATIONS_FIXTURE_DIR")
        .ok()
        .filter(|d| !d.trim().is_empty())
        .is_some()
        || std::env::var("SKIPPR_OPENAI_FIXTURE_DIR")
            .ok()
            .filter(|d| !d.trim().is_empty())
            .is_some();

    if fixture_mode {
        emit_doctor_check(
            output,
            checks,
            true,
            "AI Citations fixture mode enabled (offline)",
            None,
        );
        return;
    }

    if std::env::var("OPENAI_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
        .is_some()
    {
        emit_doctor_check(
            output,
            checks,
            true,
            "OPENAI_API_KEY is set for AI Citations",
            None,
        );
    } else {
        emit_doctor_check(
            output,
            checks,
            false,
            "OPENAI_API_KEY is not set — required for AI Citations sync",
            Some("Export OPENAI_API_KEY or set SKIPPR_AI_CITATIONS_FIXTURE_DIR for offline runs"),
        );
        *ok = false;
    }
}

fn check_site_quality_env(output: &str, checks: &mut Vec<DoctorCheck>, ok: &mut bool) {
    if which("node") {
        emit_doctor_check(output, checks, true, "node found on PATH", None);
    } else {
        emit_doctor_check(
            output,
            checks,
            false,
            "node not found on PATH — required for Site Quality Playwright worker",
            Some("Install Node.js 20+ and ensure `node` is on PATH"),
        );
        *ok = false;
    }

    let fixture_mode = std::env::var("SKIPPR_SITE_QUALITY_FIXTURE_DIR")
        .ok()
        .filter(|d| !d.trim().is_empty())
        .is_some();

    if fixture_mode {
        emit_doctor_check(
            output,
            checks,
            true,
            "SKIPPR_SITE_QUALITY_FIXTURE_DIR set (offline fixture mode)",
            None,
        );
    }

    if fixture_mode {
        emit_doctor_check(
            output,
            checks,
            true,
            "worker script check skipped (fixture mode)",
            None,
        );
    } else {
        match skippr_runtime_sdk::site_quality_worker::resolve_site_quality_worker_script(None) {
            Ok(path) => {
                emit_doctor_check(
                    output,
                    checks,
                    true,
                    &format!("site-quality-worker.mjs found at {}", path.display()),
                    None,
                );
            }
            Err(err) => {
                emit_doctor_check(
                    output,
                    checks,
                    false,
                    &format!(
                        "site-quality-worker.mjs not found ({err}); run from a skipprd checkout or set SKIPPR_SITE_QUALITY_WORKER_SCRIPT"
                    ),
                    None,
                );
                *ok = false;
            }
        }
    }

    if fixture_mode {
        emit_doctor_check(
            output,
            checks,
            true,
            "Chromium check skipped (fixture mode)",
            None,
        );
    } else if which("npx") {
        emit_doctor_check(
            output,
            checks,
            true,
            "npx found — run `npx playwright install chromium` if Chromium is not cached",
            None,
        );
    } else {
        emit_doctor_check(
            output,
            checks,
            false,
            "Chromium for Playwright may be missing — install Node/npx and run `npx playwright install chromium`",
            None,
        );
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
    pipeline: &str,
) {
    let path = config_path(explicit_config);
    if !path.exists() {
        eprintln!("error: {} not found", path.display());
        eprintln!("Run 'skippr init <project>' first.");
        std::process::exit(1);
    }

    let engine_cfg = match load_cli_execution_config(explicit_config) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };
    if let Err(e) = validate_pipeline_exists(&engine_cfg, pipeline) {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }

    let server_credentials = match load_cli_server_credentials().await {
        Ok(credentials) => credentials,
        Err(e) => {
            eprintln!("[skippr] ERROR: {}", e);
            eprintln!("[skippr]   Run 'skippr user login' to authenticate interactively,");
            eprintln!("[skippr]   or set SKIPPR_API_KEY for CI/CD.");
            std::process::exit(1);
        }
    };
    let tenant = server_credentials.tenant_id.trim();
    if tenant.is_empty() {
        eprintln!("[skippr] ERROR: authenticated credentials did not include a tenant id.");
        std::process::exit(1);
    }
    skipprd::helpers::configuration::Config::setenv("TENANT", tenant);
    set_public_cli_el_storage_default();

    std::env::set_var("SKIPPR_CONFIG_FILE", &path);
    skipprd::helpers::logging::init_logging(log);
    skipprd::helpers::configuration::Config::build_config();
    skipprd::helpers::configuration::PIPELINE_NAME
        .write()
        .clear();
    skipprd::helpers::configuration::PIPELINE_NAME
        .write()
        .push_str(pipeline);
    let project_root = project_root_from_config_path(&path);
    let workspace = yaml_string_at(&engine_cfg, &["skippr", "workspace"]).unwrap_or("default");
    let pipeline_cfg = pipeline_config(&engine_cfg, pipeline).expect("pipeline validated above");
    let data_dir = cli_pipeline_data_dir(&project_root, tenant, pipeline, workspace, pipeline_cfg);
    if let Err(err) = std::fs::create_dir_all(&data_dir) {
        eprintln!(
            "[skippr] failed to create pipeline data directory '{}': {}",
            data_dir.display(),
            err
        );
        std::process::exit(1);
    }
    skipprd::helpers::configuration::Config::setenv("DATA_DIR", &data_dir.to_string_lossy());
    skipprd::helpers::configuration::Config::setenv("SKIPPR_PIPELINE_DATA_ROOT", "true");
    load_dotenv_for_skippr_config_yaml_path(&path);
    skipprd::helpers::configuration::Config::init().await;
    std::env::set_var("SKIPPR_CLOUD_WORKSPACE", workspace);
}

async fn cmd_thread_resolve(
    log: Option<String>,
    explicit_config: &Option<PathBuf>,
    pipeline: PipelineName,
    output: &str,
) {
    let engine_cfg = match load_cli_execution_config(explicit_config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = validate_pipeline_exists(&engine_cfg, pipeline.as_str()) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
    let internal_file = match react_config_from_pipeline_config(&engine_cfg, pipeline.as_str()) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    let resolved =
        match react_host::resolve_config(internal_file, react::config::ServeOverrides::default()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        };
    let thread_id = match find_latest_thread_for_resolved_config(&resolved).await {
        Ok(id) => id,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    if output == "json" {
        println!(
            "{}",
            serde_json::json!({ "threadId": thread_id, "pipeline": pipeline.as_str() })
        );
    } else if let Some(ref tid) = thread_id {
        println!("{tid}");
    } else {
        eprintln!(
            "[skippr] no thread found for pipeline {}",
            pipeline.as_str()
        );
    }
    let _ = log;
}

async fn cmd_discover(
    log: Option<String>,
    explicit_config: &Option<PathBuf>,
    args: EngineDiscoverArgs,
) {
    prepare_engine_command(log, explicit_config, &args.pipeline).await;
    let workspace = std::env::var("SKIPPR_CLOUD_WORKSPACE").unwrap_or_else(|_| "default".into());
    let failed = workspace_run_lock::with_heavy_run_lock(
        &workspace,
        "discover",
        Some(&args.pipeline),
        || async {
            skipprd::engine::run_discover(&args.output)
                .await
                .err()
                .map(|err| {
                    eprintln!("[skippr] discover failed: {}", err);
                })
                .is_some()
        },
    )
    .await;
    if failed {
        std::process::exit(1);
    }
}

async fn cmd_metadata(
    log: Option<String>,
    explicit_config: &Option<PathBuf>,
    action: metadata_cmd::MetadataAction,
) {
    match action {
        metadata_cmd::MetadataAction::Show(args) => {
            prepare_engine_command(log, explicit_config, &args.pipeline).await;
            metadata_cmd::run_metadata_show(&args.output).await;
        }
        metadata_cmd::MetadataAction::Apply(args) => {
            prepare_engine_command(log, explicit_config, &args.pipeline).await;
            metadata_cmd::run_metadata_apply(&args).await;
        }
    }
}

async fn cmd_sync(log: Option<String>, explicit_config: &Option<PathBuf>, args: EngineSyncArgs) {
    prepare_engine_command(log, explicit_config, &args.pipeline).await;
    let workspace = std::env::var("SKIPPR_CLOUD_WORKSPACE").unwrap_or_else(|_| "default".into());
    let command = workspace_run_lock::sync_api_command(args.once);
    let failed = workspace_run_lock::with_heavy_run_lock(
        &workspace,
        command,
        Some(&args.pipeline),
        || async {
            skipprd::metrics::Metrics::init_send_loop();
            skipprd::engine::run_sync(&args.output, args.once)
                .await
                .err()
                .map(|err| {
                    eprintln!("[skippr] sync failed: {}", err);
                })
                .is_some()
        },
    )
    .await;
    if failed {
        std::process::exit(1);
    }
}

async fn cmd_ask(log: Option<String>, explicit_config: &Option<PathBuf>, args: AskArgs) {
    chat_cmd::run_chat(
        log,
        explicit_config,
        chat_cmd::ChatAction::Send(chat_cmd::ChatSendArgs {
            pipeline: Some(args.pipeline),
            mode: chat_cmd::ChatModeCli::Ask,
            message: args.question,
            thread: None,
            output: args.output,
        }),
    )
    .await;
}

async fn cmd_plan(log: Option<String>, explicit_config: &Option<PathBuf>, args: PlanArgs) {
    let goal = args
        .goal
        .clone()
        .unwrap_or_else(|| "produce a data-engineering plan".to_string());
    chat_cmd::run_chat(
        log,
        explicit_config,
        chat_cmd::ChatAction::Send(chat_cmd::ChatSendArgs {
            pipeline: Some(args.pipeline),
            mode: chat_cmd::ChatModeCli::Plan,
            message: goal,
            thread: None,
            output: args.output,
        }),
    )
    .await;
}

async fn build_lineage_suite_ctx(
    explicit_config: &Option<PathBuf>,
    pipeline: &str,
) -> Result<react_core::suite::SuiteCtx, String> {
    let auth_ctx = headless_prep::authenticate_headless_for_pipeline(explicit_config, pipeline)
        .await
        .map_err(|e| e.to_string())?;
    let lineage_env = prepare_lineage_engine_environment(
        explicit_config,
        pipeline,
        auth_ctx.resolved.scope.tenant.as_str(),
    )
    .await?;
    let mut suite_ctx =
        react::bootstrap::build_suite_ctx_with(&auth_ctx.resolved, &react_host::SkipprHost)
            .await
            .map_err(|e| e.to_string())?;
    suite_ctx.set_capability(std::sync::Arc::new(
        react_suite_data_engineer::ctx_ext::SkipprdMetadataCap {
            locations: lineage_env.metadata_locations,
        },
    ));
    Ok(suite_ctx)
}

struct LineageEngineEnvironment {
    metadata_locations: Vec<react_suite_data_engineer::ctx_ext::SkipprdMetadataLocation>,
}

async fn prepare_lineage_engine_environment(
    explicit_config: &Option<PathBuf>,
    pipeline: &str,
    tenant: &str,
) -> Result<LineageEngineEnvironment, String> {
    use react_suite_data_engineer::ctx_ext::SkipprdMetadataLocation;

    let path = config_path(explicit_config);
    if !path.exists() {
        return Err(format!("{} not found", path.display()));
    }
    let engine_cfg = load_cli_execution_config(explicit_config)?;
    validate_pipeline_exists(&engine_cfg, pipeline)?;
    set_public_cli_el_storage_default();
    std::env::set_var("SKIPPR_CONFIG_FILE", &path);
    skipprd::helpers::configuration::Config::setenv("TENANT", tenant);
    skipprd::helpers::configuration::PIPELINE_NAME
        .write()
        .clear();
    skipprd::helpers::configuration::PIPELINE_NAME
        .write()
        .push_str(pipeline);

    let project_root = project_root_from_config_path(&path);
    let workspace = yaml_string_at(&engine_cfg, &["skippr", "workspace"]).unwrap_or("default");
    let pipeline_cfg = pipeline_config(&engine_cfg, pipeline)?;
    let data_dir = cli_pipeline_data_dir(&project_root, tenant, pipeline, workspace, pipeline_cfg);
    std::fs::create_dir_all(&data_dir).map_err(|err| {
        format!(
            "failed to create pipeline data directory '{}': {err}",
            data_dir.display()
        )
    })?;
    skipprd::helpers::configuration::Config::setenv("DATA_DIR", &data_dir.to_string_lossy());
    skipprd::helpers::configuration::Config::setenv("SKIPPR_PIPELINE_DATA_ROOT", "true");
    load_dotenv_for_skippr_config_yaml_path(&path);
    skipprd::helpers::configuration::Config::init().await;
    std::env::set_var("SKIPPR_CLOUD_WORKSPACE", workspace);

    let mut metadata_locations = Vec::new();
    metadata_locations.push(SkipprdMetadataLocation::LocalPath(
        data_dir.join("metadata").join("metadata.json"),
    ));
    metadata_locations.push(SkipprdMetadataLocation::LocalPath(
        data_dir
            .join(tenant)
            .join(workspace)
            .join(pipeline)
            .join("metadata")
            .join("metadata.json"),
    ));
    metadata_locations.push(SkipprdMetadataLocation::LocalPath(
        skippr_dir_from_project_root(&project_root)
            .join(tenant)
            .join(workspace)
            .join(pipeline)
            .join("metadata")
            .join("metadata.json"),
    ));
    metadata_locations.push(SkipprdMetadataLocation::StorageKey(format!(
        "{tenant}/{workspace}/{pipeline}/metadata/metadata.json"
    )));

    Ok(LineageEngineEnvironment { metadata_locations })
}

async fn load_all_lineage_graphs(
    explicit_config: &Option<PathBuf>,
    query: react_suite_data_engineer::lineage_types::LineageGraphQuery,
) -> Result<react_suite_data_engineer::lineage_types::LineageGraphSnapshot, String> {
    let cfg = load_engine_config(explicit_config)
        .and_then(|mut v| warn_and_normalize_legacy_cli_config(&mut v).map(|_| v))?;
    let pipelines = yaml_mapping_keys(cfg.get("pipelines"));
    let mut merged = react_suite_data_engineer::lineage_types::LineageGraphSnapshot::default();
    for pipeline in pipelines {
        let suite_ctx = build_lineage_suite_ctx(explicit_config, &pipeline).await?;
        let store = react_suite_data_engineer::lineage_store::LineageStore::new(
            suite_ctx.storage().clone(),
            suite_ctx.keyspace().clone(),
        );
        if let Some(graph) = store.read_graph(suite_ctx.scope()).await? {
            merged = react_suite_data_engineer::lineage_store::merge_graphs(merged, graph)?;
        }
    }
    Ok(react_suite_data_engineer::lineage_store::slice_graph(
        &merged, &query,
    ))
}

fn emit_lineage_json<T: Serialize>(output: &str, value: &T) {
    if is_jsonl_output(output) {
        print_json_line(value);
    } else {
        print_json(value);
    }
}

fn lineage_direction(raw: &str) -> react_suite_data_engineer::lineage_types::LineageDirection {
    match raw.trim().to_ascii_lowercase().as_str() {
        "upstream" => react_suite_data_engineer::lineage_types::LineageDirection::Upstream,
        "downstream" => react_suite_data_engineer::lineage_types::LineageDirection::Downstream,
        _ => react_suite_data_engineer::lineage_types::LineageDirection::Both,
    }
}

async fn cmd_lineage(
    _log: Option<String>,
    explicit_config: &Option<PathBuf>,
    action: LineageAction,
) {
    match action {
        LineageAction::Refresh(args) => {
            let output = args.output.clone();
            let suite_ctx = match build_lineage_suite_ctx(explicit_config, &args.pipeline).await {
                Ok(ctx) => ctx,
                Err(e) => {
                    emit_lineage_json(
                        &output,
                        &serde_json::json!({"ok": false, "pipeline": args.pipeline.as_str(), "error": e}),
                    );
                    std::process::exit(1);
                }
            };
            match react_suite_data_engineer::lineage_builder::refresh_lineage_graph_for_suite(
                &suite_ctx,
                react_suite_data_engineer::lineage_builder::LineageBuildOptions {
                    pipeline: args.pipeline.clone(),
                    include_query_history: args.include_query_history,
                    query_history_since: args.since.clone(),
                    query_history_limit: args.limit,
                },
            )
            .await
            {
                Ok(result) => emit_lineage_json(&output, &result),
                Err(e) => {
                    emit_lineage_json(
                        &output,
                        &serde_json::json!({"ok": false, "pipeline": args.pipeline.as_str(), "error": e}),
                    );
                    std::process::exit(1);
                }
            }
        }
        LineageAction::Graph(args) => {
            let output = args.output.clone();
            let query = react_suite_data_engineer::lineage_types::LineageGraphQuery {
                field_node_id: args.field_node_id.clone(),
                direction: lineage_direction(&args.direction),
            };
            if let Some(pipeline) = args.pipeline.as_ref() {
                let pipeline = pipeline.as_str();
                let suite_ctx = match build_lineage_suite_ctx(explicit_config, pipeline).await {
                    Ok(ctx) => ctx,
                    Err(e) => {
                        emit_lineage_json(
                            &output,
                            &serde_json::json!({"ok": false, "pipeline": pipeline, "error": e}),
                        );
                        std::process::exit(1);
                    }
                };
                match react_suite_data_engineer::lineage_builder::load_lineage_graph_for_suite(
                    &suite_ctx, query,
                )
                .await
                {
                    Ok(graph) => emit_lineage_json(
                        &output,
                        &serde_json::json!({"ok": true, "pipeline": pipeline, "graph": graph}),
                    ),
                    Err(e) => {
                        emit_lineage_json(
                            &output,
                            &serde_json::json!({"ok": false, "pipeline": pipeline, "error": e}),
                        );
                        std::process::exit(1);
                    }
                }
            } else {
                match load_all_lineage_graphs(explicit_config, query).await {
                    Ok(graph) => {
                        emit_lineage_json(&output, &serde_json::json!({"ok": true, "graph": graph}))
                    }
                    Err(e) => {
                        emit_lineage_json(&output, &serde_json::json!({"ok": false, "error": e}));
                        std::process::exit(1);
                    }
                }
            }
        }
        LineageAction::ImportQueryHistory(args) => {
            let output = args.output.clone();
            let suite_ctx = match build_lineage_suite_ctx(explicit_config, &args.pipeline).await {
                Ok(ctx) => ctx,
                Err(e) => {
                    emit_lineage_json(
                        &output,
                        &serde_json::json!({"ok": false, "pipeline": args.pipeline.as_str(), "error": e}),
                    );
                    std::process::exit(1);
                }
            };
            match react_suite_data_engineer::lineage_builder::import_query_history_for_suite(
                &suite_ctx,
                args.since.clone(),
                args.limit,
                args.include_non_select,
            )
            .await
            {
                Ok(result) => emit_lineage_json(&output, &result),
                Err(e) => {
                    emit_lineage_json(
                        &output,
                        &serde_json::json!({"ok": false, "pipeline": args.pipeline.as_str(), "error": e}),
                    );
                    std::process::exit(1);
                }
            }
        }
    }
}

async fn cmd_query(_log: Option<String>, explicit_config: &Option<PathBuf>, args: QueryArgs) {
    let emit = |body: &serde_json::Value| {
        if is_jsonl_output(&args.output) {
            print_json_line(body);
        } else {
            print_json(body);
        }
    };

    let auth_ctx =
        match headless_prep::authenticate_headless_for_pipeline(explicit_config, &args.pipeline)
            .await
        {
            Ok(ctx) => ctx,
            Err(e) => {
                let body = serde_json::json!({
                    "ok": false,
                    "error": e,
                    "pipeline": args.pipeline.clone(),
                });
                emit(&body);
                std::process::exit(1);
            }
        };

    let suite_ctx =
        match react::bootstrap::build_suite_ctx_with(&auth_ctx.resolved, &react_host::SkipprHost)
            .await
        {
            Ok(ctx) => ctx,
            Err(e) => {
                let body = serde_json::json!({
                    "ok": false,
                    "error": e,
                    "pipeline": args.pipeline.clone(),
                });
                emit(&body);
                std::process::exit(1);
            }
        };

    let Some(query) = suite_ctx
        .capability::<react_suite_data_engineer::ctx_ext::QueryCap>()
        .map(|cap| cap.0.clone())
    else {
        let body = serde_json::json!({
            "ok": false,
            "error": "query provider missing",
            "pipeline": args.pipeline.clone(),
        });
        emit(&body);
        std::process::exit(1);
    };

    let prepared =
        match react_suite_data_engineer::sql_prepare::prepare_read_only_sql(&args.sql, 50) {
            Ok(prepared) => prepared,
            Err(e) => {
                let body = serde_json::json!({
                    "ok": false,
                    "error": e,
                    "pipeline": args.pipeline.clone(),
                });
                emit(&body);
                std::process::exit(1);
            }
        };

    let started_at = std::time::Instant::now();
    match query.query(&prepared.sql).await {
        Ok(qr) => {
            let body = serde_json::json!({
                "ok": true,
                "pipeline": args.pipeline.clone(),
                "sql": prepared.sql,
                "data": {
                    "header": qr.header,
                    "rows": qr.rows,
                },
                "meta": qr.meta,
                "elapsed_ms": started_at.elapsed().as_millis() as u64,
                "probe": {
                    "normalized_sql": prepared.normalized_sql,
                },
            });
            emit(&body);
        }
        Err(e) => {
            let body = serde_json::json!({
                "ok": false,
                "pipeline": args.pipeline.clone(),
                "sql": prepared.sql,
                "error": e,
                "elapsed_ms": started_at.elapsed().as_millis() as u64,
                "probe": {
                    "normalized_sql": prepared.normalized_sql,
                },
            });
            emit(&body);
            std::process::exit(1);
        }
    }
}

struct DirectDbtPreflightRun {
    result: react_module_provider_dbt::DbtPreflightResult,
    profiles_temp: Option<tempfile::TempDir>,
}

fn failed_dbt_preflight(message: String) -> DirectDbtPreflightRun {
    DirectDbtPreflightRun {
        result: react_module_provider_dbt::DbtPreflightResult {
            ok: false,
            runner: "unknown".to_string(),
            command: "dbt".to_string(),
            code: -1,
            stdout: String::new(),
            stderr: message.clone(),
            remediation: Some(message),
        },
        profiles_temp: None,
    }
}

fn is_dbt_diff_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("sql" | "yml" | "yaml" | "csv" | "md" | "txt")
    )
}

fn snapshot_dbt_diff_files(root: &Path) -> HashMap<String, String> {
    fn visit(root: &Path, dir: &Path, out: &mut HashMap<String, String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if matches!(name, "target" | "logs" | "dbt_packages" | ".git") {
                    continue;
                }
                visit(root, &path, out);
                continue;
            }
            if !path.is_file() || !is_dbt_diff_file(&path) {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.insert(rel, content);
        }
    }

    let mut out = HashMap::new();
    if root.is_dir() {
        visit(root, root, &mut out);
    }
    out
}

fn changed_dbt_diff_files(
    before: &HashMap<String, String>,
    after: &HashMap<String, String>,
) -> Vec<serde_json::Value> {
    let mut paths: Vec<String> = before
        .keys()
        .chain(after.keys())
        .cloned()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter(|path| before.get(path) != after.get(path))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let before_text = before.get(&path).map(String::as_str).unwrap_or("");
            let after_text = after.get(&path).map(String::as_str).unwrap_or("");
            let before_lines: std::collections::BTreeSet<&str> = before_text.lines().collect();
            let after_lines: std::collections::BTreeSet<&str> = after_text.lines().collect();
            let lines_added = after_lines.difference(&before_lines).count();
            let lines_removed = before_lines.difference(&after_lines).count();
            let change_kind = if !before.contains_key(&path) {
                "created"
            } else if !after.contains_key(&path) {
                "deleted"
            } else {
                "modified"
            };
            serde_json::json!({
                "path": path,
                "change_kind": change_kind,
                "lines_added": lines_added,
                "lines_removed": lines_removed,
            })
        })
        .collect()
}

fn run_direct_dbt_preflight(
    resolved: &react_core::resolved_config::ReactResolvedConfig,
    direct_project_dir: Option<&Path>,
) -> DirectDbtPreflightRun {
    let Some(providers) = react_suite_data_engineer::de_config::de_config_from_resolved(resolved)
    else {
        return failed_dbt_preflight(
            "suite_config missing or invalid for dbt preflight".to_string(),
        );
    };
    if !providers.dbt.enabled {
        return failed_dbt_preflight("dbt is disabled in the resolved Skippr config".to_string());
    }
    let runner_mode = match react_module_provider_dbt::DbtRunnerMode::parse(&providers.dbt.runner) {
        Ok(mode) => mode,
        Err(err) => return failed_dbt_preflight(err),
    };
    let runner = react_module_provider_dbt::DbtRunnerConfig {
        mode: runner_mode,
        docker_image: providers.dbt.docker_image.clone(),
        docker_platform: providers.dbt.docker_platform.clone(),
        docker_network: providers.dbt.docker_network.clone(),
        docker_mount_aws_dir: providers.dbt.docker_mount_aws_dir,
    };
    let project_dir = direct_project_dir.unwrap_or_else(|| Path::new("."));
    if let Err(err) = std::fs::create_dir_all(project_dir) {
        return failed_dbt_preflight(format!(
            "failed to create dbt project directory {}: {err}",
            project_dir.display()
        ));
    }

    let generated =
        match react_suite_data_engineer::skippr_cli_generate_dbt_profiles_yml(resolved, None) {
            Ok(generated) => generated,
            Err(err) => return failed_dbt_preflight(err),
        };
    let profiles_temp = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(err) => {
            return failed_dbt_preflight(format!("failed to create profiles tempdir: {err}"))
        }
    };
    let profiles_path = profiles_temp.path().join("profiles.yml");
    if let Err(err) = std::fs::write(&profiles_path, generated.profiles_yml.as_bytes()) {
        return failed_dbt_preflight(format!(
            "failed to write generated profiles.yml for preflight: {err}"
        ));
    }
    let mut env_pairs = vec![(
        "DBT_PROFILES_DIR",
        profiles_temp.path().to_string_lossy().to_string(),
    )];
    for (key, value) in generated.tier_routing.env_vars() {
        if !value.trim().is_empty() {
            env_pairs.push((key, value));
        }
    }
    let result = react_module_provider_dbt::preflight_dbt_environment_for_runner(
        &runner,
        project_dir,
        Some(profiles_temp.path()),
        &env_pairs,
    );
    DirectDbtPreflightRun {
        result,
        profiles_temp: Some(profiles_temp),
    }
}

async fn cmd_model(log: Option<String>, explicit_config: &Option<PathBuf>, args: ModelArgs) {
    let agent_type = "model";
    let transport_agent_type = "agent";
    emit_de_suite_event(
        &args.output,
        DeSuiteEvent {
            event: "model_start",
            timestamp: event_timestamp(),
            pipeline: &args.pipeline,
            run_kind: Some(agent_type),
            thread_id: None,
            phase: Some("preflight"),
            repair_status: None,
            pending_plan_revision: None,
            failure_summary: None,
            error: None,
            answer: None,
            plan: None,
            ok: None,
            model_preflight: None,
            changed_files: None,
            validation: None,
        },
    );
    let engine_cfg = match load_cli_execution_config(explicit_config) {
        Ok(c) => c,
        Err(e) => {
            emit_de_suite_event(
                &args.output,
                DeSuiteEvent {
                    event: "model_error",
                    timestamp: event_timestamp(),
                    pipeline: &args.pipeline,
                    run_kind: Some(agent_type),
                    thread_id: None,
                    phase: Some("preflight"),
                    repair_status: None,
                    pending_plan_revision: None,
                    failure_summary: Some(&e),
                    error: Some(&e),
                    answer: None,
                    plan: None,
                    ok: Some(false),
                    model_preflight: None,
                    changed_files: None,
                    validation: None,
                },
            );
            eprintln!("error: {}", e);
            eprintln!("Run 'skippr init <project>' first.");
            std::process::exit(1);
        }
    };
    let mut internal_file = match react_config_from_pipeline_config(&engine_cfg, &args.pipeline) {
        Ok(f) => f,
        Err(e) => {
            emit_de_suite_event(
                &args.output,
                DeSuiteEvent {
                    event: "model_error",
                    timestamp: event_timestamp(),
                    pipeline: &args.pipeline,
                    run_kind: Some(agent_type),
                    thread_id: None,
                    phase: Some("preflight"),
                    repair_status: None,
                    pending_plan_revision: None,
                    failure_summary: Some(&e),
                    error: Some(&e),
                    answer: None,
                    plan: None,
                    ok: Some(false),
                    model_preflight: None,
                    changed_files: None,
                    validation: None,
                },
            );
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

    let workspace =
        yaml_string_at(&engine_cfg, &["skippr", "workspace"]).unwrap_or_else(|| "default".into());
    workspace_run_lock::with_heavy_run_lock_client(
        client.clone(),
        &workspace,
        "model",
        Some(&args.pipeline),
        || async {
            run_model_body(
                log,
                explicit_config,
                &args,
                &engine_cfg,
                agent_type,
                transport_agent_type,
                client,
                internal_file,
            )
            .await;
        },
    )
    .await;
}

async fn run_model_body(
    log: Option<String>,
    explicit_config: &Option<PathBuf>,
    args: &ModelArgs,
    _engine_cfg: &serde_yaml::Value,
    agent_type: &str,
    transport_agent_type: &str,
    client: api_client::ApiClient,
    internal_file: react::config::ReactConfigFile,
) {
    set_public_cli_el_storage_default();
    let model_config_path = config_path(explicit_config);
    std::env::set_var("SKIPPR_CONFIG_FILE", &model_config_path);
    let run_id = uuid::Uuid::new_v4().to_string();
    react_suite_data_engineer::metering::set_metering_run_id(&run_id);
    eprintln!("[skippr] run {run_id}");

    let metering = react_suite_data_engineer::metering::global_metering();
    let _ = metering
        .record_batch(&[
            react_suite_data_engineer::metering::UsageEvent::PipelineRun {
                project_id: args.pipeline.as_str().to_string(),
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

    if resolved.scope.project_id.as_str() != args.pipeline.as_str() {
        let msg = format!(
            "resolved model scope project '{}' does not match requested pipeline '{}'; refusing to run to avoid writing dbt artifacts under the wrong pipeline prefix",
            resolved.scope.project_id, args.pipeline
        );
        emit_de_suite_event(
            &args.output,
            DeSuiteEvent {
                event: "model_error",
                timestamp: event_timestamp(),
                pipeline: &args.pipeline,
                run_kind: Some(agent_type),
                thread_id: None,
                phase: Some("preflight"),
                repair_status: None,
                pending_plan_revision: None,
                failure_summary: Some(&msg),
                error: Some(&msg),
                answer: None,
                plan: None,
                ok: Some(false),
                model_preflight: None,
                changed_files: None,
                validation: None,
            },
        );
        eprintln!("[skippr] ERROR: {msg}");
        std::process::exit(1);
    }

    eprintln!(
        "[skippr] model config: project={} tenant={} storage={:?}",
        resolved.scope.project_id, resolved.scope.tenant, resolved.storage.mode
    );

    let model_config_path = config_path(explicit_config);
    let thread_id = if args.no_resume {
        eprintln!("[skippr] not resuming previous thread (--no-resume)");
        None
    } else {
        match load_model_thread_for_pipeline(&resolved, &model_config_path, &args.pipeline).await {
            Ok(thread_id) => thread_id,
            Err(e) => {
                eprintln!("[skippr] WARNING: failed to load model thread: {e}");
                None
            }
        }
    };
    if let Some(ref tid) = thread_id {
        react_suite_data_engineer::metering::set_metering_thread_id(tid);
        eprintln!("[skippr] resuming thread {tid}");
        emit_de_suite_event(
            &args.output,
            DeSuiteEvent {
                event: "model_thread_resumed",
                timestamp: event_timestamp(),
                pipeline: &args.pipeline,
                run_kind: Some(agent_type),
                thread_id: Some(tid),
                phase: Some("resume"),
                repair_status: None,
                pending_plan_revision: None,
                failure_summary: None,
                error: None,
                answer: None,
                plan: None,
                ok: None,
                model_preflight: None,
                changed_files: None,
                validation: None,
            },
        );
    } else {
        eprintln!("[skippr] starting new modeling thread");
    }

    let status_cfg = resolved.clone();
    let run_thread_id = thread_id.clone();
    let dbt_output_path = args.dbt_output_path.clone().unwrap_or_else(|| {
        project_root_from_config_path(&model_config_path)
            .join(args.pipeline.as_str())
            .join("dbt")
    });
    let dbt_output_path = if dbt_output_path.is_absolute() {
        dbt_output_path
    } else {
        project_root_from_config_path(&model_config_path).join(dbt_output_path)
    };
    eprintln!("[skippr] dbt output path: {}", dbt_output_path.display());
    let dbt_diff_before = snapshot_dbt_diff_files(&dbt_output_path);
    std::env::set_var("SKIPPR_LOCAL_DBT_PROJECT_ROOT", &dbt_output_path);
    std::env::set_var("SKIPPR_LOCAL_IDE_ROOT", &dbt_output_path);
    let preflight_profiles_temp: Option<tempfile::TempDir> = {
        let preflight = run_direct_dbt_preflight(&resolved, Some(dbt_output_path.as_path()));
        let preflight_value = serde_json::to_value(&preflight.result).unwrap_or_else(|_| {
            serde_json::json!({
                "ok": false,
                "remediation": "Failed to serialize dbt preflight result."
            })
        });
        emit_de_suite_event(
            &args.output,
            DeSuiteEvent {
                event: "model_preflight",
                timestamp: event_timestamp(),
                pipeline: &args.pipeline,
                run_kind: Some(agent_type),
                thread_id: run_thread_id.as_deref(),
                phase: Some("preflight"),
                repair_status: None,
                pending_plan_revision: None,
                failure_summary: preflight.result.remediation.as_deref(),
                error: if preflight.result.ok {
                    None
                } else {
                    preflight.result.remediation.as_deref()
                },
                answer: None,
                plan: None,
                ok: Some(preflight.result.ok),
                model_preflight: Some(preflight_value),
                changed_files: None,
                validation: None,
            },
        );
        if !preflight.result.ok {
            if let Some(remediation) = preflight.result.remediation.as_deref() {
                eprintln!("[skippr] dbt preflight failed: {remediation}");
            }
            emit_de_suite_event(
                &args.output,
                DeSuiteEvent {
                    event: "model_error",
                    timestamp: event_timestamp(),
                    pipeline: &args.pipeline,
                    run_kind: Some(agent_type),
                    thread_id: run_thread_id.as_deref(),
                    phase: Some("preflight"),
                    repair_status: None,
                    pending_plan_revision: None,
                    failure_summary: preflight.result.remediation.as_deref(),
                    error: preflight.result.remediation.as_deref(),
                    answer: None,
                    plan: None,
                    ok: Some(false),
                    model_preflight: None,
                    changed_files: None,
                    validation: None,
                },
            );
            std::process::exit(1);
        }
        preflight.profiles_temp
    };
    let headless_prompt = Some(format!(
        "Build or update the complete local dbt project for pipeline `{pipeline}` using the phased data-engineer workflow. \
The local dbt project root is `{dbt_root}` and is the editable source of truth for authoring; phase file edits must land there so the IDE can show diffs. \
The Skippr config is `{config_path}`. Use the existing phased cleanse and gold workflow: discover sources of truth, design and review cleanse/silver, author and validate, then design and review gold models, author and validate, and finish with project status. \
Do not bypass phase validation/review gates. If dbt, warehouse, or source prerequisites block completion, persist the phase blocker with the concrete validation or source error.",
        pipeline = args.pipeline,
        dbt_root = dbt_output_path.display(),
        config_path = model_config_path.display()
    ));
    let _preflight_profiles_temp = preflight_profiles_temp;
    emit_de_suite_event(
        &args.output,
        DeSuiteEvent {
            event: "model_authoring_start",
            timestamp: event_timestamp(),
            pipeline: &args.pipeline,
            run_kind: Some(agent_type),
            thread_id: run_thread_id.as_deref(),
            phase: Some("authoring"),
            repair_status: None,
            pending_plan_revision: None,
            failure_summary: None,
            error: None,
            answer: None,
            plan: None,
            ok: None,
            model_preflight: None,
            changed_files: None,
            validation: None,
        },
    );
    eprintln!(
        "[skippr] starting headless data-engineer workflow ({agent_type} via {transport_agent_type})"
    );
    let headless = react_host::run_headless_detailed(
        resolved,
        react::run_engine::HeadlessRunOpts {
            log_level: log,
            verbose_debug: false,
            terminal: false,
            thread_id,
            suite_id: Some("data_engineer".to_string()),
            agent: transport_agent_type.to_string(),
            skip_logging_init: false,
            headless_prompt,
            stream_jsonl: false,
        },
    )
    .await;
    let exit_code = headless.exit_code;
    let status_thread_id = headless.thread_id.clone().or(run_thread_id);
    let dbt_diff_after = snapshot_dbt_diff_files(&dbt_output_path);
    let changed_dbt_files = changed_dbt_diff_files(&dbt_diff_before, &dbt_diff_after);
    let changed_dbt_files_value = if changed_dbt_files.is_empty() {
        None
    } else {
        Some(serde_json::Value::Array(changed_dbt_files.clone()))
    };
    if !changed_dbt_files.is_empty() {
        let count_kind = |kind: &str| {
            changed_dbt_files
                .iter()
                .filter(|file| {
                    file.get("change_kind").and_then(serde_json::Value::as_str) == Some(kind)
                })
                .count()
        };
        let sum_numeric = |field: &str| {
            changed_dbt_files
                .iter()
                .filter_map(|file| file.get(field).and_then(serde_json::Value::as_u64))
                .sum::<u64>() as usize
        };
        emit_de_suite_event(
            &args.output,
            DeSuiteEvent {
                event: "model_file_changed",
                timestamp: event_timestamp(),
                pipeline: &args.pipeline,
                run_kind: Some(agent_type),
                thread_id: status_thread_id.as_deref(),
                phase: Some("file_changes"),
                repair_status: None,
                pending_plan_revision: None,
                failure_summary: None,
                error: None,
                answer: None,
                plan: None,
                ok: Some(exit_code == 0),
                model_preflight: None,
                changed_files: changed_dbt_files_value.clone(),
                validation: None,
            },
        );
        eprintln!(
            "[skippr] model changed files: total={} created={} modified={} deleted={} +{} -{}",
            changed_dbt_files.len(),
            count_kind("created"),
            count_kind("modified"),
            count_kind("deleted"),
            sum_numeric("lines_added"),
            sum_numeric("lines_removed")
        );
    }
    eprintln!("[skippr] headless data-engineer workflow exited with code {exit_code}");
    if let Some(err) = headless.bootstrap_error.as_deref() {
        eprintln!("[skippr] data-engineer bootstrap failed before a thread was created: {err}");
    }
    if let Some(err) = headless.failure_summary.as_deref() {
        eprintln!("[skippr] data-engineer workflow failed: {err}");
    }
    let mut model_failure_summary: Option<String> = None;
    if let Some(tid) = status_thread_id.as_deref() {
        if let Err(e) =
            persist_model_thread_binding(&status_cfg, &model_config_path, &args.pipeline, tid).await
        {
            eprintln!("[skippr] WARNING: failed to persist model thread binding: {e}");
        }
        match load_model_thread_status(&status_cfg, tid).await {
            Ok(Some(status)) => {
                model_failure_summary = status.failure_brief.clone();
                emit_de_suite_event(
                    &args.output,
                    DeSuiteEvent {
                        event: "model_phase_changed",
                        timestamp: event_timestamp(),
                        pipeline: &args.pipeline,
                        run_kind: Some(agent_type),
                        thread_id: Some(tid),
                        phase: Some(status.current_phase.as_str()),
                        repair_status: status.last_evaluation.as_deref(),
                        pending_plan_revision: Some(status.pending_plan_revision),
                        failure_summary: status.failure_brief.as_deref(),
                        error: None,
                        answer: None,
                        plan: None,
                        ok: None,
                        model_preflight: None,
                        changed_files: changed_dbt_files_value.clone(),
                        validation: None,
                    },
                );
                eprintln!(
                    "[skippr] data-engineer thread status: current_phase={} last_evaluation={} failed={} pending_plan_revision={}",
                    status.current_phase,
                    status.last_evaluation.as_deref().unwrap_or("none"),
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
    } else if exit_code != 0
        && headless.bootstrap_error.is_none()
        && headless.failure_summary.is_none()
    {
        eprintln!(
            "[skippr] model run failed before the CLI received a thread id or failure summary; no stale latest-thread status was used."
        );
    }
    let final_failure_summary = headless
        .bootstrap_error
        .as_deref()
        .or(headless.failure_summary.as_deref())
        .or(model_failure_summary.as_deref());
    emit_de_suite_event(
        &args.output,
        DeSuiteEvent {
            event: if exit_code == 0 {
                "model_complete"
            } else {
                "model_error"
            },
            timestamp: event_timestamp(),
            pipeline: &args.pipeline,
            run_kind: Some(agent_type),
            thread_id: status_thread_id.as_deref(),
            phase: Some("complete"),
            repair_status: None,
            pending_plan_revision: None,
            failure_summary: final_failure_summary,
            error: final_failure_summary,
            answer: None,
            plan: None,
            ok: Some(exit_code == 0),
            model_preflight: None,
            changed_files: changed_dbt_files_value,
            validation: None,
        },
    );
    std::process::exit(exit_code);
}

async fn load_model_thread_status(
    cfg: &react_core::resolved_config::ReactResolvedConfig,
    thread_id: &str,
) -> Result<Option<react_suite_data_engineer::DataEngineerThreadStatus>, String> {
    let (storage, keyspace) = model_storage_handles(cfg).await?;
    let control = react_core::session::ControlStateStore::new(storage, cfg.scope.clone(), keyspace);
    react_suite_data_engineer::load_thread_status(&control, thread_id).await
}

async fn load_model_thread_for_pipeline(
    cfg: &react_core::resolved_config::ReactResolvedConfig,
    config_path: &Path,
    pipeline: &str,
) -> Result<Option<String>, String> {
    if let Some(thread_id) = load_bound_model_thread(cfg, config_path, pipeline).await? {
        return Ok(Some(thread_id));
    }

    let Some(thread_id) = find_latest_thread_for_resolved_config(cfg).await? else {
        eprintln!("[skippr] no previous model thread found for pipeline {pipeline}");
        return Ok(None);
    };
    match load_model_thread_status(cfg, &thread_id).await {
        Ok(Some(_)) => {
            eprintln!(
                "[skippr] resuming latest scoped model thread {} for pipeline {}",
                thread_id, pipeline
            );
            if let Err(e) =
                persist_model_thread_binding(cfg, config_path, pipeline, &thread_id).await
            {
                eprintln!("[skippr] WARNING: failed to persist model thread binding: {e}");
            }
            Ok(Some(thread_id))
        }
        Ok(None) => {
            eprintln!(
                "[skippr] latest scoped thread {thread_id} has no data-engineer status; starting fresh"
            );
            Ok(None)
        }
        Err(e) => Err(format!(
            "failed to validate latest scoped model thread {thread_id}: {e}"
        )),
    }
}

async fn load_bound_model_thread(
    cfg: &react_core::resolved_config::ReactResolvedConfig,
    config_path: &Path,
    pipeline: &str,
) -> Result<Option<String>, String> {
    let fingerprint = model_thread_config_fingerprint(cfg, config_path, pipeline);
    let (storage, keyspace) = model_storage_handles(cfg).await?;
    let key = model_thread_binding_key(keyspace.as_ref(), &cfg.scope, &fingerprint);
    let value = match react_core::storage::retry_get_json(storage.as_ref(), &key).await {
        Ok(value) => value,
        Err(e) if react_core::storage::is_storage_not_found_error(&e) => {
            eprintln!(
                "[skippr] no bound model thread for pipeline {pipeline}; checking latest scoped thread"
            );
            return Ok(None);
        }
        Err(e) => return Err(format!("failed to read model thread binding {key}: {e}")),
    };

    let binding: ModelThreadBinding = serde_json::from_value(value)
        .map_err(|e| format!("failed to parse model thread binding {key}: {e}"))?;
    if !model_thread_binding_matches(&binding, pipeline, &fingerprint) {
        eprintln!(
            "[skippr] model thread binding did not match requested pipeline/config; starting fresh"
        );
        return Ok(None);
    }
    if uuid::Uuid::parse_str(&binding.thread_id).is_err() {
        eprintln!("[skippr] model thread binding contains invalid thread id; starting fresh");
        return Ok(None);
    }
    match load_model_thread_status(cfg, &binding.thread_id).await {
        Ok(Some(_)) => {
            eprintln!(
                "[skippr] resuming bound model thread {} for pipeline {}",
                binding.thread_id, pipeline
            );
            Ok(Some(binding.thread_id))
        }
        Ok(None) => {
            eprintln!(
                "[skippr] bound model thread has no persisted data-engineer status; starting fresh"
            );
            Ok(None)
        }
        Err(e) => Err(format!(
            "failed to validate bound model thread {}: {e}",
            binding.thread_id
        )),
    }
}

async fn persist_model_thread_binding(
    cfg: &react_core::resolved_config::ReactResolvedConfig,
    config_path: &Path,
    pipeline: &str,
    thread_id: &str,
) -> Result<(), String> {
    let fingerprint = model_thread_config_fingerprint(cfg, config_path, pipeline);
    let (storage, keyspace) = model_storage_handles(cfg).await?;
    let key = model_thread_binding_key(keyspace.as_ref(), &cfg.scope, &fingerprint);
    let binding = ModelThreadBinding {
        schema_version: 1,
        pipeline: pipeline.to_string(),
        config_fingerprint: fingerprint,
        config_path: stable_config_path(config_path),
        thread_id: thread_id.to_string(),
        updated_at: chrono::Utc::now().to_rfc3339(),
    };
    let value = serde_json::to_value(binding)
        .map_err(|e| format!("failed to serialize model thread binding: {e}"))?;
    react_core::storage::retry_put_json(storage.as_ref(), &key, &value)
        .await
        .map_err(|e| format!("failed to write model thread binding {key}: {e}"))
}

async fn model_storage_handles(
    cfg: &react_core::resolved_config::ReactResolvedConfig,
) -> Result<
    (
        Arc<dyn react_core::storage::StorageAdapter>,
        Arc<dyn react_core::keyspace::Keyspace>,
    ),
    String,
> {
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
    Ok((storage, keyspace))
}

fn model_thread_binding_key(
    keyspace: &dyn react_core::keyspace::Keyspace,
    scope: &react_core::scope::RequestScope,
    fingerprint: &str,
) -> String {
    let filename = format!("{fingerprint}.json");
    keyspace.scoped_key(scope, &["model_thread_bindings", &filename])
}

fn model_thread_config_fingerprint(
    cfg: &react_core::resolved_config::ReactResolvedConfig,
    config_path: &Path,
    pipeline: &str,
) -> String {
    let input = format!(
        "model-thread-v1\0tenant={}\0workspace={}\0project={}\0pipeline={}\0config={}",
        cfg.scope.tenant,
        cfg.scope.workspace,
        cfg.scope.project_id,
        pipeline,
        stable_config_path(config_path)
    );
    let digest = sha2::Sha256::digest(input.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn stable_config_path(config_path: &Path) -> String {
    std::fs::canonicalize(config_path)
        .unwrap_or_else(|_| config_path.to_path_buf())
        .display()
        .to_string()
}

fn model_thread_binding_matches(
    binding: &ModelThreadBinding,
    pipeline: &str,
    config_fingerprint: &str,
) -> bool {
    binding.schema_version == 1
        && binding.pipeline == pipeline
        && binding.config_fingerprint == config_fingerprint
        && !binding.thread_id.trim().is_empty()
}

async fn cmd_feedback(
    pipeline: PipelineName,
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
    let project = pipeline.as_str();
    let srv_creds = match load_reset_server_credentials().await {
        Ok(creds) => creds,
        Err(e) => {
            eprintln!("[skippr] ERROR: {e}");
            std::process::exit(1);
        }
    };
    let resolved_cfg = match resolve_feedback_runtime_config(explicit_config, &pipeline, &srv_creds)
    {
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

#[cfg(test)]
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
            ));
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
    let (bucket, s3_creds) = authenticated_storage_bucket_and_credentials(srv_creds)?;
    let storage = react_s3_storage_from_credentials(&bucket, &s3_creds).await;
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
    explicit_config: &Option<PathBuf>,
    pipeline: &PipelineName,
    srv_creds: &api_client::CredentialsResponse,
) -> Result<react_core::resolved_config::ReactResolvedConfig, String> {
    let engine_cfg = load_cli_execution_config(explicit_config)?;
    validate_pipeline_exists(&engine_cfg, pipeline.as_str())?;
    let mut internal_file = react_config_from_pipeline_config(&engine_cfg, pipeline.as_str())?;
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

pub(crate) fn list_threads_in_local_storage(
    storage_root: &std::path::Path,
    scope: &react_core::scope::RequestScope,
) -> Result<Vec<(String, std::time::SystemTime)>, String> {
    let keyspace = react_core::keyspace::LocalKeyspace::new(storage_root.display().to_string());
    let threads_dir = storage_root.join(keyspace.threads_prefix(scope).trim_end_matches('/'));
    let entries = match std::fs::read_dir(&threads_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(format!(
                "failed to read local threads directory {}: {e}",
                threads_dir.display()
            ));
        }
    };

    let mut out: Vec<(String, std::time::SystemTime)> = Vec::new();
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
        out.push((thread_id, modified));
    }
    Ok(out)
}

async fn list_all_threads_in_s3_storage(
    cfg: &react_core::resolved_config::ReactResolvedConfig,
) -> Result<Vec<(String, chrono::DateTime<chrono::Utc>)>, String> {
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

    let mut best: HashMap<String, chrono::DateTime<chrono::Utc>> = HashMap::new();
    for object in objects {
        let Some(thread_id) = primary_thread_id_from_key(&object.key, &prefix) else {
            continue;
        };
        let Some(last_modified) = object.last_modified else {
            continue;
        };
        best.entry(thread_id)
            .and_modify(|t| {
                if last_modified > *t {
                    *t = last_modified;
                }
            })
            .or_insert(last_modified);
    }

    let mut v: Vec<_> = best.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    Ok(v)
}

pub(crate) async fn list_threads_for_resolved_config(
    cfg: &react_core::resolved_config::ReactResolvedConfig,
) -> Result<Vec<(String, chrono::DateTime<chrono::Utc>)>, String> {
    match cfg.storage.mode {
        react_core::resolved_config::StorageMode::Local => {
            let root = cfg
                .storage
                .path
                .as_ref()
                .ok_or_else(|| "missing storage.path for local mode".to_string())?;
            let mut rows = list_threads_in_local_storage(std::path::Path::new(root), &cfg.scope)?;
            rows.sort_by(|a, b| b.1.cmp(&a.1));
            Ok(rows
                .into_iter()
                .map(|(tid, st)| (tid, chrono::DateTime::<chrono::Utc>::from(st)))
                .collect())
        }
        react_core::resolved_config::StorageMode::S3 => list_all_threads_in_s3_storage(cfg).await,
    }
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
    ensure_project_working_dir(&cli.config);

    match cli.cmd {
        Cmd::Init { name, output } => cmd_init(&name, &cli.config, &output).await,
        Cmd::Reset(args) => cmd_reset(&cli.config, args).await,
        Cmd::Connect { output, target } => match target {
            ConnectTarget::Warehouse { kind } => cmd_connect_warehouse(kind, &cli.config, &output),
            ConnectTarget::Source { kind } => cmd_connect_source(kind, &cli.config, &output),
        },
        Cmd::Doctor { output } => cmd_doctor(&cli.config, &output),
        Cmd::Config { action } => match action {
            ConfigAction::Schema { output } => cmd_config_schema(&output),
            ConfigAction::Show { output } => cmd_config_show(&cli.config, &output),
        },
        Cmd::Discover(args) => cmd_discover(cli.log, &cli.config, args).await,
        Cmd::Metadata { action } => cmd_metadata(cli.log, &cli.config, action).await,
        Cmd::Sync(args) => cmd_sync(cli.log, &cli.config, args).await,
        Cmd::Model(args) => cmd_model(cli.log, &cli.config, args).await,
        Cmd::Test { action } => match action {
            test_cmd::TestSubcommand::List(args) => {
                if let Err(e) = test_cmd::cmd_test_list(cli.log, &cli.config, args).await {
                    eprintln!("[skippr] ERROR: {e}");
                    std::process::exit(1);
                }
            }
            test_cmd::TestSubcommand::Run(args) => {
                if let Err(e) = test_cmd::cmd_test_run(cli.log, &cli.config, args).await {
                    eprintln!("[skippr] ERROR: {e}");
                    std::process::exit(1);
                }
            }
        },
        Cmd::Dbt { action } => match action {
            dbt_cmd::DbtSubcommand::CompileSql(args) => {
                if let Err(e) = dbt_cmd::cmd_dbt_compile_sql(cli.log, &cli.config, args).await {
                    eprintln!("[skippr] ERROR: {e}");
                    std::process::exit(1);
                }
            }
        },
        Cmd::Ask(args) => cmd_ask(cli.log, &cli.config, args).await,
        Cmd::Plan(args) => cmd_plan(cli.log, &cli.config, args).await,
        Cmd::Query(args) => cmd_query(cli.log, &cli.config, args).await,
        Cmd::Lineage { action } => cmd_lineage(cli.log, &cli.config, action).await,
        Cmd::Chat { action } => chat_cmd::run_chat(cli.log, &cli.config, action).await,
        Cmd::Thread { action } => match action {
            ThreadAction::Resolve { pipeline, output } => {
                cmd_thread_resolve(cli.log, &cli.config, pipeline, &output).await;
            }
        },
        Cmd::Feedback {
            pipeline,
            good,
            bad,
            comment,
            no_diagnostics,
        } => cmd_feedback(pipeline, good, bad, comment, !no_diagnostics, &cli.config).await,
        Cmd::Vector { action } => match action {
            VectorAction::IngestDocs {
                pipeline,
                vector_source,
                src_path,
                chunk_chars,
                chunk_overlap,
                include_glob,
                exclude_glob,
                dry_run,
                output,
            } => {
                vector_ingest_docs::run_vector_ingest_docs(
                    vector_ingest_docs::VectorIngestDocsArgs {
                        config: cli.config.clone(),
                        pipeline,
                        vector_source,
                        src_path,
                        chunk_chars,
                        chunk_overlap,
                        include_glob,
                        exclude_glob,
                        dry_run,
                        output,
                    },
                )
                .await;
            }
        },
        Cmd::User { output, action } => match action {
            UserAction::Login => cmd_user_login(&output).await,
            UserAction::Logout => cmd_user_logout(&output),
            UserAction::Account => cmd_user_account(&output).await,
            UserAction::BuyCredits { amount } => cmd_user_buy_credits(amount, &output).await,
            UserAction::CreateApiKey { name } => cmd_user_create_api_key(&name, &output).await,
            UserAction::RevokeApiKey { key_id } => cmd_user_revoke_api_key(&key_id, &output).await,
            UserAction::ListApiKeys => cmd_user_list_api_keys(&output).await,
        },
        Cmd::Runs { action } => match action {
            RunsAction::ReleaseLock { workspace } => {
                workspace_run_lock::release_workspace_heavy_lock_best_effort(&workspace).await;
            }
        },
    }
}

async fn cmd_user_login(output: &str) {
    if auth::load_credentials().is_some() {
        if is_json_output(output) {
            print_json(
                &serde_json::json!({"ok": true, "authenticated": true, "message": "already logged in"}),
            );
            return;
        }
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
                    if is_json_output(output) {
                        print_json(
                            &serde_json::json!({"ok": true, "authenticated": true, "email": email}),
                        );
                    } else {
                        println!();
                        println!("  Logged in successfully.");
                        println!();
                        println!("  Next steps:");
                        println!("    skippr user account       — view balance");
                        println!("    skippr user buy-credits   — add funds");
                        println!("    skippr discover/sync/model — start a pipeline");
                        println!();
                    }
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

fn cmd_user_logout(output: &str) {
    auth::clear_credentials();
    if is_json_output(output) {
        print_json(&serde_json::json!({"ok": true, "authenticated": false}));
    } else {
        println!("Logged out. Local credentials removed.");
    }
}

pub(crate) async fn refresh_user_credentials_or_exit(
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

pub(crate) fn create_token_provider(
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
    let (creds, authenticated_with_api_key) = match load_cli_auth_credentials().await {
        Ok(value) => value,
        Err(err) => {
            eprintln!("{}", err);
            std::process::exit(1);
        }
    };
    let base_url = auth::auth_base_url();
    let tokens = create_token_provider(&creds);
    let client = api_client::ApiClient::authenticated(&base_url, tokens);
    if let Err(e) = ensure_eula_accepted(&client, !authenticated_with_api_key).await {
        eprintln!("{}", e);
        std::process::exit(1);
    }
    client
}

pub(crate) async fn ensure_eula_accepted(
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

async fn cmd_user_account(output: &str) {
    let client = authenticated_api_client().await;
    match client.get_account().await {
        Ok(account) => {
            if is_json_output(output) {
                print_json(&serde_json::json!({
                    "ok": true,
                    "account": account,
                    "plan": account.profile.plan,
                    "balance": account.balance.balance,
                    "monthly_cost_est": account.monthly_cost_est,
                    "recent_usage": account.recent_usage,
                    "eula_required_version": SKIPPR_EULA_VERSION,
                    "eula_url": SKIPPR_EULA_URL
                }));
                return;
            }
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

async fn cmd_user_buy_credits(amount: Option<f64>, output: &str) {
    let amount = match amount {
        Some(a) => a,
        None => {
            if is_json_output(output) {
                print_json(&serde_json::json!({
                    "ok": false,
                    "error": "amount_required",
                    "minimum": 5.0,
                    "maximum": 10000.0
                }));
                return;
            }
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
            if is_json_output(output) {
                print_json(&serde_json::json!({
                    "ok": true,
                    "amount": amount,
                    "checkout_url": url
                }));
                return;
            }
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

async fn cmd_user_create_api_key(name: &str, output: &str) {
    let client = authenticated_api_client().await;
    match client.create_api_key(name).await {
        Ok(key) => {
            if is_json_output(output) {
                print_json(&serde_json::json!({"ok": true, "api_key": key}));
                return;
            }
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

async fn cmd_user_revoke_api_key(key_id: &str, output: &str) {
    let client = authenticated_api_client().await;
    match client.revoke_api_key(key_id).await {
        Ok(()) => {
            if is_json_output(output) {
                print_json(&serde_json::json!({"ok": true, "key_id": key_id, "revoked": true}));
            } else {
                println!("API key {} revoked.", key_id);
            }
        }
        Err(e) => {
            eprintln!("Failed to revoke API key: {}", e);
            std::process::exit(1);
        }
    }
}

async fn cmd_user_list_api_keys(output: &str) {
    let client = authenticated_api_client().await;
    match client.list_api_keys().await {
        Ok(keys) => {
            if is_json_output(output) {
                print_json(&serde_json::json!({"ok": true, "api_keys": keys}));
                return;
            }
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

pub(crate) const LOW_BALANCE_USD_THRESHOLD: f64 = 5.0;

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

async fn delete_storage_prefix(
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

async fn delete_skipprd_storage_prefix(
    storage: &std::sync::Arc<dyn skipprd::adapters::storage::StorageAdapter>,
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
# Copy this file to `.env` in the same folder as skippr.yml (or merge into an existing `.env`).
# Skippr loads `.env` then `.env.local` before resolving ${VAR} placeholders in skippr.yml.
# Variables already set in your shell are not overwritten by `.env` (use `.env.local` to force overrides).

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
        cmd_init("test-project", &Some(config.clone()), "json").await;
        assert!(config.exists());
        let contents = fs::read_to_string(&config).unwrap();
        assert!(contents.contains("test-project"));
        assert!(!contents.contains("tenant:"));
        assert!(!contents.contains("react:"));
        assert!(!contents.contains("dbt:"));
    }

    #[tokio::test]
    async fn init_is_idempotent_when_already_initialised() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("skippr.yaml");
        cmd_init("my-pipeline", &Some(config.clone()), "json").await;
        assert!(config.exists());
        let original = fs::read_to_string(&config).unwrap();

        // Second init should not fail or change anything
        cmd_init("my-pipeline", &Some(config.clone()), "json").await;
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

    /// `config show` must not require `${VAR}` to be set; it only lists structural keys for the IDE.
    #[test]
    fn config_show_introspection_loads_yaml_with_unset_env_scalar_refs() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("skippr.yml");
        const UNSET: &str = "SKIPPR_CLI_TEST_UNSET_ENV_REF_9f3e2a1b";
        let yaml = format!(
            r#"
skippr:
  workspace: test_ws
pipelines:
  pl1:
    data_source: data_sources.src1
    data_sink: data_sinks.sf1
data_sources:
  src1:
    Mssql:
      connection_string: server=tcp:127.0.0.1;database=db
      tables: [dbo.t]
data_sinks:
  sf1:
    Snowflake:
      account: ${{{unset}}}
      user: u
      database: d
      schema: s
      warehouse: wh
      role: r
      private_key_path: /tmp/k
"#,
            unset = UNSET
        );
        fs::write(&config, yaml).unwrap();
        std::env::remove_var(UNSET);

        let mut cfg = load_engine_config(&Some(config.clone())).expect("parse yaml");
        warn_and_normalize_legacy_cli_config(&mut cfg).expect("normalize");

        let pipelines = yaml_mapping_keys(cfg.get("pipelines"));
        assert!(pipelines.contains(&"pl1".to_string()));
        let sinks = yaml_mapping_keys(cfg.get("data_sinks"));
        assert!(sinks.contains(&"sf1".to_string()));

        let mut missing = Vec::new();
        collect_missing_env_scalar_messages_yaml(&cfg, "", &mut missing);
        assert!(
            missing.iter().any(|m| m.contains(UNSET)),
            "expected unset env diagnostic, got: {:?}",
            missing
        );
    }

    #[test]
    fn config_show_connections_include_redacted_details_and_refs() {
        let cfg: serde_yaml::Value = serde_yaml::from_str(
            r#"
skippr:
  workspace: test_ws
pipelines:
  pl1:
    data_source: data_sources.src1
    data_sink: data_sinks.sf1
data_sources:
  src1:
    Mssql:
      connection_string: server=tcp:127.0.0.1;database=db
      tables: [dbo.t]
data_sinks:
  sf1:
    Snowflake:
      account: acct
      user: user
      password: super-secret
      database: analytics
    schema_sink: schema_sinks.glue1
schema_sinks:
  glue1:
    Glue:
      glue_database_name: analytics
"#,
        )
        .expect("yaml");

        let connections = config_show_connections(&cfg);
        assert_eq!(connections.sources[0].name, "src1");
        assert_eq!(connections.sources[0].provider.as_deref(), Some("Mssql"));
        assert_eq!(connections.sources[0].pipelines, vec!["pl1"]);

        let sink = &connections.sinks[0];
        assert_eq!(sink.name, "sf1");
        assert_eq!(sink.provider.as_deref(), Some("Snowflake"));
        assert!(sink.supports_sql);
        assert_eq!(sink.schema_sink.as_deref(), Some("glue1"));
        assert_eq!(sink.pipelines, vec!["pl1"]);
        let password = sink
            .fields
            .iter()
            .find(|field| field.name == "password")
            .expect("password field");
        assert!(password.secret);
        assert_eq!(password.value, "********");

        let schema_sink = &connections.schema_sinks[0];
        assert_eq!(schema_sink.name, "glue1");
        assert_eq!(schema_sink.linked_sinks, vec!["sf1"]);
        assert_eq!(schema_sink.pipelines, vec!["pl1"]);
    }

    #[test]
    fn react_config_derives_snowflake_warehouse_from_pipeline() {
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

        let internal = react_config_from_pipeline_config(&cfg, "cursor_semantic_validation")
            .expect("internal config");
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
        assert_eq!(
            providers
                .get("el")
                .and_then(|el| el.get("skippr_input"))
                .and_then(|input| input.get("kind"))
                .and_then(|kind| kind.as_str()),
            Some("mssql")
        );
        assert_eq!(
            providers
                .get("el")
                .and_then(|el| el.get("skippr_input"))
                .and_then(|input| input.get("connection_string"))
                .and_then(|connection_string| connection_string.as_str()),
            Some("server=tcp:127.0.0.1,1433;database=testdb")
        );
        assert_eq!(
            providers
                .get("dbt")
                .and_then(|dbt| dbt.get("naming"))
                .and_then(|naming| naming.get("target_schema"))
                .and_then(|schema| schema.as_str()),
            Some("cursor_semantic_validation")
        );
    }

    #[test]
    fn react_config_translates_google_analytics_for_lineage() {
        let cfg: serde_yaml::Value = serde_yaml::from_str(
            r#"
skippr:
  workspace: default
pipelines:
  picnic_ga4:
    data_source: data_sources.picnic_ga4
    data_sink: data_sinks.picnic
data_sources:
  picnic_ga4:
    GoogleAnalytics:
      property_id: "123456789"
      start_date: "2024-01-01"
      oauth_token_url: https://oauth2.googleapis.com/token
      oauth_client_id: ${GA4_OAUTH_CLIENT_ID}
      oauth_client_secret: ${GA4_OAUTH_CLIENT_SECRET}
      oauth_refresh_token: ${GA4_OAUTH_REFRESH_TOKEN}
data_sinks:
  picnic:
    schema_sink: schema_sinks.glue_picnic
    Athena:
      athena_workgroup_name: picnic
      athena_results_s3_bucket: s3://example-athena-results/
      s3_bucket: example-datalake
schema_sinks:
  glue_picnic:
    Glue:
      glue_database_name: picnic
"#,
        )
        .expect("yaml");

        let internal =
            react_config_from_pipeline_config(&cfg, "picnic_ga4").expect("internal config");
        let providers = internal.providers.expect("providers");
        assert_eq!(
            providers
                .get("el")
                .and_then(|el| el.get("skippr_input"))
                .and_then(|input| input.get("kind"))
                .and_then(|kind| kind.as_str()),
            Some("google_analytics")
        );
        assert_eq!(
            providers
                .get("el")
                .and_then(|el| el.get("skippr_input"))
                .and_then(|input| input.get("property_id"))
                .and_then(|id| id.as_str()),
            Some("123456789")
        );
    }

    #[test]
    fn react_config_derives_athena_schema_from_pipeline_schema_sink() {
        let cfg: serde_yaml::Value = serde_yaml::from_str(
            r#"
skippr:
  workspace: picnic
pipelines:
  picnic:
    data_source: data_sources.picnic
    data_sink: data_sinks.picnic
data_sources:
  picnic:
    S3:
      s3_bucket: circles-analytics-prod
data_sinks:
  picnic:
    schema_sink: schema_sinks.glue_picnic
    Athena:
      athena_workgroup_name: picnic
      athena_results_s3_bucket: asgsdag-datalake
      s3_bucket: asgsdag-datalake
      s3_prefix: /datalake
schema_sinks:
  glue_picnic:
    Glue:
      glue_database_name: picnic
"#,
        )
        .expect("yaml");

        let internal = react_config_from_pipeline_config(&cfg, "picnic").expect("internal config");
        let providers = internal.providers.expect("providers");
        let wh = providers
            .get("warehouse")
            .and_then(|v| v.as_object())
            .expect("warehouse object");

        assert_eq!(wh.get("kind").and_then(|v| v.as_str()), Some("athena"));
        assert_eq!(wh.get("schema").and_then(|v| v.as_str()), Some("picnic"));
        assert_eq!(wh.get("workgroup").and_then(|v| v.as_str()), Some("picnic"));
        assert_eq!(
            wh.get("result_s3").and_then(|v| v.as_str()),
            Some("s3://asgsdag-datalake/")
        );
        assert_eq!(
            providers
                .get("el")
                .and_then(|el| el.get("skippr_input"))
                .and_then(|input| input.get("kind"))
                .and_then(|kind| kind.as_str()),
            Some("s3")
        );
        assert_eq!(
            providers
                .get("el")
                .and_then(|el| el.get("skippr_input"))
                .and_then(|input| input.get("s3_bucket"))
                .and_then(|bucket| bucket.as_str()),
            Some("circles-analytics-prod")
        );
    }

    #[test]
    fn react_config_pipeline_errors_are_actionable() {
        let missing_pipeline: serde_yaml::Value = serde_yaml::from_str(
            r#"
skippr:
  workspace: demo
pipelines: {}
data_sinks: {}
"#,
        )
        .expect("yaml");
        let err = react_config_from_pipeline_config(&missing_pipeline, "missing")
            .expect_err("missing pipeline");
        assert!(err.contains("does not define pipeline"));

        let missing_sink: serde_yaml::Value = serde_yaml::from_str(
            r#"
skippr:
  workspace: demo
pipelines:
  demo:
    data_sink: data_sinks.warehouse
data_sinks: {}
"#,
        )
        .expect("yaml");
        let err =
            react_config_from_pipeline_config(&missing_sink, "demo").expect_err("missing sink");
        assert!(err.contains("does not define data sink 'warehouse'"));

        let missing_schema_sink: serde_yaml::Value = serde_yaml::from_str(
            r#"
skippr:
  workspace: demo
pipelines:
  demo:
    data_sink: data_sinks.warehouse
data_sinks:
  warehouse:
    schema_sink: schema_sinks.glue_missing
    Athena:
      athena_workgroup_name: demo
"#,
        )
        .expect("yaml");
        let err = react_config_from_pipeline_config(&missing_schema_sink, "demo")
            .expect_err("missing schema sink");
        assert!(err.contains("references unknown schema_sinks.glue_missing"));

        let unsupported: serde_yaml::Value = serde_yaml::from_str(
            r#"
skippr:
  workspace: demo
pipelines:
  demo:
    data_sink: data_sinks.warehouse
data_sinks:
  warehouse:
    File:
      path: /tmp/out
"#,
        )
        .expect("yaml");
        let err =
            react_config_from_pipeline_config(&unsupported, "demo").expect_err("unsupported sink");
        assert!(err.contains("not supported by `skippr model`"));
    }

    #[test]
    fn cli_config_rejects_react_section() {
        let mut cfg: serde_yaml::Value = serde_yaml::from_str(
            r#"
skippr:
  workspace: demo
react:
  providers: {}
"#,
        )
        .expect("yaml");

        let err = warn_and_normalize_legacy_cli_config(&mut cfg).expect_err("react rejected");

        assert!(err.contains("react:"));
        assert!(err.contains("skippr model --pipeline"));
    }

    #[test]
    fn cli_config_warns_and_drops_legacy_tenant_and_dbt() {
        let mut cfg: serde_yaml::Value = serde_yaml::from_str(
            r#"
skippr:
  workspace: demo
  tenant: old-tenant
dbt:
  target: prod
"#,
        )
        .expect("yaml");

        warn_and_normalize_legacy_cli_config(&mut cfg).expect("normalize legacy keys");

        assert!(cfg.get("dbt").is_none());
        assert!(cfg
            .get("skippr")
            .and_then(|skippr| skippr.get("tenant"))
            .is_none());
    }

    #[test]
    fn raw_config_save_path_does_not_expand_env_refs() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("skippr.yaml");
        fs::write(
            &config,
            r#"
skippr:
  workspace: demo
pipelines: {}
data_sources:
  source:
    Mssql:
      connection_string: ${SKIPPR_TEST_RAW_SECRET}
"#,
        )
        .unwrap();
        std::env::set_var("SKIPPR_TEST_RAW_SECRET", "should-not-be-written");

        let cfg = load_cli_raw_config_for_save(&Some(config)).expect("raw config");

        assert_eq!(
            cfg.get("data_sources")
                .and_then(|sources| sources.get("source"))
                .and_then(|source| source.get("Mssql"))
                .and_then(|mssql| mssql.get("connection_string"))
                .and_then(|value| value.as_str()),
            Some("${SKIPPR_TEST_RAW_SECRET}")
        );

        std::env::remove_var("SKIPPR_TEST_RAW_SECRET");
    }

    #[test]
    fn load_resolved_engine_config_interpolates_from_dot_env() {
        const VAR: &str = "SKIPPR_CLI_DOTENV_INTERP_TEST";
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("skippr.yml");
        std::fs::write(
            dir.path().join(".env"),
            format!("{VAR}=secret-from-env-file\n"),
        )
        .unwrap();
        std::fs::write(
            &config,
            format!(
                r#"
skippr:
  workspace: demo
pipelines: {{}}
data_sources:
  source:
    Mssql:
      connection_string: ${{{VAR}}}
"#
            ),
        )
        .unwrap();

        std::env::remove_var(VAR);
        let resolved = load_resolved_engine_config(&Some(config.clone())).expect("resolved");
        std::env::remove_var(VAR);

        let cs = resolved
            .get("data_sources")
            .and_then(|s| s.get("source"))
            .and_then(|s| s.get("Mssql"))
            .and_then(|m| m.get("connection_string"))
            .and_then(|v| v.as_str())
            .expect("connection_string");
        assert_eq!(cs, "secret-from-env-file");
    }

    #[test]
    fn load_resolved_engine_config_dot_env_fills_empty_process_env() {
        const VAR: &str = "SKIPPR_CLI_DOTENV_EMPTY_ENV_TEST";
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("skippr.yml");
        std::fs::write(
            dir.path().join(".env"),
            format!("{VAR}=secret-from-env-file\n"),
        )
        .unwrap();
        std::fs::write(
            &config,
            format!(
                r#"
skippr:
  workspace: demo
pipelines: {{}}
data_sources:
  source:
    Mssql:
      connection_string: ${{{VAR}}}
"#
            ),
        )
        .unwrap();

        std::env::set_var(VAR, "");
        let resolved = load_resolved_engine_config(&Some(config.clone())).expect("resolved");
        std::env::remove_var(VAR);

        let cs = resolved
            .get("data_sources")
            .and_then(|s| s.get("source"))
            .and_then(|s| s.get("Mssql"))
            .and_then(|m| m.get("connection_string"))
            .and_then(|v| v.as_str())
            .expect("connection_string");
        assert_eq!(cs, "secret-from-env-file");
    }

    #[test]
    fn load_resolved_engine_config_dot_env_local_overrides_dot_env() {
        const VAR: &str = "SKIPPR_CLI_DOTENV_LOCAL_TEST";
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("skippr.yml");
        std::fs::write(dir.path().join(".env"), format!("{VAR}=from-dot-env\n")).unwrap();
        std::fs::write(dir.path().join(".env.local"), format!("{VAR}=from-local\n")).unwrap();
        std::fs::write(
            &config,
            format!(
                r#"
skippr:
  workspace: demo
pipelines: {{}}
data_sources:
  source:
    Mssql:
      connection_string: ${{{VAR}}}
"#
            ),
        )
        .unwrap();

        std::env::remove_var(VAR);
        let resolved = load_resolved_engine_config(&Some(config)).expect("resolved");
        std::env::remove_var(VAR);

        let cs = resolved
            .get("data_sources")
            .and_then(|s| s.get("source"))
            .and_then(|s| s.get("Mssql"))
            .and_then(|m| m.get("connection_string"))
            .and_then(|v| v.as_str())
            .expect("connection_string");
        assert_eq!(cs, "from-local");
    }

    #[test]
    fn sync_and_discover_require_pipeline() {
        let sync_err = Cli::try_parse_from(["skippr", "sync"]).expect_err("missing pipeline");
        assert!(sync_err.to_string().contains("--pipeline"));

        let discover_err =
            Cli::try_parse_from(["skippr", "discover"]).expect_err("missing pipeline");
        assert!(discover_err.to_string().contains("--pipeline"));
    }

    #[test]
    fn query_sql_accepts_leading_line_comment() {
        let sql = "-- skippr-plan-spec-digest: abc\nselect 1";
        let cli = Cli::try_parse_from([
            "skippr",
            "query",
            "--pipeline",
            "bike_hire",
            "--sql",
            sql,
            "--output",
            "json",
        ])
        .expect("parse query with leading SQL comment");

        match cli.cmd {
            Cmd::Query(args) => assert_eq!(args.sql, sql),
            _ => panic!("expected query command"),
        }
    }

    #[test]
    fn model_requires_pipeline_and_rejects_data_sink() {
        let err = Cli::try_parse_from(["skippr", "model"]).expect_err("missing pipeline");
        assert!(err.to_string().contains("--pipeline"));

        let err = Cli::try_parse_from(["skippr", "model", "--data-sink", "warehouse"])
            .expect_err("removed data sink flag");
        assert!(err.to_string().contains("--data-sink"));
    }

    fn test_model_resolved_config(
        storage_root: &Path,
        project: &str,
    ) -> react_core::resolved_config::ReactResolvedConfig {
        react_core::resolved_config::ReactResolvedConfig {
            server: react_core::resolved_config::ServerResolved { port: 0 },
            storage: react_core::resolved_config::StorageResolved {
                mode: react_core::resolved_config::StorageMode::Local,
                bucket: None,
                path: Some(storage_root.display().to_string()),
                s3_credentials: None,
            },
            scope: react_core::scope::RequestScope::parse("_", "dev", project)
                .expect("valid test scope"),
            llm: react_core::resolved_config::LlmResolved::default(),
            suite_config: serde_json::json!({}),
        }
    }

    fn write_model_thread_with_status(storage_root: &Path, project: &str, thread_id: &str) {
        let threads_dir = storage_root.join(format!("_/dev/{project}/threads"));
        fs::create_dir_all(&threads_dir).unwrap();
        fs::write(threads_dir.join(format!("{thread_id}.json")), "{}").unwrap();

        let state_dir = storage_root.join(format!("_/dev/{project}/state/{thread_id}"));
        fs::create_dir_all(&state_dir).unwrap();
        let control = serde_json::json!({
            "schema_version": 1,
            "suite_id": "data_engineer",
            "payload": {
                "schema_version": 2,
                "phase": {
                    "current_phase": "model_plan"
                }
            }
        });
        fs::write(
            state_dir.join("control.json"),
            serde_json::to_vec(&control).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn model_thread_identity_is_pipeline_and_config_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("skippr.yml");
        fs::write(&config, "skippr: {}\n").unwrap();
        let cfg = test_model_resolved_config(dir.path(), "bank");

        let bank_fingerprint = model_thread_config_fingerprint(&cfg, &config, "bank");
        let bike_fingerprint = model_thread_config_fingerprint(&cfg, &config, "bike_hire");
        assert_ne!(bank_fingerprint, bike_fingerprint);

        let binding = ModelThreadBinding {
            schema_version: 1,
            pipeline: "bike_hire".to_string(),
            config_fingerprint: bike_fingerprint,
            config_path: stable_config_path(&config),
            thread_id: "11111111-1111-1111-1111-111111111111".to_string(),
            updated_at: "2026-05-19T00:00:00Z".to_string(),
        };

        assert!(
            !model_thread_binding_matches(&binding, "bank", &bank_fingerprint),
            "a bike_hire model thread binding must not satisfy a bank model run"
        );
    }

    #[tokio::test]
    async fn model_resume_uses_latest_thread_in_requested_pipeline_scope() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("skippr.yml");
        fs::write(&config, "skippr: {}\n").unwrap();
        write_model_thread_with_status(dir.path(), "bank", "22222222-2222-2222-2222-222222222222");
        write_model_thread_with_status(
            dir.path(),
            "bike_hire",
            "33333333-3333-3333-3333-333333333333",
        );

        let cfg = test_model_resolved_config(dir.path(), "bank");
        let thread = load_model_thread_for_pipeline(&cfg, &config, "bank")
            .await
            .expect("model thread lookup");

        assert!(
            matches!(
                thread.as_deref(),
                Some("22222222-2222-2222-2222-222222222222")
            ),
            "model runs must resume only the newest thread under the requested pipeline scope"
        );
    }

    #[test]
    fn init_rejects_legacy_reset_flag() {
        let err =
            Cli::try_parse_from(["skippr", "init", "demo", "--reset"]).expect_err("legacy reset");
        assert!(err.to_string().contains("--reset"));
    }

    #[test]
    fn reset_cli_accepts_pipeline_and_yes() {
        let cli = Cli::try_parse_from(["skippr", "reset", "--pipeline", "orders", "--yes"])
            .expect("parse reset");
        match cli.cmd {
            Cmd::Reset(args) => {
                assert_eq!(args.pipeline, "orders");
                assert!(args.yes);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn chat_send_accepts_optional_pipeline() {
        let cli = Cli::try_parse_from([
            "skippr",
            "chat",
            "send",
            "--mode",
            "ask",
            "--message",
            "status?",
        ])
        .expect("chat send without pipeline should parse");
        match cli.cmd {
            Cmd::Chat {
                action: chat_cmd::ChatAction::Send(args),
            } => {
                assert!(args.pipeline.is_none());
                assert_eq!(args.mode, chat_cmd::ChatModeCli::Ask);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn feedback_requires_pipeline() {
        let err = Cli::try_parse_from(["skippr", "feedback", "--good", "--comment", "nice"])
            .expect_err("feedback without pipeline must not parse");
        assert!(err.to_string().contains("--pipeline"));
    }

    fn test_credentials() -> api_client::CredentialsResponse {
        api_client::CredentialsResponse {
            credentials: api_client::StsCreds {
                access_key_id: "ak".to_string(),
                secret_access_key: "sk".to_string(),
                session_token: "tok".to_string(),
                expiration: "never".to_string(),
            },
            bucket: "model-bucket".to_string(),
            tenant_id: "auth-tenant".to_string(),
            llm_api_key: String::new(),
            accounting_url: String::new(),
            knowledge_credentials: None,
            public_vectors_bucket: None,
        }
    }

    #[test]
    fn derive_pipeline_reset_target_defaults_to_project_skippr_dir() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("skippr.yaml");
        fs::write(
            &config,
            r#"
skippr:
  workspace: analytics
pipelines:
  orders:
    data_source: data_sources.src
    data_sink: data_sinks.sink
"#,
        )
        .unwrap();
        let cfg = load_resolved_engine_config(&Some(config.clone())).expect("config");

        let target =
            derive_pipeline_reset_target(&cfg, &config, "orders", &test_credentials()).unwrap();

        assert_eq!(
            target.local_runtime_dir,
            dir.path().join(".skippr/auth-tenant/orders")
        );
    }

    #[test]
    fn derive_pipeline_reset_target_uses_selected_pipeline_only() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("skippr.yaml");
        fs::write(
            &config,
            r#"
skippr:
  workspace: analytics
  skippr_s3_bucket: runtime-bucket
pipelines:
  orders:
    data_dir: state
  customers:
    data_dir: other-state
"#,
        )
        .unwrap();
        let cfg = load_resolved_engine_config(&Some(config.clone())).expect("config");

        let target =
            derive_pipeline_reset_target(&cfg, &config, "orders", &test_credentials()).unwrap();

        assert_eq!(target.pipeline, "orders");
        assert_eq!(target.skipprd_tenant, "auth-tenant");
        assert_eq!(target.model_tenant, "auth-tenant");
        assert_eq!(target.workspace, "analytics");
        assert_eq!(target.skipprd_bucket.as_deref(), Some("runtime-bucket"));
        assert_eq!(target.skipprd_prefix, "auth-tenant/analytics/orders/");
        assert_eq!(target.model_bucket, "model-bucket");
        assert!(target.model_prefix.contains("auth-tenant"));
        assert!(target.model_prefix.contains("orders"));
        assert_eq!(
            target.local_runtime_dir,
            dir.path().join("state/analytics_orders")
        );
    }

    #[tokio::test]
    async fn delete_storage_prefix_removes_only_matching_objects() {
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

        let deleted = delete_storage_prefix(&storage, "t/w/p/")
            .await
            .expect("delete");
        assert_eq!(deleted.len(), 3);
        assert!(storage.list_prefix("t/w/p/").await.unwrap().is_empty());
        assert_eq!(
            storage.list_prefix("t/w/other/").await.unwrap(),
            vec!["t/w/other/state/999/state.json".to_string()]
        );
    }

    #[tokio::test]
    async fn pipeline_reset_deletes_only_selected_pipeline_state() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("skippr.yaml");
        fs::write(
            &config,
            r#"
skippr:
  workspace: analytics
  skippr_s3_bucket: runtime-bucket
pipelines:
  orders:
    data_dir: state
  customers:
    data_dir: state
"#,
        )
        .unwrap();
        let cfg = load_resolved_engine_config(&Some(config.clone())).expect("config");
        let target =
            derive_pipeline_reset_target(&cfg, &config, "orders", &test_credentials()).unwrap();

        fs::create_dir_all(target.local_runtime_dir.join("db")).unwrap();
        fs::write(target.local_runtime_dir.join("db/CURRENT"), "fake").unwrap();
        let other_runtime = dir.path().join("state/analytics_customers");
        fs::create_dir_all(other_runtime.join("db")).unwrap();
        fs::write(other_runtime.join("db/CURRENT"), "fake").unwrap();

        for path in &target.local_model_dirs {
            fs::create_dir_all(path.join("threads")).unwrap();
            fs::write(path.join("threads/t.json"), "{}").unwrap();
        }
        let other_model = dir.path().join(".skippr/_/dev/customers/threads");
        fs::create_dir_all(&other_model).unwrap();
        fs::write(other_model.join("t.json"), "{}").unwrap();

        let skipprd_dir = tempfile::tempdir().unwrap();
        let skipprd_storage = Arc::new(skipprd::adapters::storage::LocalDiskStorageAdapter::new(
            &skipprd_dir.path().display().to_string(),
        )) as Arc<dyn skipprd::adapters::storage::StorageAdapter>;
        skipprd_storage
            .put_bytes(
                "auth-tenant/analytics/orders/metadata/metadata.json",
                b"{}",
                "application/json",
            )
            .await
            .unwrap();
        skipprd_storage
            .put_bytes(
                "auth-tenant/analytics/customers/metadata/metadata.json",
                b"{}",
                "application/json",
            )
            .await
            .unwrap();

        let model_dir = tempfile::tempdir().unwrap();
        let model_storage =
            Arc::new(LocalFileStorageAdapter::new(model_dir.path().to_path_buf()).unwrap())
                as Arc<dyn StorageAdapter>;
        let model_keep = target.model_prefix.replace("orders", "customers");
        model_storage
            .put_bytes(
                &format!("{}threads/t.json", target.model_prefix),
                b"{}",
                "application/json",
            )
            .await
            .unwrap();
        model_storage
            .put_bytes(
                &format!("{model_keep}threads/t.json"),
                b"{}",
                "application/json",
            )
            .await
            .unwrap();

        let report = delete_pipeline_reset_target_with_storage(
            &target,
            Some(&skipprd_storage),
            &model_storage,
        )
        .await
        .expect("reset");

        assert!(!target.local_runtime_dir.exists());
        assert!(other_runtime.exists());
        for path in &target.local_model_dirs {
            assert!(!path.exists());
        }
        assert!(other_model.exists());
        assert_eq!(report.deleted_skipprd_remote.len(), 1);
        assert!(skipprd_storage
            .list_prefix("auth-tenant/analytics/orders/")
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            skipprd_storage
                .list_prefix("auth-tenant/analytics/customers/")
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(report.deleted_model_remote.len(), 1);
        assert!(model_storage
            .list_prefix(&target.model_prefix)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            model_storage.list_prefix(&model_keep).await.unwrap().len(),
            1
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
            "--pipeline",
            "orders",
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
                pipeline,
            } => {
                assert!(!good);
                assert!(bad);
                assert_eq!(comment.as_deref(), Some("timed out in repair loop"));
                assert!(!no_diagnostics);
                assert_eq!(pipeline.as_str(), "orders");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn feedback_cli_accepts_no_diagnostics_flag() {
        let cli = Cli::try_parse_from([
            "skippr",
            "feedback",
            "--pipeline",
            "orders",
            "--bad",
            "--no-diagnostics",
        ])
        .expect("parse feedback args");
        match cli.cmd {
            Cmd::Feedback {
                good,
                bad,
                comment,
                no_diagnostics,
                pipeline,
            } => {
                assert!(!good);
                assert!(bad);
                assert!(comment.is_none());
                assert!(no_diagnostics);
                assert_eq!(pipeline.as_str(), "orders");
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
