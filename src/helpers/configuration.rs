use std::collections::HashMap;
use std::fmt::Debug;
use std::fs;
use std::fs::File;
use std::io::Read;

use std::path::Path;

use std::sync::Arc;
use yaml_rust::YamlLoader;

use dashmap::DashMap;
use lazy_static::lazy_static;
use once_cell::sync::Lazy;
use once_cell::sync::OnceCell;

// use aws_config::profile::profile_file::ProfileFileKind::Config;
use serde_derive::Deserialize;

use serde_json::Value;

use crate::discover::{Metadata, OutputMetadata, PipelineMetadata};
use crate::METADATA;

use crate::plugins::athena::DataSinkAthenaPluginConfig;

use crate::helpers::timed_rwlock::TimedRwLock;
use crate::helpers::Helpers;
use crate::plugins::data_source::http::DataSourceHttpPluginConfig;
use crate::plugins::data_source::kinesis::DataSourceKinesisPluginConfig;
use crate::plugins::data_source::sqs::DataSourceSqsPluginConfig;
use crate::plugins::data_source::stdin::DataSourceStdinPluginConfig;
use crate::plugins::dynamodb_input::DataSourceDynamodbPluginConfig;
use crate::plugins::file_input::DataSourceLocalFilePluginConfig;
use crate::plugins::file_output::DataSinkFilePluginConfig;
use crate::plugins::mysql_input::DataSourceMysqlPluginConfig;
use crate::plugins::s3_output::DataSinkS3PluginConfig;
use crate::plugins::s3_input::DataSourceS3PluginConfig;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use toml;
use tracing::{debug, error, info, warn};

lazy_static! {
    static ref ENV_CACHE: TimedRwLock<DashMap<String, String>> =
        TimedRwLock::new("env_cache".to_string(), DashMap::new());
}

const DEFAULT_CONFIG: &'static str = "NULL_VALUE";

pub static DATA_DIR_INIT_ONCE: OnceCell<()> = OnceCell::new();

type SchemaSyncWorkerState = (UnboundedSender<String>, Option<std::thread::JoinHandle<()>>);
static SCHEMA_SYNC_WORKER: Lazy<std::sync::Mutex<Option<SchemaSyncWorkerState>>> =
    Lazy::new(|| std::sync::Mutex::new(None));

#[derive(Debug, Deserialize, Clone)]
pub struct Skippr {
    pub workspace: Option<String>,
    pub tenant: Option<String>,
    pub skippr_s3_bucket: Option<String>,
    pub storage_mode: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Transform {
    pub batch_time_fields: Option<String>,
    pub batch_time_unit: Option<String>,
    pub flatten_events: Option<String>,
    pub record_field_path: Option<String>,
    pub batch_partition_fields: Option<String>,
    pub partition_allowed_values: Option<String>,
    pub namespace_fields: Option<String>,
    pub time_partition_prefix: Option<String>,
    pub enable_single_quote_parsing: Option<String>,
    pub enable_unicode_parsing: Option<String>,
    pub batch_order_fields: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Stats {
    pub enabled: Option<bool>,
    pub hll_precision: Option<u8>,
    pub histogram_enabled: Option<bool>,
    pub flush_seconds: Option<u64>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SemanticLayerSettings {
    pub llm_enabled: Option<bool>,
    pub llm_debounce_ms: Option<u64>,
}

#[derive(Debug, Deserialize, Clone)]
pub enum DataSourcePluginConfig {
    S3(DataSourceS3PluginConfig),
    File(DataSourceLocalFilePluginConfig),
    Mssql(DataSourceMssqlPluginConfig),
    Mysql(DataSourceMysqlPluginConfig),
    Dynamodb(DataSourceDynamodbPluginConfig),
    Kinesis(DataSourceKinesisPluginConfig),
    Sqs(DataSourceSqsPluginConfig),
    Http(DataSourceHttpPluginConfig),
    Stdin(DataSourceStdinPluginConfig),
}

impl DataSourcePluginConfig {
    pub fn format(&self) -> String {
        match self {
            DataSourcePluginConfig::S3(s3_config) => s3_config
                .format
                .clone()
                .or(Some("json".to_string()))
                .as_ref()
                .unwrap()
                .clone(),
            DataSourcePluginConfig::File(file_config) => file_config
                .format
                .clone()
                .or(Some("json".to_string()))
                .unwrap(),
            DataSourcePluginConfig::Mssql(mssql_config) => mssql_config
                .format
                .clone()
                .unwrap_or_else(|| "row".to_string()),
            DataSourcePluginConfig::Mysql(mysql_config) => mysql_config
                .format
                .clone()
                .unwrap_or_else(|| "json".to_string()),
            DataSourcePluginConfig::Dynamodb(dynamodb_config) => dynamodb_config
                .format
                .clone()
                .unwrap_or_else(|| "json".to_string()),
            DataSourcePluginConfig::Kinesis(kinesis_config) => kinesis_config
                .format
                .clone()
                .unwrap_or_else(|| "json".to_string()),
            DataSourcePluginConfig::Sqs(sqs_config) => sqs_config
                .format
                .clone()
                .unwrap_or_else(|| "json".to_string()),
            DataSourcePluginConfig::Http(http_config) => http_config
                .format
                .clone()
                .or(Some("json".to_string()))
                .as_ref()
                .unwrap()
                .clone(),
            DataSourcePluginConfig::Stdin(stdin_config) => stdin_config
                .format
                .clone()
                .unwrap_or_else(|| "json".to_string()),
        }
    }

    pub fn plugin_name(&self) -> Option<String> {
        match self {
            DataSourcePluginConfig::S3(_) => Some("S3".to_string()),
            DataSourcePluginConfig::File(_) => Some("File".to_string()),
            DataSourcePluginConfig::Mssql(_) => Some("Mssql".to_string()),
            DataSourcePluginConfig::Mysql(_) => Some("Mysql".to_string()),
            DataSourcePluginConfig::Dynamodb(_) => Some("Dynamodb".to_string()),
            DataSourcePluginConfig::Kinesis(_) => Some("Kinesis".to_string()),
            DataSourcePluginConfig::Sqs(_) => Some("Sqs".to_string()),
            DataSourcePluginConfig::Http(_) => Some("Http".to_string()),
            DataSourcePluginConfig::Stdin(_) => Some("Stdin".to_string()),
        }
    }

    pub fn batch_size_bytes(&self) -> Option<i64> {
        match self {
            DataSourcePluginConfig::S3(s3_config) => s3_config.batch_size_bytes,
            DataSourcePluginConfig::File(file_config) => file_config.batch_size_bytes,
            DataSourcePluginConfig::Mssql(mssql_config) => mssql_config.batch_size_bytes,
            DataSourcePluginConfig::Mysql(mysql_config) => mysql_config.batch_size_bytes,
            DataSourcePluginConfig::Dynamodb(dynamodb_config) => dynamodb_config.batch_size_bytes,
            DataSourcePluginConfig::Kinesis(kinesis_config) => kinesis_config.batch_size_bytes,
            DataSourcePluginConfig::Sqs(sqs_config) => sqs_config.batch_size_bytes,
            DataSourcePluginConfig::Http(http_config) => http_config.batch_size_bytes,
            DataSourcePluginConfig::Stdin(stdin_config) => stdin_config.batch_size_bytes,
        }
    }

    pub fn batch_size_seconds(&self) -> Option<i64> {
        match self {
            DataSourcePluginConfig::S3(s3_config) => s3_config.batch_size_seconds,
            DataSourcePluginConfig::File(file_config) => file_config.batch_size_seconds,
            DataSourcePluginConfig::Mssql(mssql_config) => mssql_config.batch_size_seconds,
            DataSourcePluginConfig::Mysql(mysql_config) => mysql_config.batch_size_seconds,
            DataSourcePluginConfig::Dynamodb(dynamodb_config) => dynamodb_config.batch_size_seconds,
            DataSourcePluginConfig::Kinesis(kinesis_config) => kinesis_config.batch_size_seconds,
            DataSourcePluginConfig::Sqs(sqs_config) => sqs_config.batch_size_seconds,
            DataSourcePluginConfig::Http(http_config) => http_config.batch_size_seconds,
            DataSourcePluginConfig::Stdin(stdin_config) => stdin_config.batch_size_seconds,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct DataSourceMssqlPluginConfig {
    pub connection_string: String,
    pub tables: Option<Vec<String>>,
    pub batch_size_rows: Option<usize>,
    pub query_timeout_seconds: Option<u64>,
    pub format: Option<String>,
    pub batch_size_bytes: Option<i64>,
    pub batch_size_seconds: Option<i64>,
}

#[derive(Debug, Deserialize, Clone)]
pub enum DataSinkPluginConfig {
    Athena(DataSinkAthenaPluginConfig),
    Bigquery(DataSinkBigqueryPluginConfig),
    File(DataSinkFilePluginConfig),
    Postgres(DataSinkPostgresPluginConfig),
    S3(DataSinkS3PluginConfig),
    Snowflake(DataSinkSnowflakePluginConfig),
    Stdout,
}

impl DataSinkPluginConfig {
    pub fn format(&self) -> String {
        match self {
            DataSinkPluginConfig::Athena(c) => c
                .format
                .clone()
                .or(Some("json".to_string()))
                .as_ref()
                .unwrap()
                .clone(),
            DataSinkPluginConfig::Bigquery(c) => c
                .format
                .clone()
                .unwrap_or_else(|| "json".to_string()),
            DataSinkPluginConfig::File(c) => c
                .format
                .clone()
                .or(Some("json".to_string()))
                .unwrap(),
            DataSinkPluginConfig::Postgres(c) => c
                .format
                .clone()
                .unwrap_or_else(|| "json".to_string()),
            DataSinkPluginConfig::S3(c) => c
                .format
                .clone()
                .or(Some("json".to_string()))
                .unwrap(),
            DataSinkPluginConfig::Snowflake(c) => c
                .format
                .clone()
                .unwrap_or_else(|| "parquet".to_string()),
            DataSinkPluginConfig::Stdout => "json".to_string(),
        }
    }

    pub fn plugin_name(&self) -> Option<String> {
        match self {
            DataSinkPluginConfig::Athena(_) => Some("Athena".to_string()),
            DataSinkPluginConfig::Bigquery(_) => Some("Bigquery".to_string()),
            DataSinkPluginConfig::File(_) => Some("File".to_string()),
            DataSinkPluginConfig::Postgres(_) => Some("Postgres".to_string()),
            DataSinkPluginConfig::S3(_) => Some("S3".to_string()),
            DataSinkPluginConfig::Snowflake(_) => Some("Snowflake".to_string()),
            DataSinkPluginConfig::Stdout => Some("Stdout".to_string()),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct DataSinkSnowflakePluginConfig {
    pub account: String,
    pub user: String,
    #[serde(default)]
    pub password: Option<String>,
    pub warehouse: String,
    pub database: String,
    pub schema: String,
    pub role: Option<String>,
    pub stage: Option<String>,
    pub format: Option<String>,
    #[serde(default)]
    pub private_key_path: Option<String>,
    #[serde(default)]
    pub staging_s3_bucket: Option<String>,
    #[serde(default)]
    pub staging_s3_prefix: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DataSinkBigqueryPluginConfig {
    pub project: String,
    pub dataset: String,
    pub location: Option<String>,
    pub credentials_path: Option<String>,
    pub format: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DataSinkPostgresPluginConfig {
    #[serde(default = "default_postgres_host")]
    pub host: String,
    pub port: Option<u16>,
    pub user: String,
    #[serde(default)]
    pub password: Option<String>,
    pub database: String,
    #[serde(default = "default_postgres_schema")]
    pub schema: String,
    pub sslmode: Option<String>,
    pub format: Option<String>,
}

fn default_postgres_host() -> String {
    "localhost".to_string()
}

fn default_postgres_schema() -> String {
    "public".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct GlueSchemaSinkConfig {
    pub glue_database_name: String,
}

#[derive(Debug, Deserialize, Clone)]
pub enum SchemaSinkConfig {
    Glue(GlueSchemaSinkConfig),
    Snowflake(DataSinkSnowflakePluginConfig),
    Postgres(DataSinkPostgresPluginConfig),
    Bigquery(DataSinkBigqueryPluginConfig),
}

/// Wrapper for a data sink or deadletter sink registry entry.
/// Contains the plugin config plus an optional schema_sink reference.
#[derive(Debug, Deserialize, Clone)]
pub struct DataSinkEntry {
    #[serde(flatten)]
    pub config: DataSinkPluginConfig,
    pub schema_sink: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Pipeline {
    #[serde(rename = "type")]
    pub r#type: Option<String>,
    #[allow(dead_code)]
    pub reset_offsets: Option<String>,
    #[allow(dead_code)]
    pub reset_metadata: Option<String>,
    pub auto_approve: Option<String>,
    pub env: Option<String>,
    pub buffer_threshold_bytes: Option<u64>,
    pub buffer_threshold_seconds: Option<u64>,
    #[allow(dead_code)]
    buffer_disk_threshold_bytes: Option<u64>,
    pub chaos_mode: Option<String>,
    pub sync_frequency_seconds: Option<u64>,
    pub data_dir: Option<String>,
    pub transform: Option<Transform>,
    #[serde(alias = "input")]
    pub data_source: Option<String>,
    #[serde(alias = "output")]
    pub data_sink: Option<String>,
    #[serde(alias = "deadletters", alias = "deadletter")]
    pub deadletter_sink: Option<String>,
    pub stats: Option<Stats>,
    pub semantic_layer: Option<SemanticLayerSettings>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub skippr: Option<Skippr>,
    #[serde(default)]
    pub pipelines: HashMap<String, Pipeline>,
    #[serde(alias = "data_inputs")]
    pub data_sources: Option<HashMap<String, DataSourcePluginConfig>>,
    #[serde(alias = "data_outputs")]
    pub data_sinks: Option<HashMap<String, DataSinkEntry>>,
    #[serde(alias = "data_deadletters")]
    pub deadletter_sinks: Option<HashMap<String, DataSinkEntry>>,
    #[serde(alias = "schema_outputs")]
    pub schema_sinks: Option<HashMap<String, SchemaSinkConfig>>,
}

pub static APP_CONFIG: Lazy<Arc<TimedRwLock<Option<Config>>>> =
    Lazy::new(|| Arc::new(TimedRwLock::new("config".to_string(), None)));
pub static PIPELINE_NAME: Lazy<Arc<TimedRwLock<String>>> = Lazy::new(|| {
    Arc::new(TimedRwLock::new(
        "pipeline_name".to_string(),
        "default".to_string(),
    ))
});

#[allow(dead_code)]
impl Config {
    const ALLOWED_BATCH_TIME_UNITS: [&'static str; 5] = ["year", "month", "day", "hour", "minute"];

    // Reserved pipeline/table names that cannot be used
    pub fn reserved_pipeline_names() -> &'static [&'static str] {
        &["deadletters", "wal", "_skippr", "skippr", "metadata"]
    }

    // Validate current pipeline name against reserved list
    pub fn assert_pipeline_not_reserved() {
        let name = Self::get_pipeline_name();
        let cleaned = Helpers::clean_field_name(name.clone());
        for r in Self::reserved_pipeline_names().iter() {
            if name.eq_ignore_ascii_case(r) || cleaned.eq_ignore_ascii_case(r) {
                error!(
                    "Invalid pipeline name '{}': reserved. Choose a different name. Reserved: {:?}",
                    name,
                    Self::reserved_pipeline_names()
                );
                std::process::exit(1);
            }
        }
    }

    // LLM configuration accessors
    pub fn llm_provider() -> String {
        // env > default("OPENAI")
        let v = Self::getenv("LLM_PROVIDER", "OPENAI");
        v
    }

    pub fn llm_chat_model() -> Option<String> {
        // let v = Self::getenv("LLM_CHAT_MODEL", "gpt-4o-mini"); if v.is_empty() { None } else { Some(v) }
        // let v = Self::getenv("LLM_CHAT_MODEL", "gpt-4.1"); if v.is_empty() { None } else { Some(v) }
        let v = Self::getenv("LLM_CHAT_MODEL", "gpt-5.1");
        if v.is_empty() {
            None
        } else {
            Some(v)
        }
    }

    pub fn llm_embed_model() -> Option<String> {
        let v = Self::getenv("LLM_EMBED_MODEL", "text-embedding-3-small");
        if v.is_empty() {
            None
        } else {
            Some(v)
        }
    }

    pub fn llm_base_url() -> Option<String> {
        let v = Self::getenv("LLM_BASE_URL", "https://api.openai.com");
        if v.is_empty() {
            None
        } else {
            Some(v)
        }
    }

    pub fn llm_api_key() -> Option<String> {
        let v = Self::getenv("LLM_API_KEY", "");
        if v.is_empty() {
            None
        } else {
            Some(v)
        }
    }

    pub fn llm_gpu_layers() -> Option<usize> {
        let v = Self::getenv("LLM_GPU_LAYERS", "");
        v.parse::<usize>().ok()
    }

    pub fn llm_context_length() -> usize {
        // Default optimized for latency
        let v = Self::getenv("LLM_CONTEXT_LENGTH", "4096");
        v.parse::<usize>().unwrap_or(4096)
    }
    pub fn llm_context_length_opt() -> Option<usize> {
        let v = Self::getenv("LLM_CONTEXT_LENGTH", "");
        if v.is_empty() {
            None
        } else {
            v.parse::<usize>().ok()
        }
    }
    pub fn catalog_llm_batch_size() -> usize {
        let v = Self::getenv("CATALOG_LLM_BATCH_SIZE", "4");
        v.parse::<usize>().unwrap_or(4)
    }
    pub fn catalog_llm_timeout_secs() -> u64 {
        let v = Self::getenv("CATALOG_LLM_TIMEOUT_SECS", "0");
        v.parse::<u64>().unwrap_or(0)
    }
    pub fn log_wal_enabled() -> bool {
        // Unified flag overrides
        if Self::truth_value(&Self::getenv("LOG_WAL", "")) {
            return true;
        }
        // Backward-compatible behavior
        Self::truth_value(&Self::getenv("LOG_WAL_DEBUG", "false"))
            || Self::truth_value(&Self::getenv("LOG_WAL_UPLOADS", "false"))
    }

    pub fn new() -> Config {
        Config {
            skippr: Some(Skippr {
                workspace: None,
                tenant: None,
                skippr_s3_bucket: None,
                storage_mode: None,
            }),
            pipelines: HashMap::new(),
            data_sources: None,
            data_sinks: None,
            deadletter_sinks: None,
            schema_sinks: None,
        }
    }

    pub fn find_config_file() -> String {
        let config_file = Config::getenv("SKIPPR_CONFIG_FILE", "");

        if config_file != "" {
            if Path::new(&config_file).exists() {
                return config_file.to_string();
            }
        }

        let valid_locations = vec![
            "./skippr.yml",
            "./skippr.yaml",
            "./skippr.toml",
            "./skippr.json",
        ];

        let mut file_path = String::new();

        if file_path.is_empty() {
            for location in valid_locations {
                if Path::new(location).exists() {
                    file_path = location.to_string();
                    break;
                }
            }
        }

        file_path
    }


    pub fn build_config() {
        let file_path = Config::find_config_file();

        let mut file = match File::open(&file_path) {
            Ok(file) => file,
            Err(_error) => {
                {
                    let mut app_config = APP_CONFIG.write();
                    app_config.replace(Config::new());
                }
                return;
            }
        };

        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .expect("Something went wrong reading the file");

        let config: serde_value::Value = if file_path.ends_with(".json") {
            serde_json::from_str(&contents).unwrap()
        } else if file_path.ends_with(".yml") || file_path.ends_with(".yaml") {
            serde_yaml::from_str(&contents).unwrap()
        } else if file_path.ends_with(".toml") {
            toml::from_str(&contents).unwrap()
        } else {
            panic!("Unsupported file format");
        };

        // Serialize the serde_value::Value into a String
        let string_val = serde_json::to_string(&config).unwrap();

        // Deserialize the String back into serde_json::Value
        let config: Value = serde_json::from_str(&string_val).unwrap();

        // recusively merge config with any set ENV vars
        // let config = Config::merge_config_with_env(config);

        let string_val = serde_json::to_string(&config).unwrap();
        // Deserialize the String back into Config
        let config: Config = serde_json::from_str(&string_val).unwrap();

        // println!("config: {:?}", config);

        {
            // let mut app_config = APP_CONFIG.write().unwrap();
            // let mut app_config = match APP_CONFIG.write().as_mut() {
            //     Some(app_config) => {
            //         app_config
            //     }
            //     None => {
            //         Config::new()
            //     }
            // };
            //
            // app_config = &mut config

            // set APP_CONFIG to Some(&mut config)
            let mut app_config = APP_CONFIG.write();
            app_config.replace(config);
        }

        // Ensure SKIPPR_S3_BUCKET env var is set from config (fallbacks handled inside getter)
        let bucket = Config::get_skippr_s3_bucket();
        Config::setenv("SKIPPR_S3_BUCKET", &bucket);

        // panic!("test");
    }

    fn merge_env_vars(val: &mut Value, prefix: String) {
        match val {
            Value::Object(map) => {
                for (k, v) in map.iter_mut() {
                    let env_key = format!("{}_{}", prefix, k).to_uppercase();
                    Config::merge_env_vars(v, env_key);
                }
            }
            Value::Array(arr) => {
                for (i, v) in arr.iter_mut().enumerate() {
                    let env_key = format!("{}_{}", prefix, i).to_uppercase();
                    Config::merge_env_vars(v, env_key);
                }
            }
            _ => {
                if let Ok(env_val) = std::env::var(&prefix) {
                    *val = Value::String(env_val);
                }
            }
        }
    }

    pub fn merge_config_with_env(mut config: Value) -> Value {
        Config::merge_env_vars(&mut config, "SKIPPR".to_string());
        config
    }

    fn parse_registry_ref(reference: &str, expected_prefix: &str) -> Result<String, String> {
        let parts: Vec<&str> = reference.split('.').collect();
        if parts.len() != 2 || parts[0] != expected_prefix || parts[1].is_empty() {
            return Err(format!(
                "Invalid registry reference '{}'. Expected '{}.<name>'.",
                reference, expected_prefix
            ));
        }
        Ok(parts[1].to_string())
    }

    /// Resolve the schema sink for a `DataSinkEntry` and merge inherited
    /// fields into its `DataSinkPluginConfig`. This is the single point where
    /// a data sink inherits control-plane config (database name, etc.) from
    /// its associated schema sink.
    fn inherit_schema_sink_fields(
        config: &Config,
        entry: &DataSinkEntry,
    ) -> DataSinkPluginConfig {
        let mut plugin_config = entry.config.clone();
        let schema_cfg = entry.schema_sink.as_ref().and_then(|schema_ref| {
            let schema_name = Self::parse_registry_ref(schema_ref, "schema_sinks").ok()?;
            config.schema_sinks.as_ref()?.get(&schema_name).cloned()
        });
        if let Some(schema_cfg) = schema_cfg {
            match (&mut plugin_config, schema_cfg) {
                (
                    DataSinkPluginConfig::Athena(ref mut athena),
                    SchemaSinkConfig::Glue(glue),
                ) => {
                    athena.glue_database_name = glue.glue_database_name;
                }
                _ => {}
            }
        }
        plugin_config
    }

    pub fn get_pipeline_input_plugin_name() -> String {
        if Config::get_envcache("DATA_SOURCE_PLUGIN_NAME") != "" {
            return Config::get_envcache("DATA_SOURCE_PLUGIN_NAME");
        } else {
            let config = Config::get();

            let pipline = match config.pipelines.get(PIPELINE_NAME.read().as_str()) {
                Some(pipeline) => pipeline,
                None => {
                    let plugin_name = Config::getenv("DATA_SOURCE_PLUGIN_NAME", "");

                    Config::set_evncache("DATA_SOURCE_PLUGIN_NAME", &plugin_name.clone());

                    return plugin_name;
                }
            };

            if pipline.data_source.is_some() {
                // split dot string
                let input_plugin_name = pipline
                    .data_source
                    .as_ref()
                    .unwrap()
                    .split('.')
                    .collect::<Vec<&str>>()[1]
                    .to_string();

                let res = match config.data_sources.as_ref() {
                    Some(data_sources) => match data_sources.get(&input_plugin_name) {
                        Some(plugin_config) => plugin_config
                            .plugin_name()
                            .clone()
                            .or(Some("".to_string()))
                            .unwrap(),
                        None => Config::getenv("DATA_SOURCE_PLUGIN_NAME", ""),
                    },
                    None => Config::getenv("DATA_SOURCE_PLUGIN_NAME", ""),
                };

                Config::set_evncache("DATA_SOURCE_PLUGIN_NAME", &res.clone());
                res
            } else {
                let res = Config::getenv("DATA_SOURCE_PLUGIN_NAME", "");
                Config::set_evncache("DATA_SOURCE_PLUGIN_NAME", &res.clone());
                res
            }
        }
    }

    pub fn get_pipeline_output_plugin_name() -> String {
        if Config::get_envcache("DATA_OUTPUT_PLUGIN_NAME") != "" {
            return Config::get_envcache("DATA_OUTPUT_PLUGIN_NAME");
        } else {
            let config = Config::get();

            let pipline = match config.pipelines.get(PIPELINE_NAME.read().as_str()) {
                Some(pipeline) => pipeline,
                None => {
                    let plugin_name = Config::getenv("DATA_OUTPUT_PLUGIN_NAME", "");
                    Config::set_evncache("DATA_OUTPUT_PLUGIN_NAME", &plugin_name.clone());
                    return plugin_name;
                }
            };

            if pipline.data_sink.is_some() {
                // split dot string
                let output_plugin_name = pipline
                    .data_sink
                    .as_ref()
                    .unwrap()
                    .split('.')
                    .collect::<Vec<&str>>()[1]
                    .to_string();

                let res = match config.data_sinks.as_ref() {
                    Some(data_sinks) => match data_sinks.get(&output_plugin_name) {
                        Some(entry) => entry
                            .config
                            .plugin_name()
                            .clone()
                            .or(Some("".to_string()))
                            .unwrap(),
                        None => Config::getenv("DATA_OUTPUT_PLUGIN_NAME", ""),
                    },
                    None => Config::getenv("DATA_OUTPUT_PLUGIN_NAME", ""),
                };

                Config::set_evncache("DATA_OUTPUT_PLUGIN_NAME", &res.clone());
                res
            } else {
                let res = Config::getenv("DATA_OUTPUT_PLUGIN_NAME", "");
                Config::set_evncache("DATA_OUTPUT_PLUGIN_NAME", &res.clone());
                res
            }
        }
    }

    pub fn get_pipeline_deadletters_ref() -> Option<String> {
        let pipeline = Config::get_pipeline_config();
        pipeline.deadletter_sink.clone()
    }

    fn resolve_deadletter_plugin_config_for(
        config: &Config,
        pipeline: &Pipeline,
    ) -> Result<Option<DataSinkPluginConfig>, String> {
        let reference = match pipeline.deadletter_sink.as_ref() {
            Some(reference) => reference,
            None => return Ok(None),
        };
        let deadletter_name = Self::parse_registry_ref(reference, "deadletter_sinks")?;

        match config
            .deadletter_sinks
            .as_ref()
            .and_then(|registry| registry.get(&deadletter_name))
        {
            Some(entry) => Ok(Some(Self::inherit_schema_sink_fields(config, entry))),
            None => Err(format!(
                "Deadletter sink '{}' was configured but not found in deadletter_sinks.",
                reference
            )),
        }
    }

    fn deadletter_config_violations_for(config: &Config, pipeline: &Pipeline) -> Vec<String> {
        let mut violations = Vec::new();
        if let Some(deadletters_ref) = pipeline.deadletter_sink.as_ref() {
            if let Err(err) = Self::resolve_deadletter_plugin_config_for(config, pipeline) {
                violations.push(err);
            }

            if pipeline.data_sink.as_ref() == Some(deadletters_ref) {
                violations.push(
                    "Deadletter sink must not reference the same registry entry as the primary output."
                        .to_string(),
                );
            }
        }
        violations
    }

    pub fn get_pipeline_output_sink_ref() -> String {
        let pipeline = Config::get_pipeline_config();
        pipeline
            .data_sink
            .clone()
            .unwrap_or_else(|| "data_sinks.__default__".to_string())
    }

    pub fn get_pipeline_deadletter_plugin_name() -> Result<Option<String>, String> {
        let config = Config::get();
        let pipeline = Config::get_pipeline_config();
        match Self::resolve_deadletter_plugin_config_for(&config, &pipeline)? {
            Some(plugin_config) => Ok(plugin_config.plugin_name()),
            None => Ok(None),
        }
    }

    pub fn get_pipeline_schema_plugin_name() -> String {
        if Config::get_envcache("DATA_SCHEMA_PLUGIN_NAME") != "" {
            return Config::get_envcache("DATA_SCHEMA_PLUGIN_NAME");
        } else {
            let config = Config::get();

            let pipline = match config.pipelines.get(PIPELINE_NAME.read().as_str()) {
                Some(pipeline) => pipeline,
                None => {
                    let plugin_name = Config::getenv("DATA_SCHEMA_PLUGIN_NAME", "");
                    Config::set_evncache("DATA_SCHEMA_PLUGIN_NAME", &plugin_name.clone());
                    return plugin_name;
                }
            };

            let schema_sink_ref = pipline.data_sink.as_ref().and_then(|sink_ref| {
                let sink_name = Self::parse_registry_ref(sink_ref, "data_sinks").ok()?;
                config.data_sinks.as_ref()?.get(&sink_name)?.schema_sink.clone()
            });

            if let Some(ref schema_ref) = schema_sink_ref {
                let schema_name = match Self::parse_registry_ref(schema_ref, "schema_sinks") {
                    Ok(name) => name,
                    Err(_) => {
                        let res = Config::getenv("DATA_SCHEMA_PLUGIN_NAME", "");
                        Config::set_evncache("DATA_SCHEMA_PLUGIN_NAME", &res);
                        return res;
                    }
                };

                let res = match config.schema_sinks.as_ref() {
                    Some(schema_sinks) => match schema_sinks.get(&schema_name) {
                        Some(schema_config) => match schema_config {
                            SchemaSinkConfig::Glue(_) => "Glue".to_string(),
                            SchemaSinkConfig::Snowflake(_) => "Snowflake".to_string(),
                            SchemaSinkConfig::Postgres(_) => "Postgres".to_string(),
                            SchemaSinkConfig::Bigquery(_) => "Bigquery".to_string(),
                        },
                        None => Config::getenv("DATA_SCHEMA_PLUGIN_NAME", ""),
                    },
                    None => Config::getenv("DATA_SCHEMA_PLUGIN_NAME", ""),
                };

                Config::set_evncache("DATA_SCHEMA_PLUGIN_NAME", &res.clone());
                res
            } else {
                let res = Config::getenv("DATA_SCHEMA_PLUGIN_NAME", "");
                Config::set_evncache("DATA_SCHEMA_PLUGIN_NAME", &res.clone());
                res
            }
        }
    }

    pub fn get_skippr_s3_bucket() -> String {
        if Config::get_envcache("SKIPPR_S3_BUCKET") != "" {
            return Config::get_envcache("SKIPPR_S3_BUCKET");
        } else {
            let config = Config::get();

            let default_bucket = Config::getenv("SKIPPR_S3_BUCKET", "");

            let bucket = match config.skippr {
                Some(skippr) => match skippr.skippr_s3_bucket.as_ref() {
                    Some(bucket) => bucket.to_string(),
                    None => default_bucket,
                },
                None => default_bucket,
            };

            Config::set_evncache("SKIPPR_S3_BUCKET", &bucket.clone());
            bucket
        }
    }

    /// Returns `"local"` or `"s3"` (default). Controls where pipeline metadata
    /// and stats are persisted.
    pub fn get_storage_mode() -> String {
        if Config::get_envcache("SKIPPR_STORAGE_MODE") != "" {
            return Config::get_envcache("SKIPPR_STORAGE_MODE");
        } else {
            let config = Config::get();

            let default_mode = Config::getenv("SKIPPR_STORAGE_MODE", "s3");

            let mode = match config.skippr {
                Some(skippr) => match skippr.storage_mode.as_ref() {
                    Some(m) => m.to_string(),
                    None => default_mode,
                },
                None => default_mode,
            };

            Config::set_evncache("SKIPPR_STORAGE_MODE", &mode);
            mode
        }
    }

    // WAL storage selection: "disk" (default) or "s3"
    pub fn get_wal_storage() -> String {
        if Config::get_envcache("WAL_STORAGE") != "" {
            return Config::get_envcache("WAL_STORAGE");
        } else {
            let val = Config::getenv("WAL_STORAGE", "disk");
            Config::set_evncache("WAL_STORAGE", &val);
            val
        }
    }

    pub fn set_wal_storage(value: &str) {
        Config::setenv("WAL_STORAGE", value);
    }

    // WAL S3 bucket (always use SKIPPR_S3_BUCKET; no separate WAL_S3_BUCKET)
    pub fn get_wal_s3_bucket() -> String {
        // Intentionally ignore any WAL_S3_BUCKET environment variables; standardize on SKIPPR_S3_BUCKET
        Config::get_skippr_s3_bucket()
    }

    // WAL prefix (default derived: {tenant}/{workspace}/{pipeline}/segments)
    pub fn get_wal_s3_prefix() -> String {
        let default_prefix = format!(
            "{}/{}/{}/segments",
            Config::get_tenant(),
            Config::get_workspace_name(),
            Config::get_pipeline_name()
        );
        default_prefix
    }

    // Coalescing controls for WAL (reduce S3 requests)
    // Target WAL object size in bytes (default 4 MiB)
    pub fn get_wal_bytes_per_file() -> u64 {
        if Config::get_envcache("WAL_BYTES_PER_FILE") != "" {
            return Config::get_envcache("WAL_BYTES_PER_FILE")
                .parse::<u64>()
                .unwrap_or(4 * 1024 * 1024);
        } else {
            let val = Config::getenv("WAL_BYTES_PER_FILE", &(4 * 1024 * 1024).to_string());
            Config::set_evncache("WAL_BYTES_PER_FILE", &val);
            val.parse::<u64>().unwrap_or(4 * 1024 * 1024)
        }
    }

    pub fn get_wal_max_delay_seconds() -> u64 {
        if Config::get_envcache("WAL_MAX_DELAY_SECONDS") != "" {
            return Config::get_envcache("WAL_MAX_DELAY_SECONDS")
                .parse::<u64>()
                .unwrap_or(60);
        } else {
            let val = Config::getenv("WAL_MAX_DELAY_SECONDS", "60");
            Config::set_evncache("WAL_MAX_DELAY_SECONDS", &val);
            val.parse::<u64>().unwrap_or(60)
        }
    }

    pub fn get_pipelines() -> Vec<String> {
        let config = Config::get();

        let mut pipelines = vec![];

        for (key, _value) in config.pipelines.iter() {
            pipelines.push(key.clone());
        }

        pipelines
    }

    pub fn get_pipeline_config() -> Pipeline {
        let config = Config::get();

        let pipeline = match config.pipelines.get(PIPELINE_NAME.read().as_str()) {
            Some(pipeline) => pipeline,
            None => {
                return Pipeline {
                    r#type: None,
                    reset_offsets: None,
                    reset_metadata: None,
                    auto_approve: None,
                    env: None,
                    buffer_threshold_bytes: None,
                    buffer_threshold_seconds: None,
                    buffer_disk_threshold_bytes: None,
                    chaos_mode: None,
                    sync_frequency_seconds: None,
                    data_dir: None,
                    transform: None,
                    data_source: None,
                    data_sink: None,
                    deadletter_sink: None,
                    stats: None,
                    semantic_layer: None,
                }
            }
        };

        pipeline.clone()
    }

    pub fn get_transform_config() -> Transform {
        let pipline = Config::get_pipeline_config();

        match pipline.transform.as_ref() {
            Some(transform) => transform.clone(),
            None => Transform {
                batch_time_fields: None,
                batch_time_unit: None,
                flatten_events: None,
                record_field_path: None,
                batch_partition_fields: None,
                partition_allowed_values: None,
                namespace_fields: None,
                time_partition_prefix: None,
                enable_single_quote_parsing: None,
                enable_unicode_parsing: None,
                batch_order_fields: None,
            },
        }
    }

    pub fn get_pipeline_type() -> String {
        if Config::get_envcache("PIPELINE_TYPE") != "" {
            if Config::get_envcache("PIPELINE_TYPE") == DEFAULT_CONFIG {
                return "INGEST".to_string();
            }
            return Config::get_envcache("PIPELINE_TYPE");
        } else {
            let pipeline = Config::get_pipeline_config();
            let default_type = &Config::getenv("PIPELINE_TYPE", "INGEST");
            let v = pipeline.r#type.as_ref().unwrap_or(default_type);
            Config::set_evncache("PIPELINE_TYPE", &v.clone());
            v.to_string()
        }
    }

    pub fn get_stats_config() -> Stats {
        let pipeline = Config::get_pipeline_config();
        match pipeline.stats.as_ref() {
            Some(stats) => stats.clone(),
            None => Stats {
                enabled: None,
                hll_precision: None,
                histogram_enabled: None,
                flush_seconds: None,
            },
        }
    }

    pub fn get_transform_batch_partition_fields() -> String {
        if Config::get_envcache("TRANSFORM_BATCH_PARTITION_FIELDS") != "" {
            if Config::get_envcache("TRANSFORM_BATCH_PARTITION_FIELDS") == DEFAULT_CONFIG {
                return "".to_string();
            }
            return Config::get_envcache("TRANSFORM_BATCH_PARTITION_FIELDS");
        } else {
            let pipline = Config::get_pipeline_config();

            let default_batch_partition_fields =
                &Config::getenv("TRANSFORM_BATCH_PARTITION_FIELDS", DEFAULT_CONFIG);
            let batch_partition_fields = match pipline.transform.as_ref() {
                Some(transform) => transform
                    .batch_partition_fields
                    .as_ref()
                    .unwrap_or(default_batch_partition_fields),
                None => default_batch_partition_fields,
            };

            Config::set_evncache(
                "TRANSFORM_BATCH_PARTITION_FIELDS",
                &batch_partition_fields.clone(),
            );
            batch_partition_fields.to_string()
        }
    }

    pub fn get_transform_batch_order_fields() -> String {
        if Config::get_envcache("TRANSFORM_BATCH_ORDER_FIELDS") != "" {
            if Config::get_envcache("TRANSFORM_BATCH_ORDER_FIELDS") == DEFAULT_CONFIG {
                return "".to_string();
            }
            return Config::get_envcache("TRANSFORM_BATCH_ORDER_FIELDS");
        } else {
            let pipline = Config::get_pipeline_config();

            let default_batch_order_fields =
                &Config::getenv("TRANSFORM_BATCH_ORDER_FIELDS", DEFAULT_CONFIG);
            let batch_order_fields = match pipline.transform.as_ref() {
                Some(transform) => transform
                    .batch_order_fields
                    .as_ref()
                    .unwrap_or(default_batch_order_fields),
                None => default_batch_order_fields,
            };

            Config::set_evncache(
                "TRANSFORM_BATCH_ORDER_FIELDS",
                &batch_order_fields.clone(),
            );
            batch_order_fields.to_string()
        }
    }

    pub fn get_partition_allowed_values() -> String {
        if Config::get_envcache("TRANSFORM_PARTITION_ALLOWED_VALUES") != "" {
            if Config::get_envcache("TRANSFORM_PARTITION_ALLOWED_VALUES") == DEFAULT_CONFIG {
                return "".to_string();
            }
            return Config::get_envcache("TRANSFORM_PARTITION_ALLOWED_VALUES");
        } else {
            let pipline = Config::get_pipeline_config();

            let default_partition_allowed_values =
                &Config::getenv("TRANSFORM_PARTITION_ALLOWED_VALUES", DEFAULT_CONFIG);
            let partition_allowed_values = match pipline.transform.as_ref() {
                Some(transform) => transform
                    .partition_allowed_values
                    .as_ref()
                    .unwrap_or(default_partition_allowed_values),
                None => default_partition_allowed_values,
            };

            Config::set_evncache(
                "TRANSFORM_PARTITION_ALLOWED_VALUES",
                &partition_allowed_values.clone(),
            );

            if partition_allowed_values == DEFAULT_CONFIG {
                return "".to_string();
            } else {
                partition_allowed_values.to_string()
            }
        }
    }

    pub fn get_transform_namespace_fields() -> String {
        if Config::get_envcache("TRANSFORM_NAMESPACE_FIELDS") != "" {
            if Config::get_envcache("TRANSFORM_NAMESPACE_FIELDS") == DEFAULT_CONFIG {
                return "".to_string();
            }
            return Config::get_envcache("TRANSFORM_NAMESPACE_FIELDS");
        } else {
            let pipline = Config::get_pipeline_config();

            let default_namespace_fields =
                &Config::getenv("TRANSFORM_NAMESPACE_FIELDS", DEFAULT_CONFIG);
            let namespace_fields = match pipline.transform.as_ref() {
                Some(transform) => transform
                    .namespace_fields
                    .as_ref()
                    .unwrap_or(default_namespace_fields),
                None => default_namespace_fields,
            };
            Config::set_evncache("TRANSFORM_NAMESPACE_FIELDS", &namespace_fields.clone());

            namespace_fields.to_string()
        }
    }

    pub fn get_transform_flatten_events() -> bool {
        if Config::get_envcache("TRANSFORM_FLATTEN_EVENTS") != "" {
            if Config::get_envcache("TRANSFORM_FLATTEN_EVENTS") == DEFAULT_CONFIG {
                return false;
            }
            return Config::truth_value(&Config::get_envcache("TRANSFORM_FLATTEN_EVENTS"));
        } else {
            let pipline = Config::get_pipeline_config();

            let default_flatten_events =
                &Config::getenv("TRANSFORM_FLATTEN_EVENTS", DEFAULT_CONFIG);
            let flatten_events = match pipline.transform.as_ref() {
                Some(transform) => transform
                    .flatten_events
                    .as_ref()
                    .unwrap_or(default_flatten_events),
                None => default_flatten_events,
            };
            Config::set_evncache("TRANSFORM_FLATTEN_EVENTS", &flatten_events.clone());

            Config::truth_value(flatten_events)
        }
    }

    pub fn get_transform_record_field_path() -> String {
        if Config::get_envcache("TRANSFORM_RECORD_FIELD_PATH") != "" {
            if Config::get_envcache("TRANSFORM_RECORD_FIELD_PATH") == DEFAULT_CONFIG {
                return "".to_string();
            }
            return Config::get_envcache("TRANSFORM_RECORD_FIELD_PATH");
        } else {
            let pipline = Config::get_pipeline_config();

            let default_record_field_path =
                &Config::getenv("TRANSFORM_RECORD_FIELD_PATH", DEFAULT_CONFIG);

            let record_field_path = match pipline.transform.as_ref() {
                Some(transform) => transform
                    .record_field_path
                    .as_ref()
                    .unwrap_or(default_record_field_path),
                None => default_record_field_path,
            };

            Config::set_evncache("TRANSFORM_RECORD_FIELD_PATH", &record_field_path.clone());
            record_field_path.to_string()
        }
    }

    pub fn get_transform_batch_time_fields() -> String {
        if Config::get_envcache("TRANSFORM_BATCH_TIME_FIELDS") != "" {
            if Config::get_envcache("TRANSFORM_BATCH_TIME_FIELDS") == DEFAULT_CONFIG {
                return "".to_string();
            }
            return Config::get_envcache("TRANSFORM_BATCH_TIME_FIELDS");
        } else {
            let pipline = Config::get_pipeline_config();

            let default_batch_time_fields =
                &Config::getenv("TRANSFORM_BATCH_TIME_FIELDS", DEFAULT_CONFIG);

            let batch_time_fields = match pipline.transform.as_ref() {
                Some(transform) => transform
                    .batch_time_fields
                    .as_ref()
                    .unwrap_or(default_batch_time_fields),
                None => default_batch_time_fields,
            };
            Config::set_evncache("TRANSFORM_BATCH_TIME_FIELDS", &batch_time_fields.clone());
            batch_time_fields.to_string()
        }
    }

    pub fn get_transform_batch_time_unit() -> String {
        if Config::get_envcache("TRANSFORM_BATCH_TIME_UNIT") != "" {
            if Config::get_envcache("TRANSFORM_BATCH_TIME_UNIT") == DEFAULT_CONFIG {
                return "".to_string();
            }
            return Config::get_envcache("TRANSFORM_BATCH_TIME_UNIT");
        } else {
            let pipline = Config::get_pipeline_config();

            let default_batch_time_unit =
                &Config::getenv("TRANSFORM_BATCH_TIME_UNIT", DEFAULT_CONFIG);

            let batch_time_unit = match pipline.transform.as_ref() {
                Some(transform) => transform
                    .batch_time_unit
                    .as_ref()
                    .unwrap_or(default_batch_time_unit),
                None => default_batch_time_unit,
            };

            Config::set_evncache("TRANSFORM_BATCH_TIME_UNIT", &batch_time_unit.clone());
            batch_time_unit.to_string()
        }
    }

    fn is_valid_batch_time_unit(unit: &str) -> bool {
        Self::ALLOWED_BATCH_TIME_UNITS
            .iter()
            .any(|allowed| unit.eq_ignore_ascii_case(allowed))
    }

    fn normalize_optional_config_value(value: String) -> String {
        let trimmed = value.trim();
        if trimmed.is_empty() || trimmed == DEFAULT_CONFIG {
            String::new()
        } else {
            trimmed.to_string()
        }
    }

    pub fn get_config_dependency_violations() -> Vec<String> {
        let mut violations: Vec<String> = Vec::new();

        let batch_time_unit =
            Self::normalize_optional_config_value(Config::get_transform_batch_time_unit());
        let batch_time_fields =
            Self::normalize_optional_config_value(Config::get_transform_batch_time_fields());
        let batch_partition_fields =
            Self::normalize_optional_config_value(Config::get_transform_batch_partition_fields());
        let partition_allowed_values =
            Self::normalize_optional_config_value(Config::get_partition_allowed_values());

        if !batch_time_unit.is_empty() {
            if !Self::is_valid_batch_time_unit(&batch_time_unit) {
                violations.push(format!(
                    "Invalid 'TRANSFORM_BATCH_TIME_UNIT' value '{}'. Allowed values: {:?}.",
                    batch_time_unit,
                    Self::ALLOWED_BATCH_TIME_UNITS
                ));
            }

            if batch_time_fields.is_empty() {
                violations.push(
                    "Config dependency missing: 'TRANSFORM_BATCH_TIME_FIELDS' is required when 'TRANSFORM_BATCH_TIME_UNIT' is set."
                        .to_string(),
                );
            }
        }

        if !partition_allowed_values.is_empty() && batch_partition_fields.is_empty() {
            violations.push(
                "Config dependency missing: 'TRANSFORM_BATCH_PARTITION_FIELDS' is required when 'TRANSFORM_PARTITION_ALLOWED_VALUES' is set."
                    .to_string(),
            );
        }

        let config = Config::get();
        let pipeline = Config::get_pipeline_config();
        violations.extend(Self::deadletter_config_violations_for(&config, &pipeline));

        violations
    }

    pub fn config_dependencies_valid() -> bool {
        Self::get_config_dependency_violations().is_empty()
    }

    pub fn assert_config_dependencies_valid() {
        let violations = Self::get_config_dependency_violations();
        if violations.is_empty() {
            return;
        }

        for violation in violations {
            error!("{}", violation);
        }
        std::process::exit(1);
    }

    pub fn get_time_partition_prefix() -> Option<String> {
        if Config::get_envcache("TRANSFORM_TIME_PARTITION_PREFIX") != "" {
            if Config::get_envcache("TRANSFORM_TIME_PARTITION_PREFIX") == DEFAULT_CONFIG {
                return None;
            }
            return Some(Config::get_envcache("TRANSFORM_TIME_PARTITION_PREFIX"));
        } else {
            let pipline = Config::get_pipeline_config();

            let default = &Config::getenv("TRANSFORM_TIME_PARTITION_PREFIX", DEFAULT_CONFIG);

            let batch_time_unit = match pipline.transform.as_ref() {
                Some(transform) => match transform.time_partition_prefix.as_ref() {
                    Some(time_partition_prefix) => {
                        Config::set_evncache(
                            "TRANSFORM_TIME_PARTITION_PREFIX",
                            &time_partition_prefix.clone(),
                        );
                        Some(time_partition_prefix.to_string())
                    }
                    None => {
                        Config::set_evncache("TRANSFORM_TIME_PARTITION_PREFIX", &default.clone());
                        None
                    }
                },
                None => None,
            };

            batch_time_unit
        }
    }

    pub fn get_sync_frequency() -> u64 {
        const DEFAULT: u64 = 900;

        if Config::get_envcache("SYNC_FREQUENCY") != "" {
            if Config::get_envcache("SYNC_FREQUENCY") == DEFAULT_CONFIG {
                return DEFAULT;
            }
            return Config::get_envcache("SYNC_FREQUENCY")
                .parse::<u64>()
                .unwrap();
        } else {
            let pipline = Config::get_pipeline_config();

            let default_sync_frequency = &Config::getenv("SYNC_FREQUENCY", &DEFAULT.to_string())
                .parse::<u64>()
                .unwrap();

            let sync_frequency = pipline
                .sync_frequency_seconds
                .as_ref()
                .unwrap_or(default_sync_frequency);

            Config::set_evncache("SYNC_FREQUENCY", &sync_frequency.clone().to_string());
            sync_frequency.clone()
        }
    }

    pub fn get_pipeline_chaos_mode() -> bool {
        if Config::get_envcache("SKIPPR_CHAOS_MODE") != "" {
            if Config::get_envcache("SKIPPR_CHAOS_MODE") == DEFAULT_CONFIG {
                return false;
            }
            return Config::truth_value(&Config::get_envcache("SKIPPR_CHAOS_MODE"));
        } else {
            let pipline = Config::get_pipeline_config();

            let default_chaos_mode = Config::getenv("SKIPPR_CHAOS_MODE", "no");
            let chaos_mode = pipline.chaos_mode.as_ref().unwrap_or(&default_chaos_mode);

            Config::set_evncache("SKIPPR_CHAOS_MODE", &chaos_mode.clone());

            Config::truth_value(chaos_mode)
        }
    }

    pub fn get_pipeline_data_dir() -> String {
        if Config::get_envcache("DATA_DIR") != "" {
            return Config::get_envcache("DATA_DIR");
        } else {
            let config = Config::get();

            let default_data_dir = Config::getenv("DATA_DIR", "./data");
            // let default_data_dir = "./data".to_string();

            let pipeline_dir = match config.pipelines.get(PIPELINE_NAME.read().as_str()) {
                Some(pipeline) => pipeline
                    .data_dir
                    .as_ref()
                    .unwrap_or(&default_data_dir)
                    .to_string(),
                None => default_data_dir,
            };

            Config::set_evncache("DATA_DIR", &pipeline_dir.clone());
            pipeline_dir
        }
    }

    pub fn get_pipeline_env() -> String {
        if Config::get_envcache("SKIPPR_ENV") != "" {
            return Config::get_envcache("SKIPPR_ENV");
        } else {
            let config = Config::get();

            let default_env = Config::getenv("SKIPPR_ENV", "prod");
            let pipeline_env = match config.pipelines.get(PIPELINE_NAME.read().as_str()) {
                Some(pipeline) => pipeline.env.as_ref().unwrap_or(&default_env).to_string(),
                None => default_env,
            };

            Config::set_evncache("SKIPPR_ENV", &pipeline_env.clone());
            pipeline_env
        }
    }

    pub fn get_auto_approve() -> bool {
        if Config::get_envcache("SCHEMA_AUTO_APPROVE") != "" {
            return Config::truth_value(&Config::get_envcache("SCHEMA_AUTO_APPROVE"));
        } else {
            let pipeline = Config::get_pipeline_config();

            let default_auto_approve = Config::getenv("SCHEMA_AUTO_APPROVE", "true");
            let auto_approve = pipeline
                .auto_approve
                .as_ref()
                .unwrap_or(&default_auto_approve);

            Config::set_evncache("SCHEMA_AUTO_APPROVE", &auto_approve.clone());

            Config::truth_value(auto_approve)
        }
    }

    pub fn get_reset_offsets() -> bool {
        if Config::get_envcache("RESET_OFFSETS") != "" {
            return Config::truth_value(&Config::get_envcache("RESET_OFFSETS"));
        } else {
            let pipeline = Config::get_pipeline_config();

            let default_auto_approve = &Config::getenv("RESET_OFFSETS", "false");
            let auto_approve = pipeline
                .reset_offsets
                .as_ref()
                .unwrap_or(&default_auto_approve);

            Config::set_evncache("RESET_OFFSETS", &auto_approve.clone());

            Config::truth_value(auto_approve)
        }
    }

    pub fn get_reset_metadata() -> bool {
        if Config::get_envcache("RESET_METADATA") != "" {
            return Config::truth_value(&Config::get_envcache("RESET_METADATA"));
        } else {
            let pipeline = Config::get_pipeline_config();

            let default = &Config::getenv("RESET_METADATA", "false");
            let value = pipeline.reset_metadata.as_ref().unwrap_or(&default);

            Config::set_evncache("RESET_METADATA", &value.clone());

            Config::truth_value(value)
        }
    }

    pub fn get_envcache(name: &str) -> String {
        match ENV_CACHE.read().get(name) {
            Some(val) => val.clone(),
            None => {
                // println!("Missed cache: {}", name);
                "".to_string()
            }
        }
    }

    pub fn set_evncache(name: &str, value: &str) {
        let cache = ENV_CACHE.write();
        cache.insert(name.to_string(), value.to_string());
    }

    pub fn reset_envcache() {
        let cache = ENV_CACHE.write();
        cache.clear();
    }

    pub fn get_pipeline_buffer_threshold_bytes() -> u64 {
        if Config::get_envcache("BUFFER_THRESHOLD_BYTES") != "" {
            return Config::get_envcache("BUFFER_THRESHOLD_BYTES")
                .parse::<u64>()
                .unwrap();
        } else {
            let pipline = Config::get_pipeline_config();

            let default = Config::getenv("BUFFER_THRESHOLD_BYTES", "10485760")
                .parse::<u64>()
                .unwrap();

            let buffer_threshold_bytes = match pipline.buffer_threshold_bytes.as_ref() {
                Some(buffer_threshold_bytes) => buffer_threshold_bytes,
                None => &default,
            };

            Config::set_evncache(
                "BUFFER_THRESHOLD_BYTES",
                &buffer_threshold_bytes.to_string(),
            );
            buffer_threshold_bytes.clone()
        }
    }

    pub fn get_pipeline_buffer_threshold_seconds() -> u64 {
        if Config::get_envcache("BUFFER_THRESHOLD_SECONDS") != "" {
            return Config::get_envcache("BUFFER_THRESHOLD_SECONDS")
                .parse::<u64>()
                .unwrap();
        } else {
            let pipline = Config::get_pipeline_config();

            let default = Config::getenv("BUFFER_THRESHOLD_SECONDS", "60")
                .parse::<u64>()
                .unwrap();

            let buffer_threshold_seconds = match pipline.buffer_threshold_seconds.as_ref() {
                Some(buffer_threshold_seconds) => buffer_threshold_seconds,
                None => &default,
            };

            Config::set_evncache(
                "BUFFER_THRESHOLD_SECONDS",
                &buffer_threshold_seconds.to_string(),
            );
            buffer_threshold_seconds.clone()
        }
    }

    pub fn get_pipeline_input_plugin_config() -> Result<DataSourcePluginConfig, String> {
        let pipeline_config = Config::get_pipeline_config();
        let config = Config::get();
        let input_name = match pipeline_config.data_source.as_ref() {
            Some(input) => Self::parse_registry_ref(input, "data_sources")?,
            None => return Err("Input not found".to_string()),
        };

        config
            .data_sources
            .as_ref()
            .and_then(|registry| registry.get(&input_name))
            .cloned()
            .ok_or_else(|| "Input not found".to_string())
    }

    pub fn get_pipeline_output_plugin_config() -> Result<DataSinkPluginConfig, String> {
        let pipeline_config = Config::get_pipeline_config();
        let config = Config::get();
        let output_name = match pipeline_config.data_sink.as_ref() {
            Some(output) => Self::parse_registry_ref(output, "data_sinks")?,
            None => return Err("Output not found".to_string()),
        };

        let entry = config
            .data_sinks
            .as_ref()
            .and_then(|registry| registry.get(&output_name))
            .ok_or_else(|| "Output not found".to_string())?;

        Ok(Self::inherit_schema_sink_fields(&config, entry))
    }

    pub fn get_pipeline_deadletter_plugin_config() -> Result<Option<DataSinkPluginConfig>, String> {
        let config = Config::get();
        let pipeline = Config::get_pipeline_config();
        Self::resolve_deadletter_plugin_config_for(&config, &pipeline)
    }

    pub fn get_pipeline_schema_plugin_config() -> Result<SchemaSinkConfig, String> {
        let pipeline_config = Config::get_pipeline_config();
        let config = Config::get();

        let sink_ref = pipeline_config
            .data_sink
            .as_ref()
            .ok_or_else(|| "Data sink not found".to_string())?;
        let sink_name = Self::parse_registry_ref(sink_ref, "data_sinks")?;

        let entry = config
            .data_sinks
            .as_ref()
            .and_then(|registry| registry.get(&sink_name))
            .ok_or_else(|| "Data sink entry not found".to_string())?;

        let schema_ref = entry
            .schema_sink
            .as_ref()
            .ok_or_else(|| "Schema sink not configured on data sink entry".to_string())?;
        let schema_name = Self::parse_registry_ref(schema_ref, "schema_sinks")?;

        config
            .schema_sinks
            .as_ref()
            .and_then(|registry| registry.get(&schema_name))
            .cloned()
            .ok_or_else(|| "Schema sink not found".to_string())
    }

    pub fn get_pipeline_deadletter_schema_config() -> Result<SchemaSinkConfig, String> {
        let pipeline_config = Config::get_pipeline_config();
        let config = Config::get();

        let sink_ref = pipeline_config
            .deadletter_sink
            .as_ref()
            .ok_or_else(|| "Deadletter sink not configured".to_string())?;
        let sink_name = Self::parse_registry_ref(sink_ref, "deadletter_sinks")?;

        let entry = config
            .deadletter_sinks
            .as_ref()
            .and_then(|registry| registry.get(&sink_name))
            .ok_or_else(|| "Deadletter sink entry not found".to_string())?;

        let schema_ref = entry
            .schema_sink
            .as_ref()
            .ok_or_else(|| "Schema sink not configured on deadletter sink entry".to_string())?;
        let schema_name = Self::parse_registry_ref(schema_ref, "schema_sinks")?;

        config
            .schema_sinks
            .as_ref()
            .and_then(|registry| registry.get(&schema_name))
            .cloned()
            .ok_or_else(|| "Schema sink not found".to_string())
    }

    // Function to access the config anywhere in the code.
    pub fn get() -> Config {
        match APP_CONFIG.read().as_ref() {
            Some(app_config) => app_config.clone(),
            None => Config::new(),
        }
    }

    pub fn setenv(name: &str, value: &str) {
        std::env::set_var(name, value);
        let cache = ENV_CACHE.write();
        cache.insert(name.to_string(), value.to_string());
    }

    pub fn getenv(name: &str, default: &str) -> String {
        // Try to get from environment first
        match std::env::var(name.to_uppercase()) {
            Ok(val) if !val.is_empty() => {
                // Store in cache
                let cache = ENV_CACHE.write();
                cache.insert(name.to_string(), val.clone());
                val
            }
            _ => {
                // Use default value
                default.to_string()
            }
        }
    }

    pub fn list_dir_contents<P: AsRef<Path>>(path: P) -> std::io::Result<()> {
        if path.as_ref().is_dir() {
            for entry_result in fs::read_dir(path)? {
                let entry = entry_result?;
                let path = entry.path();
                if path.is_dir() {
                    debug!("Directory: {}", path.display());
                    Config::list_dir_contents(path.clone())
                        .expect(format!("Couldn't list dir {}", path.display()).as_str());
                } else {
                    debug!("File: {}", path.display());
                }
            }
        }
        Ok(())
    }

    pub fn get_data_dir() -> String {
        let mut data_dir = Config::get_pipeline_data_dir();
        if data_dir.ends_with('/') {
            data_dir.pop();
        }

        let pipeline_name = Config::get_full_namespace_name();

        let data_dir = format!("{}/{}", data_dir, pipeline_name);
        match fs::create_dir_all(&data_dir) {
            Ok(_g) => {}
            Err(err) => panic!(
                "Error creating data dir {}, does the host path exist? {:?}",
                data_dir, err
            ),
        }

        data_dir
    }

    pub fn truth_value(condition: &str) -> bool {
        match condition {
            "true" => true,
            "t" => true,
            "false" => false,
            "f" => false,
            "yes" => true,
            "no" => false,
            "1" => true,
            "0" => false,
            _ => false,
        }
    }

    pub fn get_pipeline_name() -> String {
        PIPELINE_NAME.read().clone()
    }

    pub fn get_workspace_name() -> String {
        if Config::get_envcache("WORKSPACE_NAME") != "" {
            return Config::get_envcache("WORKSPACE_NAME");
        } else {
            let config = Config::get();

            let default_token = Config::getenv("WORKSPACE_NAME", "default");

            let workspace_name = match config.skippr.unwrap().workspace.as_ref() {
                Some(workspace) => workspace.to_string(),
                None => default_token,
            };

            Config::set_evncache("WORKSPACE_NAME", &workspace_name.clone());
            workspace_name
        }
    }

    pub fn get_tenant() -> String {
        if Config::get_envcache("TENANT") != "" {
            return Config::get_envcache("TENANT");
        } else {
            let config = Config::get();

            let default_tenant = Config::getenv("TENANT", "default");

            let tenant = match config.skippr {
                Some(skippr) => match skippr.tenant.as_ref() {
                    Some(tenant) => tenant.to_string(),
                    None => default_tenant,
                },
                None => default_tenant,
            };

            Config::set_evncache("TENANT", &tenant.clone());
            tenant
        }
    }

    // Delegates to helpers::manifest::Manifest — kept for backward compat
    pub fn get_manifest_s3_key(namespace: &str) -> Option<(String, String)> {
        Some(crate::helpers::manifest::Manifest::s3_key(namespace))
    }

    pub async fn read_manifest(namespace: &str) -> Option<serde_json::Value> {
        crate::helpers::manifest::Manifest::read(namespace).await
    }

    pub async fn get_manifest_epoch(namespace: &str) -> Option<u64> {
        crate::helpers::manifest::Manifest::epoch(namespace).await
    }

    pub async fn update_manifest_with_prefix(namespace: &str, dir_prefix: &str) {
        crate::helpers::manifest::Manifest::ensure_prefix(namespace, dir_prefix).await;
    }

    pub async fn update_manifest_with_prefix_and_db(
        namespace: &str,
        dir_prefix: &str,
        database: &str,
    ) {
        crate::helpers::manifest::Manifest::ensure_prefix_and_db(namespace, dir_prefix, database)
            .await;
    }

    pub fn get_full_namespace_name() -> String {
        // let mut helpers = Helpers { CLEAN_FIELD_CACHE: Default::default() };

        let workspace = Self::get_workspace_name();
        let pipeline = Self::get_pipeline_name();

        format!("{}_{}", workspace, pipeline)
    }

    fn load_file() {
        let mut file = File::open("config/connections.yml").expect("Unable to open file");
        let mut contents = String::new();

        file.read_to_string(&mut contents)
            .expect("Unable to read file");

        let docs = YamlLoader::load_from_str(&contents).unwrap();

        // println!("{:?}", docs);
        let _doc = &docs[0];
        // println!("{:?}", doc["sources"]["S3"]);

        // return doc;
    }

    fn inject_flatten_flag(metadata: &mut PipelineMetadata) {
        match &Config::get_transform_config().flatten_events {
            Some(val) => metadata.flattened = Config::truth_value(val),
            None => metadata.flattened = false,
        }
    }

    pub async fn get_metadata() -> Result<PipelineMetadata, bool> {
        let tenant = Self::get_tenant();
        let workspace = Self::get_workspace_name();
        let pipeline = Self::get_pipeline_name();

        let key = format!(
            "{}/{}/{}/metadata/metadata.json",
            tenant, workspace, pipeline
        );
        info!(
            "get_metadata: pipeline='{}' key='{}'",
            pipeline, key
        );

        let storage = crate::adapters::storage::get_storage();
        match storage.get_json_opt(&key).await {
            Ok(Some(json_value)) => {
                match serde_json::from_value::<PipelineMetadata>(json_value) {
                    Ok(mut pipeline_metadata) => {
                        let num_entries = pipeline_metadata.metadata.len();
                        let keys: Vec<String> =
                            pipeline_metadata.metadata.keys().cloned().collect();
                        info!(
                            "Loaded metadata (entries={}, keys={:?})",
                            num_entries, keys
                        );
                        Self::inject_flatten_flag(&mut pipeline_metadata);
                        Ok(pipeline_metadata)
                    }
                    Err(e) => {
                        error!("Failed to parse metadata: {}", e);
                        std::process::exit(1);
                    }
                }
            }
            Ok(None) => Err(false),
            Err(e) => {
                error!("Failed to fetch metadata: {}", e);
                std::process::exit(1);
            }
        }
    }

    pub async fn delete_metadata() {
        let tenant = Self::get_tenant();
        let workspace = Self::get_workspace_name();
        let pipeline = Self::get_pipeline_name();

        let key = format!(
            "{}/{}/{}/metadata/metadata.json",
            tenant, workspace, pipeline
        );

        let storage = crate::adapters::storage::get_storage();
        match storage.delete_object(&key).await {
            Ok(_) => info!("Deleted pipeline metadata: {}", key),
            Err(e) => error!("Failed to delete metadata: {}", e),
        }
    }

    pub async fn set_metadata(pipeline_metadata: &PipelineMetadata, evolved: bool) {
        use once_cell::sync::Lazy as OnceLazy;
        static UPLOAD_LOCK: OnceLazy<tokio::sync::Mutex<()>> =
            OnceLazy::new(|| tokio::sync::Mutex::new(()));

        // NOTE: callers are responsible for updating in-memory METADATA via
        // METADATA.store() *before* calling this function.  Doing the store
        // here caused stale async snapshots (e.g. from namespace creation) to
        // regress already-evolved metadata, breaking serialization for records
        // processed during the regression window.

        let tenant = Self::get_tenant();
        let workspace = Self::get_workspace_name();
        let pipeline = Self::get_pipeline_name();

        let key = format!(
            "{}/{}/{}/metadata/metadata.json",
            tenant, workspace, pipeline
        );
        let json_value = match serde_json::to_value(pipeline_metadata) {
            Ok(v) => v,
            Err(e) => {
                error!("Failed to serialize metadata: {}", e);
                return;
            }
        };

        let storage = crate::adapters::storage::get_storage();
        let _guard = UPLOAD_LOCK.lock().await;
        match storage.put_json(&key, &json_value).await {
            Ok(_) => info!("Updated pipeline metadata: {}", key),
            Err(e) => error!("Failed to persist metadata: {}", e),
        }

        if evolved {
            let tx = Config::ensure_schema_sync_worker();
            for ns in pipeline_metadata.metadata.keys() {
                let _ = tx.send(ns.clone());
            }
        }
    }

    // Stats configuration toggles (env-based defaults)
    pub fn stats_enabled() -> bool {
        if Config::get_envcache("STATS_ENABLED") != "" {
            if Config::get_envcache("STATS_ENABLED") == DEFAULT_CONFIG {
                return true;
            }
            return Config::truth_value(&Config::get_envcache("STATS_ENABLED"));
        } else {
            let pipeline = Config::get_pipeline_config();
            let default_bool = Config::truth_value(&Config::getenv("STATS_ENABLED", "true"));
            let v_bool = match pipeline.stats.as_ref() {
                Some(s) => s.enabled.unwrap_or(default_bool),
                None => default_bool,
            };
            let v = if v_bool { "true" } else { "false" };
            Config::set_evncache("STATS_ENABLED", v);
            v_bool
        }
    }

    pub fn stats_hll_precision() -> u8 {
        if Config::get_envcache("STATS_HLL_PRECISION") != "" {
            return Config::get_envcache("STATS_HLL_PRECISION")
                .parse::<u8>()
                .unwrap_or(12);
        } else {
            let pipeline = Config::get_pipeline_config();
            let default = Config::getenv("STATS_HLL_PRECISION", "12");
            let v: String = match pipeline.stats.as_ref() {
                Some(s) => s.hll_precision.map(|x| x.to_string()).unwrap_or(default),
                None => default,
            };
            Config::set_evncache("STATS_HLL_PRECISION", &v.clone());
            v.parse::<u8>().unwrap_or(12)
        }
    }

    pub fn stats_histogram_enabled() -> bool {
        if Config::get_envcache("STATS_HISTOGRAM_ENABLED") != "" {
            if Config::get_envcache("STATS_HISTOGRAM_ENABLED") == DEFAULT_CONFIG {
                return true;
            }
            return Config::truth_value(&Config::get_envcache("STATS_HISTOGRAM_ENABLED"));
        } else {
            let pipeline = Config::get_pipeline_config();
            let default_bool =
                Config::truth_value(&Config::getenv("STATS_HISTOGRAM_ENABLED", "true"));
            let v_bool = match pipeline.stats.as_ref() {
                Some(s) => s.histogram_enabled.unwrap_or(default_bool),
                None => default_bool,
            };
            let v = if v_bool { "true" } else { "false" };
            Config::set_evncache("STATS_HISTOGRAM_ENABLED", v);
            v_bool
        }
    }

    pub async fn write_namespace_stats_async(
        namespace: &str,
        stats: &crate::discover::stats::NamespaceStats,
    ) {
        let json_value = match serde_json::to_value(stats) {
            Ok(v) => v,
            Err(e) => {
                error!("Failed to serialize stats: {}", e);
                return;
            }
        };

        let tenant = Self::get_tenant();
        let workspace = Self::get_workspace_name();
        let pipeline = Self::get_pipeline_name();

        let key = format!(
            "{}/{}/{}/stats/{}.json",
            tenant, workspace, pipeline, namespace
        );

        let storage = crate::adapters::storage::get_storage();
        match storage.put_json(&key, &json_value).await {
            Ok(_) => {
                let fields = json_value
                    .get("fields")
                    .and_then(|v| v.as_object())
                    .map(|m| m.keys().cloned().collect::<Vec<_>>())
                    .unwrap_or_default();
                debug!(
                    "META: wrote stats ns='{}' key='{}' fields={} sample=[{}]",
                    namespace,
                    key,
                    fields.len(),
                    fields.iter().take(8).cloned().collect::<Vec<_>>().join(",")
                );
            }
            Err(e) => error!("Failed to persist stats: {}", e),
        }

        let _ = crate::sqlrt::registry::ensure_ns_entry(&pipeline, namespace, |current| {
            let mut e = current.unwrap_or(crate::sqlrt::registry::NamespaceEntry {
                semantic_key: String::new(),
                catalog_key: String::new(),
                stats_key: String::new(),
                last_updated_epoch: 0,
            });
            e.stats_key = key.clone();
            e
        })
        .await;
    }

    pub fn write_namespace_stats_sync(
        namespace: &str,
        stats: &crate::discover::stats::NamespaceStats,
    ) {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            // Already in a runtime: spawn fire-and-forget to avoid blocking
            let ns = namespace.to_string();
            let snapshot = stats.clone();
            handle.spawn(async move {
                Self::write_namespace_stats_async(&ns, &snapshot).await;
            });
        } else {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(Self::write_namespace_stats_async(namespace, stats));
        }
    }

    pub async fn read_namespace_stats_async(namespace: &str) -> Option<serde_json::Value> {
        let tenant = Self::get_tenant();
        let workspace = Self::get_workspace_name();
        let pipeline = Self::get_pipeline_name();
        let key = format!(
            "{}/{}/{}/stats/{}.json",
            tenant, workspace, pipeline, namespace
        );

        let storage = crate::adapters::storage::get_storage();
        match storage.get_json_opt(&key).await {
            Ok(v) => v,
            Err(e) => {
                debug!("read_namespace_stats_async: {}", e);
                None
            }
        }
    }

    pub fn stats_flush_seconds() -> u64 {
        if Config::get_envcache("STATS_FLUSH_SECONDS") != "" {
            return Config::get_envcache("STATS_FLUSH_SECONDS")
                .parse::<u64>()
                .unwrap_or(5);
        } else {
            let pipeline = Config::get_pipeline_config();
            let default = Config::getenv("STATS_FLUSH_SECONDS", "5");
            let v: String = match pipeline.stats.as_ref() {
                Some(s) => s.flush_seconds.map(|x| x.to_string()).unwrap_or(default),
                None => default,
            };
            Config::set_evncache("STATS_FLUSH_SECONDS", &v.clone());
            v.parse::<u64>().unwrap_or(5)
        }
    }

    // Deprecated local cache helpers removed (S3 is canonical)

    pub fn catalog_llm_enabled() -> bool {
        Self::truth_value(&Self::getenv("CATALOG_LLM_ENABLED", "true"))
    }

    fn ensure_schema_sync_worker() -> UnboundedSender<String> {
        let mut guard = SCHEMA_SYNC_WORKER.lock().unwrap();
        if let Some((tx, _)) = guard.as_ref() {
            return tx.clone();
        }
        let (tx, mut rx): (UnboundedSender<String>, UnboundedReceiver<String>) =
            unbounded_channel();

        let pending: DashMap<String, ()> = DashMap::new();
        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let mut primary_plugin: Option<Box<dyn crate::plugins::SchemaSink + Send + Sync>> = None;
                let mut deadletter_plugin: Option<Box<dyn crate::plugins::SchemaSink + Send + Sync>> = None;

                while let Some(ns) = rx.recv().await {
                    if pending.insert(ns.clone(), ()).is_some() {
                        continue;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                    let sync_timeout = std::time::Duration::from_secs(
                        Config::getenv("SCHEMA_SYNC_TIMEOUT_SECONDS", "120")
                            .parse::<u64>()
                            .unwrap_or(120),
                    );
                    let flatten = Config::get_transform_flatten_events();
                    let md_snapshot = { METADATA.load().metadata.clone() };
                    if let Some(schema) = md_snapshot.get(&ns) {
                        let out_meta = if flatten {
                            OutputMetadata::from_flatterened_metadata(schema)
                        } else {
                            OutputMetadata::from_metadata(schema)
                        };

                        let deadletter_namespace = crate::ingest::deadletter::table_name();
                        let is_deadletter_ns = ns == deadletter_namespace;

                        if !is_deadletter_ns {
                            if primary_plugin.is_none() {
                                let data_sink_cfg = Config::get_pipeline_output_plugin_config().ok();
                                if let Ok(cfg) = Config::get_pipeline_schema_plugin_config() {
                                    primary_plugin = Some(crate::plugins::build_schema_sink(cfg, data_sink_cfg.as_ref()).await);
                                } else if let Some(cfg) = data_sink_cfg {
                                    primary_plugin = crate::plugins::build_schema_sync_plugin(cfg).await;
                                }
                            }
                            if let Some(ref plugin) = primary_plugin {
                                info!("Schema sync: updating schema for namespace {}", ns);
                                match tokio::time::timeout(
                                    sync_timeout,
                                    plugin.sync_schema(&ns, &out_meta),
                                )
                                .await
                                {
                                    Ok(Ok(_)) => {
                                        info!("Schema sync: synced output schema for namespace {}", ns);
                                    }
                                    Ok(Err(e)) => {
                                        warn!("Schema sync: failed for namespace {}: {}", ns, e);
                                    }
                                    Err(_) => {
                                        warn!(
                                            "Schema sync: timed out for namespace {} after {:?}; skipping",
                                            ns, sync_timeout
                                        );
                                    }
                                }
                            }
                        }

                        if is_deadletter_ns {
                            if deadletter_plugin.is_none() {
                                let dl_sink_cfg = Config::get_pipeline_deadletter_plugin_config().ok().flatten();
                                if let Ok(cfg) = Config::get_pipeline_deadletter_schema_config() {
                                    deadletter_plugin = Some(crate::plugins::build_schema_sink(cfg, dl_sink_cfg.as_ref()).await);
                                } else if let Some(cfg) = dl_sink_cfg {
                                    deadletter_plugin = crate::plugins::build_schema_sync_plugin(cfg).await;
                                }
                            }
                            if let Some(ref plugin) = deadletter_plugin {
                                let deadletter_out_meta = crate::ingest::deadletter::output_metadata();
                                info!("Schema sync: updating deadletter schema for namespace {}", ns);
                                match tokio::time::timeout(
                                    sync_timeout,
                                    plugin.sync_schema(&ns, &deadletter_out_meta),
                                )
                                .await
                                {
                                    Ok(Ok(_)) => {
                                        info!("Schema sync: synced deadletter schema for namespace {}", ns);
                                    }
                                    Ok(Err(e)) => {
                                        warn!("Schema sync: deadletter failed for namespace {}: {}", ns, e);
                                    }
                                    Err(_) => {
                                        warn!(
                                            "Schema sync: deadletter timed out for namespace {} after {:?}; skipping",
                                            ns, sync_timeout
                                        );
                                    }
                                }
                            }
                        }
                    } else {
                        debug!("Schema sync: namespace {} missing from metadata snapshot", ns);
                    }
                    pending.remove(&ns);
                }
            });
        });
        *guard = Some((tx.clone(), Some(handle)));
        tx
    }

    /// Drop the sender to close the channel, then join the worker thread so all
    /// pending schema sync operations complete before the process exits.
    pub fn drain_schema_sync_worker() {
        let mut guard = SCHEMA_SYNC_WORKER.lock().unwrap();
        if let Some((tx, handle)) = guard.take() {
            drop(tx);
            if let Some(h) = handle {
                let _ = h.join();
            }
        }
    }

    pub fn sync_output_schema_namespace(namespace: &str) {
        let tx = Config::ensure_schema_sync_worker();
        let _ = tx.send(namespace.to_string());
    }

    pub async fn init() {
        // Enforce reserved name policy early
        Self::assert_pipeline_not_reserved();
        // Enforce config dependency rules before pipeline runtime starts.
        Self::assert_config_dependencies_valid();

        if crate::helpers::configuration::DATA_DIR_INIT_ONCE
            .get()
            .is_none()
        {
            info!("Initializing data directories...");
        }

        let data_dir = Config::get_data_dir();
        let ingest_dir = &format!("{}/ingest_buffer", data_dir);
        let output_dir = &format!("{}/output_buffer", data_dir);
        match fs::create_dir(ingest_dir) {
            Ok(_g) => {}
            Err(_err) => {}
        }
        match fs::create_dir(format!("{}/done", ingest_dir)) {
            Ok(_g) => {}
            Err(_err) => {}
        }
        match fs::create_dir(output_dir) {
            Ok(_g) => {}
            Err(_err) => {}
        }

        if crate::helpers::configuration::DATA_DIR_INIT_ONCE
            .get()
            .is_none()
        {
            info!("Initialized data directories at: {}", data_dir);
            let _ = crate::helpers::configuration::DATA_DIR_INIT_ONCE.set(());
        }
    }

    pub fn get_enable_single_quote_parsing() -> bool {
        let env_value = Config::getenv("SKIPPR_ENABLE_SINGLE_QUOTE_PARSING", "false");

        if env_value.to_lowercase() == "true" {
            return true;
        }

        let config = Config::get();

        let pipeline = match config.pipelines.get(PIPELINE_NAME.read().as_str()) {
            Some(pipeline) => pipeline,
            None => return false,
        };

        if let Some(transform) = &pipeline.transform {
            if let Some(enable_single_quote_parsing) = &transform.enable_single_quote_parsing {
                return enable_single_quote_parsing.to_lowercase() == "true";
            }
        }

        false
    }

    pub fn get_enable_unicode_parsing() -> bool {
        let env_value = Config::getenv("SKIPPR_ENABLE_UNICODE_PARSING", "false");

        if env_value.to_lowercase() == "true" {
            return true;
        }

        let config = Config::get();

        let pipeline = match config.pipelines.get(PIPELINE_NAME.read().as_str()) {
            Some(pipeline) => pipeline,
            None => return false,
        };

        if let Some(transform) = &pipeline.transform {
            if let Some(enable_unicode_parsing) = &transform.enable_unicode_parsing {
                return enable_unicode_parsing.to_lowercase() == "true";
            }
        }

        false
    }

    pub fn debug_enabled() -> bool {
        // Consolidated switch to enable detailed ingest/schema/WAL logs
        Self::truth_value(&Self::getenv("SKIPPR_DEBUG_LOGS", "false"))
    }

    // Compute a stable md5 for a namespace's Metadata by canonicalizing JSON key order
    pub fn compute_namespace_md5(meta: &Metadata) -> String {
        // Build a canonical, schema-only view of metadata, ignoring counters and transient fields
        fn build_schema_view(meta: &Metadata) -> Value {
            // Only include fields that influence Arrow schema
            let mut obj = serde_json::Map::new();
            obj.insert("enabled".to_string(), Value::Bool(meta.enabled));
            obj.insert(
                "out_field_name".to_string(),
                Value::String(meta.out_field_name.clone()),
            );
            obj.insert(
                "determined_type".to_string(),
                Value::String(meta.determined_type.to_string()),
            );
            obj.insert(
                "determined_type_values".to_string(),
                Value::String(
                    meta.determined_type_values
                        .as_ref()
                        .map(|v| v.to_string())
                        .unwrap_or_default(),
                ),
            );
            obj.insert(
                "repetition_count".to_string(),
                Value::Number(serde_json::Number::from(meta.repetition_count)),
            );

            // Recurse into child fields deterministically
            if !meta.fields.is_empty() {
                let mut fields_vec: Vec<(String, Value)> = meta
                    .fields
                    .iter()
                    .map(|(k, v)| (k.clone(), build_schema_view(v)))
                    .collect();
                fields_vec.sort_by(|a, b| a.0.cmp(&b.0));
                let mut fields_obj = serde_json::Map::with_capacity(fields_vec.len());
                for (k, v) in fields_vec {
                    fields_obj.insert(k, v);
                }
                obj.insert("fields".to_string(), Value::Object(fields_obj));
            }

            Value::Object(obj)
        }

        let v = build_schema_view(meta);
        let s = serde_json::to_string(&v).unwrap_or_default();
        format!("{:?}", md5::compute(s))
    }

    pub fn pipeline_llm_enabled() -> bool {
        // env override
        if Self::getenv("LLM_ENABLED", "").len() > 0 {
            return Self::truth_value(&Self::getenv("LLM_ENABLED", "true"));
        }
        // pipeline setting
        let cfg = Self::get();
        let pn = PIPELINE_NAME.read();
        if let Some(p) = cfg.pipelines.get(pn.as_str()) {
            if let Some(sl) = &p.semantic_layer {
                return sl.llm_enabled.unwrap_or(true);
            }
        }
        true
    }

    pub fn pipeline_llm_debounce_ms() -> u64 {
        if let Ok(v) = Self::getenv("LLM_DEBOUNCE_MS", "").parse::<u64>() {
            return v;
        }
        let cfg = Self::get();
        let pn = PIPELINE_NAME.read();
        if let Some(p) = cfg.pipelines.get(pn.as_str()) {
            if let Some(sl) = &p.semantic_layer {
                return sl.llm_debounce_ms.unwrap_or(1500);
            }
        }
        1500
    }

    pub fn get_pipeline_cache_dir() -> String {
        // get_data_dir() already resolves to ./data/<workspace>_<pipeline>
        // Use it directly to avoid nested <workspace>_<pipeline>/<workspace>_<pipeline>
        let path = Self::get_data_dir();
        let _ = std::fs::create_dir_all(&path);
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests for our new JSON parsing configuration options
    #[test]
    fn test_enable_single_quote_parsing() {
        // Test environment variable override
        std::env::set_var("SKIPPR_ENABLE_SINGLE_QUOTE_PARSING", "true");
        assert_eq!(Config::get_enable_single_quote_parsing(), true);

        std::env::set_var("SKIPPR_ENABLE_SINGLE_QUOTE_PARSING", "false");
        assert_eq!(Config::get_enable_single_quote_parsing(), false);

        // Clean up
        std::env::remove_var("SKIPPR_ENABLE_SINGLE_QUOTE_PARSING");
    }

    #[test]
    fn test_enable_unicode_parsing() {
        // Test environment variable override
        std::env::set_var("SKIPPR_ENABLE_UNICODE_PARSING", "true");
        assert_eq!(Config::get_enable_unicode_parsing(), true);

        std::env::set_var("SKIPPR_ENABLE_UNICODE_PARSING", "false");
        assert_eq!(Config::get_enable_unicode_parsing(), false);

        // Clean up
        std::env::remove_var("SKIPPR_ENABLE_UNICODE_PARSING");
    }

    #[test]
    fn test_deadletter_config_unset_returns_none() {
        let pipeline = Pipeline {
            r#type: None,
            reset_offsets: None,
            reset_metadata: None,
            auto_approve: None,
            env: None,
            buffer_threshold_bytes: None,
            buffer_threshold_seconds: None,
            buffer_disk_threshold_bytes: None,
            chaos_mode: None,
            sync_frequency_seconds: None,
            data_dir: None,
            transform: None,
            data_source: None,
            data_sink: Some("data_sinks.main".to_string()),
            deadletter_sink: None,
            stats: None,
            semantic_layer: None,
        };
        let config = Config {
            skippr: Some(Skippr {
                workspace: None,
                tenant: None,
                skippr_s3_bucket: None,
                storage_mode: None,
            }),
            pipelines: HashMap::new(),
            data_sources: None,
            data_sinks: None,
            deadletter_sinks: Some(HashMap::new()),
            schema_sinks: None,
        };

        assert!(Config::resolve_deadletter_plugin_config_for(&config, &pipeline)
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_deadletter_config_invalid_reference_is_rejected() {
        let pipeline = Pipeline {
            r#type: None,
            reset_offsets: None,
            reset_metadata: None,
            auto_approve: None,
            env: None,
            buffer_threshold_bytes: None,
            buffer_threshold_seconds: None,
            buffer_disk_threshold_bytes: None,
            chaos_mode: None,
            sync_frequency_seconds: None,
            data_dir: None,
            transform: None,
            data_source: None,
            data_sink: Some("data_sinks.main".to_string()),
            deadletter_sink: Some("deadletter_sinks.missing".to_string()),
            stats: None,
            semantic_layer: None,
        };
        let config = Config {
            skippr: Some(Skippr {
                workspace: None,
                tenant: None,
                skippr_s3_bucket: None,
                storage_mode: None,
            }),
            pipelines: HashMap::new(),
            data_sources: None,
            data_sinks: None,
            deadletter_sinks: Some(HashMap::new()),
            schema_sinks: None,
        };

        assert!(Config::resolve_deadletter_plugin_config_for(&config, &pipeline).is_err());
        assert!(!Config::deadletter_config_violations_for(&config, &pipeline).is_empty());
    }
}
