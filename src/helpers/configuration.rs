use std::collections::HashMap;
use std::fmt::Debug;
use std::fs;
use std::fs::File;
use std::io::Read;

use std::path::Path;

use std::sync::mpsc::Receiver as StdReceiver;
use std::sync::Arc;
use yaml_rust::YamlLoader;

use dashmap::DashMap;
use lazy_static::lazy_static;
use once_cell::sync::Lazy;
use once_cell::sync::OnceCell;

// use aws_config::profile::profile_file::ProfileFileKind::Config;
use serde::de::DeserializeOwned;
use serde_derive::{Deserialize, Serialize};

use serde_json::Value;

use crate::discover::{Metadata, OutputMetadata, PipelineMetadata};
use crate::helpers::plugin_config::{DataSinkEntry, PluginConfigEntry};
use crate::METADATA;

use crate::helpers::timed_rwlock::TimedRwLock;
use crate::helpers::Helpers;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use toml;
use tracing::{debug, error, info, warn};

lazy_static! {
    static ref ENV_CACHE: TimedRwLock<DashMap<String, String>> =
        TimedRwLock::new("env_cache".to_string(), DashMap::new());
}

const DEFAULT_CONFIG: &'static str = "NULL_VALUE";

pub static DATA_DIR_INIT_ONCE: OnceCell<()> = OnceCell::new();

#[derive(Clone, Debug)]
struct SchemaPublicationRequest {
    namespace: String,
    version: u64,
    config: Config,
}

impl SchemaPublicationRequest {
    fn dirty_key(&self) -> String {
        self.config.schema_publication_identity(&self.namespace)
    }
}

type SchemaSyncWorkerState = (
    UnboundedSender<SchemaPublicationRequest>,
    Option<std::thread::JoinHandle<()>>,
    StdReceiver<()>,
);
static SCHEMA_SYNC_WORKER: Lazy<std::sync::Mutex<Option<SchemaSyncWorkerState>>> =
    Lazy::new(|| std::sync::Mutex::new(None));
static BLOCKING_PRIMARY_SCHEMA_PLUGIN: Lazy<
    std::sync::Mutex<Option<(String, Arc<dyn crate::plugins::SchemaSink + Send + Sync>)>>,
> = Lazy::new(|| std::sync::Mutex::new(None));
static PRIMARY_SCHEMA_PLUGIN_INIT: Lazy<tokio::sync::Mutex<()>> =
    Lazy::new(|| tokio::sync::Mutex::new(()));
static SCHEMA_COORDINATOR: Lazy<crate::schema_coordinator::SchemaCoordinator> =
    Lazy::new(crate::schema_coordinator::SchemaCoordinator::new);

#[derive(Debug, Deserialize, Clone)]
pub struct Skippr {
    pub workspace: Option<String>,
    pub tenant: Option<String>,
    pub skippr_s3_bucket: Option<String>,
    pub skipprd_el_storage_mode: Option<String>,
    /// Dedicated S3 bucket for WAL segments (falls back to skippr_s3_bucket).
    pub wal_s3_bucket: Option<String>,
    /// Offset store backend: `sled` (default) or `dynamodb`.
    pub offset_store: Option<String>,
    /// DynamoDB table for offset/checkpoint rows when offset_store=dynamodb.
    pub offset_dynamodb_table: Option<String>,
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
    /// Static field names and JSON values merged onto each source record before ingest.
    pub inject_fields: Option<HashMap<String, Value>>,
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

/// Product CLI dbt naming. Ignored by the engine runtime.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct ProductDbtConfig {
    pub target_schema: Option<String>,
    pub silver_suffix: Option<String>,
    pub gold_suffix: Option<String>,
}

pub type DataSourcePluginConfig = PluginConfigEntry;

#[derive(Debug, Deserialize, Serialize, Clone)]
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

pub type DataSinkPluginConfig = PluginConfigEntry;
pub type SchemaSinkConfig = PluginConfigEntry;

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
    /// CDC configuration. When present, the pipeline runs in CDC mode and
    /// validates source/sink compatibility at startup.
    pub cdc: Option<CdcPipelineConfig>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct CdcPipelineConfig {
    /// Default CDC contract used for dynamically discovered namespaces.
    #[serde(default)]
    pub default: CdcNamespaceConfig,
    /// Legacy default business key columns. Prefer `cdc.default.business_key_columns`.
    #[serde(default)]
    pub business_key_columns: Vec<String>,
    /// Namespace/table-specific CDC contracts. Keys are Skippr namespaces.
    #[serde(default)]
    pub namespaces: HashMap<String, CdcNamespaceConfig>,
}

impl CdcPipelineConfig {
    pub fn default_contract(&self) -> CdcNamespaceConfig {
        let mut default = self.default.clone();
        if default.business_key_columns.is_empty() && !self.business_key_columns.is_empty() {
            default.business_key_columns = self.business_key_columns.clone();
        }
        default
    }
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct CdcNamespaceConfig {
    /// Business key columns used for upsert/delete identity in the target.
    #[serde(default)]
    pub business_key_columns: Vec<String>,
    /// How exact-final-state sinks should handle rows with null business keys.
    /// The default is to reject them for sinks that require deterministic keys.
    #[serde(default)]
    pub null_key_policy: Option<String>,
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
    /// Product CLI dbt naming. Ignored by the engine runtime.
    pub dbt: Option<ProductDbtConfig>,
    /// Product CLI vector source settings. Ignored by the engine runtime.
    pub vector_sources: Option<HashMap<String, Value>>,
    /// Bound pipeline for this session. Not YAML. Set by Session / CLI loop.
    #[serde(skip)]
    pub active_pipeline: Option<String>,
}

#[allow(dead_code)]
impl Config {
    const ALLOWED_BATCH_TIME_UNITS: [&'static str; 5] = ["year", "month", "day", "hour", "minute"];

    // Reserved pipeline/table names that cannot be used
    pub fn reserved_pipeline_names() -> &'static [&'static str] {
        &["deadletters", "wal", "_skippr", "skippr", "metadata"]
    }

    pub fn bind_pipeline(&self, name: &str) -> Config {
        let mut bound = self.clone();
        bound.active_pipeline = Some(name.to_string());
        bound
    }

    pub fn pipeline_key(&self) -> &str {
        self.active_pipeline.as_deref().unwrap_or("")
    }

    // Validate current pipeline name against reserved list
    pub fn assert_pipeline_not_reserved(&self) {
        let name = self.get_pipeline_name();
        let cleaned = Helpers::clean_field_name(self, name.clone());
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
                skipprd_el_storage_mode: None,
                wal_s3_bucket: None,
                offset_store: None,
                offset_dynamodb_table: None,
            }),
            pipelines: HashMap::new(),
            data_sources: None,
            data_sinks: None,
            deadletter_sinks: None,
            schema_sinks: None,
            dbt: None,
            vector_sources: None,
            active_pipeline: None,
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
            "./skipprd.yml",
            "./skipprd.yaml",
            "./skipprd.toml",
            "./skipprd.json",
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

    pub fn build_config() -> Config {
        match Self::try_build_config() {
            Ok(config) => config,
            Err(err) => {
                eprintln!("[skippr] config failed: {}", err);
                std::process::exit(1);
            }
        }
    }

    pub fn try_build_config() -> Result<Config, String> {
        let file_path = Config::find_config_file();
        if file_path.is_empty() {
            return Ok(Config::new());
        }
        let config = Self::load_path(std::path::Path::new(&file_path))?;
        Ok(config)
    }

    pub fn load_path(path: &std::path::Path) -> Result<Config, String> {
        crate::helpers::dotenv::load_dotenv_for_config_yaml_path(path);
        let file_path = path.to_string_lossy().to_string();
        let mut file = File::open(path)
            .map_err(|err| format!("Config file '{}' could not be opened: {err}", file_path))?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .map_err(|err| format!("Failed to read config file '{}': {}", file_path, err))?;

        let config: serde_value::Value = if file_path.ends_with(".json") {
            serde_json::from_str(&contents)
                .map_err(|err| format!("Failed to parse JSON config '{}': {}", file_path, err))?
        } else if file_path.ends_with(".yml") || file_path.ends_with(".yaml") {
            serde_yaml::from_str(&contents)
                .map_err(|err| format!("Failed to parse YAML config '{}': {}", file_path, err))?
        } else if file_path.ends_with(".toml") {
            toml::from_str(&contents)
                .map_err(|err| format!("Failed to parse TOML config '{}': {}", file_path, err))?
        } else {
            return Err(format!(
                "Unsupported config file format '{}'. Supported formats: .json, .yml, .yaml, .toml.",
                file_path
            ));
        };

        let string_val = serde_json::to_string(&config)
            .map_err(|err| format!("Failed to normalize config '{}': {}", file_path, err))?;
        let mut config: Value = serde_json::from_str(&string_val)
            .map_err(|err| format!("Failed to normalize config '{}': {}", file_path, err))?;
        Config::resolve_env_refs_in_json_value(&mut config).map_err(|err| {
            format!(
                "Invalid environment reference in config '{}': {}",
                file_path, err
            )
        })?;
        let string_val = serde_json::to_string(&config)
            .map_err(|err| format!("Failed to normalize config '{}': {}", file_path, err))?;
        serde_json::from_str(&string_val)
            .map_err(|err| format!("Invalid Skippr configuration in '{}': {}", file_path, err))
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

    fn resolve_env_ref(value: &str, path: &str) -> Result<Option<String>, String> {
        let trimmed = value.trim();
        if !(trimmed.starts_with("${") && trimmed.ends_with('}')) {
            return Ok(None);
        }
        if trimmed.len() <= 3 || trimmed[2..trimmed.len() - 1].contains("${") {
            return Err(format!(
                "invalid environment reference '{}' at {}",
                value, path
            ));
        }
        if trimmed != value {
            return Err(format!(
                "environment reference '{}' at {} must be the entire scalar value",
                value, path
            ));
        }
        let var_name = &trimmed[2..trimmed.len() - 1];
        if var_name.trim().is_empty() {
            return Err(format!("empty environment reference at {}", path));
        }
        let env_value = std::env::var(var_name).map_err(|_| {
            format!(
                "skippr.yml references ${{{}}} at {}, but that environment variable is not set",
                var_name, path
            )
        })?;
        if env_value.trim().is_empty() {
            return Err(format!(
                "skippr.yml references ${{{}}} at {}, but that environment variable is empty",
                var_name, path
            ));
        }
        Ok(Some(env_value))
    }

    pub fn resolve_env_refs_in_json_value(value: &mut Value) -> Result<(), String> {
        fn walk(value: &mut Value, path: String) -> Result<(), String> {
            match value {
                Value::String(raw) => {
                    if let Some(resolved) = Config::resolve_env_ref(raw, &path)? {
                        *raw = resolved;
                    }
                }
                Value::Object(map) => {
                    for (key, child) in map.iter_mut() {
                        let child_path = if path.is_empty() {
                            key.clone()
                        } else {
                            format!("{}.{}", path, key)
                        };
                        walk(child, child_path)?;
                    }
                }
                Value::Array(items) => {
                    for (idx, child) in items.iter_mut().enumerate() {
                        walk(child, format!("{}[{}]", path, idx))?;
                    }
                }
                Value::Null | Value::Bool(_) | Value::Number(_) => {}
            }
            Ok(())
        }

        walk(value, String::new())
    }

    pub(crate) fn parse_registry_ref(
        reference: &str,
        expected_prefix: &str,
    ) -> Result<String, String> {
        let parts: Vec<&str> = reference.split('.').collect();
        if parts.len() != 2 || parts[0] != expected_prefix || parts[1].is_empty() {
            return Err(format!(
                "Invalid registry reference '{}'. Expected '{}.<name>'.",
                reference, expected_prefix
            ));
        }
        Ok(parts[1].to_string())
    }

    fn validate_registry_ref_exists<T>(
        registry: Option<&HashMap<String, T>>,
        reference: &str,
        expected_prefix: &str,
        pipeline_name: &str,
        field_name: &str,
    ) -> Result<String, String> {
        let entry_name = Self::parse_registry_ref(reference, expected_prefix).map_err(|err| {
            format!(
                "Invalid configuration for pipeline '{}': {} must be a {} reference. {}",
                pipeline_name, field_name, expected_prefix, err
            )
        })?;

        if registry.is_some_and(|entries| entries.contains_key(&entry_name)) {
            Ok(entry_name)
        } else {
            Err(format!(
                "Invalid configuration for pipeline '{}': {} references '{}', but '{}' is not defined.",
                pipeline_name, field_name, reference, entry_name
            ))
        }
    }

    pub fn validate_pipeline_registry_refs_for(
        config: &Config,
        pipeline_name: &str,
        pipeline: &Pipeline,
    ) -> Result<(), String> {
        let data_source_ref = pipeline.data_source.as_ref().ok_or_else(|| {
            format!(
                "Invalid configuration for pipeline '{}': data_source is required.",
                pipeline_name
            )
        })?;
        Self::validate_registry_ref_exists(
            config.data_sources.as_ref(),
            data_source_ref,
            "data_sources",
            pipeline_name,
            "data_source",
        )?;

        if let Some(data_sink_ref) = pipeline.data_sink.as_ref() {
            let data_sink_name = Self::validate_registry_ref_exists(
                config.data_sinks.as_ref(),
                data_sink_ref,
                "data_sinks",
                pipeline_name,
                "data_sink",
            )?;

            if let Some(schema_ref) = config
                .data_sinks
                .as_ref()
                .and_then(|sinks| sinks.get(&data_sink_name))
                .and_then(|entry| entry.schema_sink.as_ref())
            {
                Self::validate_registry_ref_exists(
                    config.schema_sinks.as_ref(),
                    schema_ref,
                    "schema_sinks",
                    pipeline_name,
                    "data_sink.schema_sink",
                )?;
            }
        }

        if let Some(deadletter_ref) = pipeline.deadletter_sink.as_ref() {
            let deadletter_name = Self::validate_registry_ref_exists(
                config.deadletter_sinks.as_ref(),
                deadletter_ref,
                "deadletter_sinks",
                pipeline_name,
                "deadletter_sink",
            )?;
            if let Some(schema_ref) = config
                .deadletter_sinks
                .as_ref()
                .and_then(|sinks| sinks.get(&deadletter_name))
                .and_then(|entry| entry.schema_sink.as_ref())
            {
                Self::validate_registry_ref_exists(
                    config.schema_sinks.as_ref(),
                    schema_ref,
                    "schema_sinks",
                    pipeline_name,
                    "deadletter_sink.schema_sink",
                )?;
            }
        }

        Ok(())
    }

    pub fn validate_current_pipeline_registry_refs(&self) -> Result<(), String> {
        let config = self.clone();
        let pipeline_name = self.get_pipeline_name();
        let pipeline = config
            .pipelines
            .get(pipeline_name.as_str())
            .ok_or_else(|| {
                format!(
                    "Invalid configuration: pipeline '{}' is not defined.",
                    pipeline_name
                )
            })?;
        Self::validate_pipeline_registry_refs_for(&config, &pipeline_name, pipeline)
    }

    /// Resolve the schema sink for a `DataSinkEntry` and merge inherited
    /// fields into its `DataSinkPluginConfig`. This is the single point where
    /// a data sink inherits control-plane config (database name, etc.) from
    /// its associated schema sink.
    fn inherit_schema_sink_fields(config: &Config, entry: &DataSinkEntry) -> DataSinkPluginConfig {
        let mut plugin_config = entry.config.clone();
        let schema_cfg = entry.schema_sink.as_ref().and_then(|schema_ref| {
            let schema_name = Self::parse_registry_ref(schema_ref, "schema_sinks").ok()?;
            config.schema_sinks.as_ref()?.get(&schema_name).cloned()
        });
        if let Some(schema_cfg) = schema_cfg {
            if plugin_config.plugin_name == "Athena" && schema_cfg.plugin_name == "Glue" {
                if let Some(glue_database_name) = schema_cfg
                    .config
                    .as_object()
                    .and_then(|raw| raw.get("glue_database_name"))
                    .cloned()
                {
                    plugin_config =
                        plugin_config.with_json_field("glue_database_name", glue_database_name);
                }
            }
        }
        plugin_config
    }

    pub fn get_pipeline_input_plugin_name(&self) -> String {
        let Some(pipeline) = self.pipelines.get(self.pipeline_key()) else {
            return Config::getenv("DATA_SOURCE_PLUGIN_NAME", "");
        };
        let Some(data_source_ref) = pipeline.data_source.as_ref() else {
            return Config::getenv("DATA_SOURCE_PLUGIN_NAME", "");
        };
        let Ok(entry_name) = Self::parse_registry_ref(data_source_ref, "data_sources") else {
            return Config::getenv("DATA_SOURCE_PLUGIN_NAME", "");
        };
        self.data_sources
            .as_ref()
            .and_then(|sources| sources.get(&entry_name))
            .and_then(|plugin| plugin.plugin_name())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| Config::getenv("DATA_SOURCE_PLUGIN_NAME", ""))
    }

    pub fn get_pipeline_output_plugin_name(&self) -> String {
        let Some(pipeline) = self.pipelines.get(self.pipeline_key()) else {
            return Config::getenv("DATA_OUTPUT_PLUGIN_NAME", "");
        };
        let Some(data_sink_ref) = pipeline.data_sink.as_ref() else {
            return Config::getenv("DATA_OUTPUT_PLUGIN_NAME", "");
        };
        let Ok(entry_name) = Self::parse_registry_ref(data_sink_ref, "data_sinks") else {
            return Config::getenv("DATA_OUTPUT_PLUGIN_NAME", "");
        };
        self.data_sinks
            .as_ref()
            .and_then(|sinks| sinks.get(&entry_name))
            .and_then(|entry| entry.config.plugin_name())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| Config::getenv("DATA_OUTPUT_PLUGIN_NAME", ""))
    }

    pub fn get_pipeline_deadletters_ref(&self) -> Option<String> {
        let pipeline = self.get_pipeline_config();
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

    pub fn get_pipeline_output_sink_ref(&self) -> String {
        let pipeline = self.get_pipeline_config();
        pipeline
            .data_sink
            .clone()
            .unwrap_or_else(|| "data_sinks.__default__".to_string())
    }

    pub fn get_pipeline_deadletter_plugin_name(&self) -> Result<Option<String>, String> {
        let config = self.clone();
        let pipeline = self.get_pipeline_config();
        match Self::resolve_deadletter_plugin_config_for(&config, &pipeline)? {
            Some(plugin_config) => Ok(plugin_config.plugin_name()),
            None => Ok(None),
        }
    }

    pub fn get_pipeline_deadletter_schema_plugin_name(&self) -> Result<Option<String>, String> {
        let pipeline = self.get_pipeline_config();
        if pipeline.deadletter_sink.is_none() {
            return Ok(None);
        }

        Ok(Some(
            self.get_pipeline_deadletter_schema_config()?.plugin_name,
        ))
    }

    pub fn get_pipeline_input_plugin_version(&self) -> Result<Option<String>, String> {
        Ok(self.get_pipeline_input_plugin_config()?.version())
    }

    pub fn get_pipeline_output_plugin_version(&self) -> Result<Option<String>, String> {
        Ok(self.get_pipeline_output_plugin_config()?.version())
    }

    pub fn get_pipeline_deadletter_plugin_version(&self) -> Result<Option<String>, String> {
        Ok(self
            .get_pipeline_deadletter_plugin_config()?
            .and_then(|config| config.version()))
    }

    pub fn get_pipeline_schema_plugin_version(&self) -> Result<Option<String>, String> {
        Ok(self.get_pipeline_schema_plugin_config()?.version())
    }

    pub fn get_pipeline_deadletter_schema_plugin_version(&self) -> Result<Option<String>, String> {
        Ok(self.get_pipeline_deadletter_schema_config()?.version())
    }

    pub fn get_pipeline_schema_plugin_name(&self) -> String {
        let Some(pipeline) = self.pipelines.get(self.pipeline_key()) else {
            return Config::getenv("DATA_SCHEMA_PLUGIN_NAME", "");
        };
        let schema_sink_ref = pipeline.data_sink.as_ref().and_then(|sink_ref| {
            let sink_name = Self::parse_registry_ref(sink_ref, "data_sinks").ok()?;
            self.data_sinks
                .as_ref()?
                .get(&sink_name)?
                .schema_sink
                .clone()
        });
        let Some(schema_ref) = schema_sink_ref else {
            return Config::getenv("DATA_SCHEMA_PLUGIN_NAME", "");
        };
        let Ok(schema_name) = Self::parse_registry_ref(&schema_ref, "schema_sinks") else {
            return Config::getenv("DATA_SCHEMA_PLUGIN_NAME", "");
        };
        self.schema_sinks
            .as_ref()
            .and_then(|sinks| sinks.get(&schema_name))
            .map(|cfg| cfg.plugin_name.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| Config::getenv("DATA_SCHEMA_PLUGIN_NAME", ""))
    }

    pub fn get_skippr_s3_bucket(&self) -> String {
        self.skippr
            .as_ref()
            .and_then(|skippr| skippr.skippr_s3_bucket.as_ref())
            .filter(|bucket| !bucket.is_empty())
            .cloned()
            .unwrap_or_else(|| Config::getenv("SKIPPR_S3_BUCKET", ""))
    }

    /// Returns `"local"` or `"s3"` (default). Controls where skipprd
    /// extract/load metadata and stats are persisted.
    pub fn get_storage_mode(&self) -> String {
        let v = Self::yaml_or_env(
            self.skippr
                .as_ref()
                .and_then(|s| s.skipprd_el_storage_mode.as_ref()),
            "SKIPPRD_EL_STORAGE_MODE",
            "s3",
        );
        if v.is_empty() {
            "s3".to_string()
        } else {
            v
        }
    }

    /// Raw `WAL_STORAGE` string before parse. Empty means unset (default disk).
    pub fn wal_storage_raw() -> String {
        if Config::get_envcache("WAL_STORAGE") != "" {
            return Config::get_envcache("WAL_STORAGE");
        }
        let val = Config::getenv("WAL_STORAGE", "disk");
        Config::set_evncache("WAL_STORAGE", &val);
        val
    }

    pub fn parse_wal_storage(
    ) -> Result<crate::helpers::wal_storage::WalStorage, crate::helpers::wal_storage::ConfigError>
    {
        Config::wal_storage_raw().parse()
    }

    /// Parsed WAL backend. Unknown values exit at startup.
    pub fn get_wal_storage(&self) -> crate::helpers::wal_storage::WalStorage {
        match Self::parse_wal_storage() {
            Ok(storage) => storage,
            Err(err) => {
                eprintln!("[skippr] config failed: {err}");
                std::process::exit(1);
            }
        }
    }

    pub fn set_wal_storage(value: &str) {
        Config::setenv("WAL_STORAGE", value);
        Config::set_evncache("WAL_STORAGE", value);
    }

    /// `Some` when `SKIPPR_OFFSET_STORE` or `skippr.offset_store` is explicitly set.
    pub fn configured_offset_store(
        &self,
    ) -> Result<
        Option<crate::helpers::wal_storage::OffsetStoreKind>,
        crate::helpers::wal_storage::ConfigError,
    > {
        if let Some(store) = self
            .skippr
            .as_ref()
            .and_then(|skippr| skippr.offset_store.as_ref())
            .filter(|store| !store.is_empty())
        {
            return store.parse().map(Some);
        }
        let from_env = std::env::var("SKIPPR_OFFSET_STORE").unwrap_or_default();
        if !from_env.is_empty() {
            return from_env.parse().map(Some);
        }
        Ok(None)
    }

    /// S3 bucket for WAL segments. Prefer skippr.wal_s3_bucket, then env, then datalake bucket.
    pub fn get_wal_s3_bucket(&self) -> String {
        if let Some(bucket) = self
            .skippr
            .as_ref()
            .and_then(|skippr| skippr.wal_s3_bucket.as_ref())
            .filter(|bucket| !bucket.is_empty())
        {
            return bucket.clone();
        }
        let from_env = Config::getenv("SKIPPR_WAL_S3_BUCKET", "");
        if !from_env.is_empty() {
            return from_env;
        }
        self.get_skippr_s3_bucket()
    }

    pub fn set_wal_s3_bucket(value: &str) {
        Config::setenv("SKIPPR_WAL_S3_BUCKET", value);
        Config::set_evncache("SKIPPR_WAL_S3_BUCKET", value);
    }

    pub fn set_offset_store(value: &str) {
        Config::setenv("SKIPPR_OFFSET_STORE", value);
        Config::set_evncache("SKIPPR_OFFSET_STORE", value);
    }

    pub fn get_offset_dynamodb_table(&self) -> String {
        if let Some(table) = self
            .skippr
            .as_ref()
            .and_then(|skippr| skippr.offset_dynamodb_table.as_ref())
            .filter(|table| !table.is_empty())
        {
            return table.clone();
        }
        Config::getenv("SKIPPR_OFFSET_DYNAMODB_TABLE", "")
    }

    pub fn set_offset_dynamodb_table(value: &str) {
        Config::setenv("SKIPPR_OFFSET_DYNAMODB_TABLE", value);
        Config::set_evncache("SKIPPR_OFFSET_DYNAMODB_TABLE", value);
    }

    /// Derived identity for DynamoDB offset rows: tenant#workspace#pipeline.
    pub fn offset_store_partition_key(&self) -> String {
        format!(
            "{}#{}#{}",
            self.get_tenant(),
            self.get_workspace_name(),
            self.get_pipeline_name()
        )
    }

    // WAL prefix (default derived: {tenant}/{workspace}/{pipeline}/segments)
    pub fn get_wal_s3_prefix(&self) -> String {
        let default_prefix = format!(
            "{}/{}/{}/segments",
            self.get_tenant(),
            self.get_workspace_name(),
            self.get_pipeline_name()
        );
        default_prefix
    }

    fn default_wal_bytes_per_file(&self) -> u64 {
        const MIN_WAL_BYTES_PER_FILE: u64 = 4 * 1024 * 1024;
        const MAX_WAL_BYTES_PER_FILE: u64 = 64 * 1024 * 1024;
        self.get_pipeline_buffer_threshold_bytes()
            .clamp(MIN_WAL_BYTES_PER_FILE, MAX_WAL_BYTES_PER_FILE)
    }

    // Coalescing controls for WAL. By default the WAL segment target follows the
    // pipeline buffer target so source batch, WAL, and compaction sizes do not drift.
    pub fn get_wal_bytes_per_file(&self) -> u64 {
        let default = Self::default_wal_bytes_per_file(self);
        let val = Config::getenv("WAL_BYTES_PER_FILE", "");
        if val.is_empty() {
            return default;
        }
        val.parse::<u64>().unwrap_or_else(|_| {
            warn!(
                "Invalid 'WAL_BYTES_PER_FILE' value '{}'. Falling back to default {}.",
                val, default
            );
            default
        })
    }

    pub fn get_wal_max_delay_seconds(&self) -> u64 {
        if Config::get_envcache("WAL_MAX_DELAY_SECONDS") != "" {
            return Self::parse_cached_u64(self, "WAL_MAX_DELAY_SECONDS", 60);
        } else {
            let val = Config::getenv("WAL_MAX_DELAY_SECONDS", "60");
            Config::set_evncache("WAL_MAX_DELAY_SECONDS", &val);
            val.parse::<u64>().unwrap_or_else(|_| {
                warn!(
                    "Invalid 'WAL_MAX_DELAY_SECONDS' value '{}'. Falling back to default 60.",
                    val
                );
                60
            })
        }
    }

    pub fn wal_rotation_thresholds(&self) -> (u64, u64) {
        (
            self.get_wal_bytes_per_file(),
            self.get_wal_max_delay_seconds(),
        )
    }

    pub fn get_pipelines(&self) -> Vec<String> {
        let config = self.clone();

        let mut pipelines = vec![];

        for (key, _value) in config.pipelines.iter() {
            pipelines.push(key.clone());
        }

        pipelines
    }

    pub fn get_pipeline_config(&self) -> Pipeline {
        let config = self.clone();

        let pipeline = match config.pipelines.get(self.pipeline_key()) {
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
                    cdc: None,
                }
            }
        };

        pipeline.clone()
    }

    pub fn get_transform_config(&self) -> Transform {
        let pipline = self.get_pipeline_config();

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
                inject_fields: None,
            },
        }
    }

    pub fn get_transform_inject_fields(&self) -> HashMap<String, Value> {
        self.get_transform_config()
            .inject_fields
            .unwrap_or_default()
    }

    pub fn get_pipeline_type(&self) -> String {
        let v = Self::yaml_or_env(
            self.get_pipeline_config().r#type.as_ref(),
            "PIPELINE_TYPE",
            "INGEST",
        );
        if v.is_empty() {
            "INGEST".to_string()
        } else {
            v
        }
    }

    pub fn get_stats_config(&self) -> Stats {
        let pipeline = self.get_pipeline_config();
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

    pub fn get_transform_batch_partition_fields(&self) -> String {
        Self::yaml_or_env(
            self.get_transform_config().batch_partition_fields.as_ref(),
            "TRANSFORM_BATCH_PARTITION_FIELDS",
            DEFAULT_CONFIG,
        )
    }

    pub fn get_transform_batch_order_fields(&self) -> String {
        Self::yaml_or_env(
            self.get_transform_config().batch_order_fields.as_ref(),
            "TRANSFORM_BATCH_ORDER_FIELDS",
            DEFAULT_CONFIG,
        )
    }

    pub fn get_partition_allowed_values(&self) -> String {
        Self::yaml_or_env(
            self.get_transform_config()
                .partition_allowed_values
                .as_ref(),
            "TRANSFORM_PARTITION_ALLOWED_VALUES",
            DEFAULT_CONFIG,
        )
    }

    pub fn get_transform_namespace_fields(&self) -> String {
        Self::yaml_or_env(
            self.get_transform_config().namespace_fields.as_ref(),
            "TRANSFORM_NAMESPACE_FIELDS",
            DEFAULT_CONFIG,
        )
    }

    pub fn get_transform_flatten_events(&self) -> bool {
        Self::yaml_bool_or_env(
            self.get_transform_config().flatten_events.as_ref(),
            "TRANSFORM_FLATTEN_EVENTS",
            DEFAULT_CONFIG,
        )
    }

    pub fn get_transform_record_field_path(&self) -> String {
        Self::yaml_or_env(
            self.get_transform_config().record_field_path.as_ref(),
            "TRANSFORM_RECORD_FIELD_PATH",
            DEFAULT_CONFIG,
        )
    }

    pub fn get_transform_batch_time_fields(&self) -> String {
        Self::yaml_or_env(
            self.get_transform_config().batch_time_fields.as_ref(),
            "TRANSFORM_BATCH_TIME_FIELDS",
            DEFAULT_CONFIG,
        )
    }

    pub fn get_transform_batch_time_unit(&self) -> String {
        Self::yaml_or_env(
            self.get_transform_config().batch_time_unit.as_ref(),
            "TRANSFORM_BATCH_TIME_UNIT",
            DEFAULT_CONFIG,
        )
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

    fn validate_env_u64(&self, name: &str, violations: &mut Vec<String>) {
        let value = Config::getenv(name, "");
        if value.trim().is_empty() {
            return;
        }
        if value.parse::<u64>().is_err() {
            violations.push(format!(
                "Invalid '{}' value '{}'. Expected an unsigned integer.",
                name, value
            ));
        }
    }

    fn validate_env_u8(&self, name: &str, violations: &mut Vec<String>) {
        let value = Config::getenv(name, "");
        if value.trim().is_empty() {
            return;
        }
        if value.parse::<u8>().is_err() {
            violations.push(format!(
                "Invalid '{}' value '{}'. Expected an integer between 0 and 255.",
                name, value
            ));
        }
    }

    fn parse_cached_u64(&self, name: &str, default: u64) -> u64 {
        let cached = Config::get_envcache(name);
        if cached == DEFAULT_CONFIG {
            return default;
        }
        cached.parse::<u64>().unwrap_or_else(|_| {
            warn!(
                "Invalid '{}' value '{}'. Falling back to default {}.",
                name, cached, default
            );
            default
        })
    }

    pub fn get_config_dependency_violations(&self) -> Vec<String> {
        let mut violations: Vec<String> = Vec::new();

        let batch_time_unit =
            Self::normalize_optional_config_value(self.get_transform_batch_time_unit());
        let batch_time_fields =
            Self::normalize_optional_config_value(self.get_transform_batch_time_fields());
        let batch_partition_fields =
            Self::normalize_optional_config_value(self.get_transform_batch_partition_fields());
        let partition_allowed_values =
            Self::normalize_optional_config_value(self.get_partition_allowed_values());

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

        let config = self.clone();
        let pipeline = self.get_pipeline_config();
        violations.extend(Self::deadletter_config_violations_for(&config, &pipeline));
        let pipeline_name = self.get_pipeline_name();
        if config.pipelines.contains_key(&pipeline_name) {
            if let Err(err) =
                Self::validate_pipeline_registry_refs_for(&config, &pipeline_name, &pipeline)
            {
                violations.push(err);
            }
        }

        Self::validate_env_u64(self, "SYNC_FREQUENCY", &mut violations);
        Self::validate_env_u64(self, "BUFFER_THRESHOLD_BYTES", &mut violations);
        Self::validate_env_u64(self, "BUFFER_THRESHOLD_SECONDS", &mut violations);
        Self::validate_env_u64(self, "WAL_BYTES_PER_FILE", &mut violations);
        Self::validate_env_u64(self, "WAL_MAX_DELAY_SECONDS", &mut violations);
        Self::validate_env_u64(self, "STATS_FLUSH_SECONDS", &mut violations);
        Self::validate_env_u64(self, "SCHEMA_SYNC_DEBOUNCE_MS", &mut violations);
        Self::validate_env_u64(self, "SCHEMA_SYNC_DRAIN_TIMEOUT_SECONDS", &mut violations);
        Self::validate_env_u64(self, "SCHEMA_SYNC_TIMEOUT_SECONDS", &mut violations);
        Self::validate_env_u8(self, "STATS_HLL_PRECISION", &mut violations);

        violations
    }

    pub fn config_dependencies_valid(&self) -> bool {
        self.get_config_dependency_violations().is_empty()
    }

    pub fn assert_config_dependencies_valid(&self) {
        let violations = self.get_config_dependency_violations();
        if violations.is_empty() {
            return;
        }

        for violation in violations {
            error!("{}", violation);
        }
        std::process::exit(1);
    }

    pub fn get_time_partition_prefix(&self) -> Option<String> {
        let v = Self::yaml_or_env(
            self.get_transform_config().time_partition_prefix.as_ref(),
            "TRANSFORM_TIME_PARTITION_PREFIX",
            DEFAULT_CONFIG,
        );
        if v.is_empty() {
            None
        } else {
            Some(v)
        }
    }

    pub fn get_sync_frequency(&self) -> u64 {
        Self::yaml_u64_or_env(
            self.get_pipeline_config().sync_frequency_seconds.as_ref(),
            "SYNC_FREQUENCY",
            900,
        )
    }

    pub fn get_pipeline_chaos_mode(&self) -> bool {
        Self::yaml_bool_or_env(
            self.get_pipeline_config().chaos_mode.as_ref(),
            "SKIPPR_CHAOS_MODE",
            "no",
        )
    }

    pub fn get_pipeline_data_dir(&self) -> String {
        if let Some(pipeline) = self.pipelines.get(self.pipeline_key()) {
            if let Some(dir) = pipeline.data_dir.as_ref().filter(|d| !d.is_empty()) {
                return dir.clone();
            }
        }
        Config::getenv("DATA_DIR", "./data")
    }

    pub fn get_pipeline_env(&self) -> String {
        let v = Self::yaml_or_env(
            self.get_pipeline_config().env.as_ref(),
            "SKIPPR_ENV",
            "prod",
        );
        if v.is_empty() {
            "prod".to_string()
        } else {
            v
        }
    }

    pub fn get_auto_approve(&self) -> bool {
        Self::yaml_bool_or_env(
            self.get_pipeline_config().auto_approve.as_ref(),
            "SCHEMA_AUTO_APPROVE",
            "true",
        )
    }

    pub fn get_reset_offsets(&self) -> bool {
        Self::yaml_bool_or_env(
            self.get_pipeline_config().reset_offsets.as_ref(),
            "RESET_OFFSETS",
            "false",
        )
    }

    pub fn get_reset_metadata(&self) -> bool {
        Self::yaml_bool_or_env(
            self.get_pipeline_config().reset_metadata.as_ref(),
            "RESET_METADATA",
            "false",
        )
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

    pub fn get_pipeline_buffer_threshold_bytes(&self) -> u64 {
        Self::yaml_u64_or_env(
            self.get_pipeline_config().buffer_threshold_bytes.as_ref(),
            "BUFFER_THRESHOLD_BYTES",
            10_485_760,
        )
    }

    pub fn get_pipeline_buffer_threshold_seconds(&self) -> u64 {
        Self::yaml_u64_or_env(
            self.get_pipeline_config().buffer_threshold_seconds.as_ref(),
            "BUFFER_THRESHOLD_SECONDS",
            60,
        )
    }

    pub fn get_pipeline_input_plugin_config(&self) -> Result<DataSourcePluginConfig, String> {
        if let Ok(raw_config_json) = std::env::var("SKIPPR_RUNTIME_INPUT_CONFIG_JSON") {
            let plugin_name = std::env::var("SKIPPR_RUNTIME_INPUT_PLUGIN_NAME")
                .unwrap_or_else(|_| self.get_pipeline_input_plugin_name());
            let config = serde_json::from_str(&raw_config_json).map_err(|err| {
                format!("Invalid SKIPPR_RUNTIME_INPUT_CONFIG_JSON override: {}", err)
            })?;
            return Ok(PluginConfigEntry {
                plugin_name,
                config,
            });
        }

        let pipeline_config = self.get_pipeline_config();
        let config = self.clone();
        let pipeline_name = self.get_pipeline_name();
        let input_name = match pipeline_config.data_source.as_ref() {
            Some(input) => Self::parse_registry_ref(input, "data_sources").map_err(|err| {
                format!(
                    "Invalid configuration for pipeline '{}': data_source must reference data_sources.<name>. {}",
                    pipeline_name, err
                )
            })?,
            None => {
                return Err(format!(
                    "Invalid configuration for pipeline '{}': data_source is required.",
                    pipeline_name
                ));
            }
        };

        config
            .data_sources
            .as_ref()
            .and_then(|registry| registry.get(&input_name))
            .cloned()
            .ok_or_else(|| {
                format!(
                    "Invalid configuration for pipeline '{}': data_source references 'data_sources.{}', but '{}' is not defined in data_sources.",
                    pipeline_name, input_name, input_name
                )
            })
    }

    pub fn get_pipeline_output_plugin_config(&self) -> Result<DataSinkPluginConfig, String> {
        let pipeline_config = self.get_pipeline_config();
        let config = self.clone();
        let pipeline_name = self.get_pipeline_name();
        let output_name = match pipeline_config.data_sink.as_ref() {
            Some(output) => Self::parse_registry_ref(output, "data_sinks").map_err(|err| {
                format!(
                    "Invalid configuration for pipeline '{}': data_sink must reference data_sinks.<name>. {}",
                    pipeline_name, err
                )
            })?,
            None => {
                return Err(format!(
                    "Invalid configuration for pipeline '{}': data_sink is required.",
                    pipeline_name
                ));
            }
        };

        let entry = config
            .data_sinks
            .as_ref()
            .and_then(|registry| registry.get(&output_name))
            .ok_or_else(|| {
                format!(
                    "Invalid configuration for pipeline '{}': data_sink references 'data_sinks.{}', but '{}' is not defined in data_sinks.",
                    pipeline_name, output_name, output_name
                )
            })?;

        Ok(Self::inherit_schema_sink_fields(&config, entry))
    }

    pub fn get_pipeline_deadletter_plugin_config(
        &self,
    ) -> Result<Option<DataSinkPluginConfig>, String> {
        let config = self.clone();
        let pipeline = self.get_pipeline_config();
        Self::resolve_deadletter_plugin_config_for(&config, &pipeline)
    }

    pub fn get_pipeline_schema_plugin_config(&self) -> Result<SchemaSinkConfig, String> {
        let pipeline_config = self.get_pipeline_config();
        let config = self.clone();
        let pipeline_name = self.get_pipeline_name();

        let sink_ref = pipeline_config.data_sink.as_ref().ok_or_else(|| {
            format!(
                "Invalid configuration for pipeline '{}': data_sink is required.",
                pipeline_name
            )
        })?;
        let sink_name = Self::parse_registry_ref(sink_ref, "data_sinks").map_err(|err| {
            format!(
                "Invalid configuration for pipeline '{}': data_sink must reference data_sinks.<name>. {}",
                pipeline_name, err
            )
        })?;

        let entry = config
            .data_sinks
            .as_ref()
            .and_then(|registry| registry.get(&sink_name))
            .ok_or_else(|| {
                format!(
                    "Invalid configuration for pipeline '{}': data_sink references '{}', but '{}' is not defined in data_sinks.",
                    pipeline_name, sink_ref, sink_name
                )
            })?;

        let schema_ref = entry
            .schema_sink
            .as_ref()
            .ok_or_else(|| {
                format!(
                    "Invalid configuration for pipeline '{}': data sink '{}' does not configure schema_sink.",
                    pipeline_name, sink_ref
                )
            })?;
        let schema_name = Self::parse_registry_ref(schema_ref, "schema_sinks").map_err(|err| {
            format!(
                "Invalid configuration for pipeline '{}': data sink '{}' schema_sink must reference schema_sinks.<name>. {}",
                pipeline_name, sink_ref, err
            )
        })?;

        config
            .schema_sinks
            .as_ref()
            .and_then(|registry| registry.get(&schema_name))
            .cloned()
            .ok_or_else(|| {
                format!(
                    "Invalid configuration for pipeline '{}': schema_sink references '{}', but '{}' is not defined in schema_sinks.",
                    pipeline_name, schema_ref, schema_name
                )
            })
    }

    pub fn get_pipeline_deadletter_schema_config(&self) -> Result<SchemaSinkConfig, String> {
        let pipeline_config = self.get_pipeline_config();
        let config = self.clone();
        let pipeline_name = self.get_pipeline_name();

        let sink_ref = pipeline_config.deadletter_sink.as_ref().ok_or_else(|| {
            format!(
                "Invalid configuration for pipeline '{}': deadletter_sink is not configured.",
                pipeline_name
            )
        })?;
        let sink_name = Self::parse_registry_ref(sink_ref, "deadletter_sinks").map_err(|err| {
            format!(
                "Invalid configuration for pipeline '{}': deadletter_sink must reference deadletter_sinks.<name>. {}",
                pipeline_name, err
            )
        })?;

        let entry = config
            .deadletter_sinks
            .as_ref()
            .and_then(|registry| registry.get(&sink_name))
            .ok_or_else(|| {
                format!(
                    "Invalid configuration for pipeline '{}': deadletter_sink references '{}', but '{}' is not defined in deadletter_sinks.",
                    pipeline_name, sink_ref, sink_name
                )
            })?;

        let schema_ref = entry
            .schema_sink
            .as_ref()
            .ok_or_else(|| {
                format!(
                    "Invalid configuration for pipeline '{}': deadletter sink '{}' does not configure schema_sink.",
                    pipeline_name, sink_ref
                )
            })?;
        let schema_name = Self::parse_registry_ref(schema_ref, "schema_sinks").map_err(|err| {
            format!(
                "Invalid configuration for pipeline '{}': deadletter sink '{}' schema_sink must reference schema_sinks.<name>. {}",
                pipeline_name, sink_ref, err
            )
        })?;

        config
            .schema_sinks
            .as_ref()
            .and_then(|registry| registry.get(&schema_name))
            .cloned()
            .ok_or_else(|| {
                format!(
                    "Invalid configuration for pipeline '{}': schema_sink references '{}', but '{}' is not defined in schema_sinks.",
                    pipeline_name, schema_ref, schema_name
                )
            })
    }

    pub fn deserialize_pipeline_input_plugin_config<T: DeserializeOwned>(
        &self,
    ) -> Result<T, String> {
        self.get_pipeline_input_plugin_config()?.deserialize()
    }

    pub fn deserialize_pipeline_output_plugin_config<T: DeserializeOwned>(
        &self,
    ) -> Result<T, String> {
        self.get_pipeline_output_plugin_config()?.deserialize()
    }

    pub fn deserialize_pipeline_deadletter_plugin_config<T: DeserializeOwned>(
        &self,
    ) -> Result<Option<T>, String> {
        self.get_pipeline_deadletter_plugin_config()?
            .map(|config| config.deserialize())
            .transpose()
    }

    pub fn deserialize_pipeline_schema_plugin_config<T: DeserializeOwned>(
        &self,
    ) -> Result<T, String> {
        self.get_pipeline_schema_plugin_config()?.deserialize()
    }

    pub fn setenv(name: &str, value: &str) {
        std::env::set_var(name, value);
        let cache = ENV_CACHE.write();
        cache.insert(name.to_string(), value.to_string());
    }

    pub fn getenv(name: &str, default: &str) -> String {
        match std::env::var(name.to_uppercase()) {
            Ok(val) if !val.is_empty() => {
                let cache = ENV_CACHE.write();
                cache.insert(name.to_string(), val.clone());
                val
            }
            _ => {
                let cached = Self::get_envcache(name);
                if !cached.is_empty() {
                    cached
                } else {
                    default.to_string()
                }
            }
        }
    }

    fn yaml_or_env(yaml: Option<&String>, env: &str, default: &str) -> String {
        if let Some(v) = yaml
            .map(|s| s.trim())
            .filter(|s| !s.is_empty() && *s != DEFAULT_CONFIG)
        {
            return v.to_string();
        }
        let v = Config::getenv(env, default);
        if v == DEFAULT_CONFIG {
            String::new()
        } else {
            v
        }
    }

    fn yaml_u64_or_env(yaml: Option<&u64>, env: &str, default: u64) -> u64 {
        if let Some(v) = yaml {
            return *v;
        }
        Config::getenv(env, &default.to_string())
            .parse::<u64>()
            .unwrap_or(default)
    }

    fn yaml_bool_or_env(yaml: Option<&String>, env: &str, default: &str) -> bool {
        Config::truth_value(&Self::yaml_or_env(yaml, env, default))
    }

    pub fn list_dir_contents<P: AsRef<Path>>(path: P) -> std::io::Result<()> {
        if path.as_ref().is_dir() {
            for entry_result in fs::read_dir(path)? {
                let entry = entry_result?;
                let path = entry.path();
                if path.is_dir() {
                    debug!("Directory: {}", path.display());
                    if let Err(err) = Config::list_dir_contents(path.clone()) {
                        warn!("Couldn't list dir {}: {}", path.display(), err);
                    }
                } else {
                    debug!("File: {}", path.display());
                }
            }
        }
        Ok(())
    }

    pub fn get_data_dir(&self) -> String {
        let mut data_dir = self.get_pipeline_data_dir();
        if data_dir.ends_with('/') {
            data_dir.pop();
        }

        if !Config::truth_value(&Config::getenv("SKIPPR_PIPELINE_DATA_ROOT", "false")) {
            let pipeline_name = self.get_full_namespace_name();
            data_dir = format!("{}/{}", data_dir, pipeline_name);
        }
        match fs::create_dir_all(&data_dir) {
            Ok(_g) => {}
            Err(err) => {
                eprintln!(
                    "[skippr] config failed: failed to create data directory '{}': {}",
                    data_dir, err
                );
                std::process::exit(1);
            }
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

    pub fn get_pipeline_name(&self) -> String {
        self.pipeline_key().to_string()
    }

    pub fn get_workspace_name(&self) -> String {
        self.skippr
            .as_ref()
            .and_then(|skippr| skippr.workspace.as_ref())
            .filter(|name| !name.is_empty())
            .cloned()
            .unwrap_or_else(|| Config::getenv("WORKSPACE_NAME", "default"))
    }

    pub fn get_tenant(&self) -> String {
        self.skippr
            .as_ref()
            .and_then(|skippr| skippr.tenant.as_ref())
            .filter(|name| !name.is_empty())
            .cloned()
            .unwrap_or_else(|| Config::getenv("TENANT", "default"))
    }

    // Delegates to helpers::manifest::Manifest — kept for backward compat
    pub fn get_manifest_s3_key(&self, namespace: &str) -> Option<(String, String)> {
        Some(crate::helpers::manifest::Manifest::s3_key(self, namespace))
    }

    pub async fn read_manifest(&self, namespace: &str) -> Option<serde_json::Value> {
        crate::helpers::manifest::Manifest::read(self, namespace).await
    }

    pub async fn get_manifest_epoch(&self, namespace: &str) -> Option<u64> {
        crate::helpers::manifest::Manifest::epoch(self, namespace).await
    }

    pub async fn update_manifest_with_prefix(&self, namespace: &str, dir_prefix: &str) {
        crate::helpers::manifest::Manifest::ensure_prefix(self, namespace, dir_prefix).await;
    }

    pub async fn update_manifest_with_prefix_and_db(
        &self,
        namespace: &str,
        dir_prefix: &str,
        database: &str,
    ) {
        crate::helpers::manifest::Manifest::ensure_prefix_and_db(
            self, namespace, dir_prefix, database,
        )
        .await;
    }

    pub fn get_full_namespace_name(&self) -> String {
        // let mut helpers = Helpers { CLEAN_FIELD_CACHE: Default::default() };

        let workspace = self.get_workspace_name();
        let pipeline = self.get_pipeline_name();

        format!("{}_{}", workspace, pipeline)
    }

    fn load_file(&self) {
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

    fn inject_flatten_flag(&self, metadata: &mut PipelineMetadata) {
        match &self.get_transform_config().flatten_events {
            Some(val) => metadata.flattened = Config::truth_value(val),
            None => metadata.flattened = false,
        }
    }

    pub async fn get_metadata(&self) -> Result<PipelineMetadata, bool> {
        let tenant = self.get_tenant();
        let workspace = self.get_workspace_name();
        let pipeline = self.get_pipeline_name();
        let key = self.pipeline_metadata_object_path(&tenant, &workspace, &pipeline);
        info!("get_metadata: pipeline='{}' key='{}'", pipeline, key);

        match self.load_pipeline_metadata_json(&key).await {
            Ok(Some(json_value)) => match serde_json::from_value::<PipelineMetadata>(json_value) {
                Ok(mut pipeline_metadata) => {
                    let num_entries = pipeline_metadata.metadata.len();
                    let keys: Vec<String> = pipeline_metadata.metadata.keys().cloned().collect();
                    info!("Loaded metadata (entries={}, keys={:?})", num_entries, keys);
                    self.inject_flatten_flag(&mut pipeline_metadata);
                    if pipeline_metadata.migrate_persisted_metadata() {
                        info!(
                            "Migrated persisted metadata to version {}",
                            pipeline_metadata.metadata_version
                        );
                        self.set_metadata(&pipeline_metadata, false).await;
                    }
                    Ok(pipeline_metadata)
                }
                Err(e) => {
                    error!("Failed to parse metadata: {}", e);
                    std::process::exit(1);
                }
            },
            Ok(None) => Err(false),
            Err(e) => {
                error!("Failed to fetch metadata: {}", e);
                std::process::exit(1);
            }
        }
    }

    pub fn pipeline_metadata_object_key(&self, key: &skippr_lease::PipelineKey) -> String {
        self.pipeline_metadata_object_path(key.tenant(), key.workspace(), key.pipeline())
    }

    fn pipeline_metadata_object_path(
        &self,
        tenant: &str,
        workspace: &str,
        pipeline: &str,
    ) -> String {
        format!("{tenant}/{workspace}/{pipeline}/metadata/metadata.json")
    }

    pub async fn load_pipeline_metadata(
        &self,
        key: &skippr_lease::PipelineKey,
    ) -> Result<Option<crate::discover::PipelineMetadata>, String> {
        let object_key = self.pipeline_metadata_object_key(key);
        info!("load_pipeline_metadata: key='{}'", object_key);
        match self.load_pipeline_metadata_json(&object_key).await {
            Ok(None) => Ok(None),
            Ok(Some(json_value)) => serde_json::from_value(json_value)
                .map(Some)
                .map_err(|err| format!("failed to parse pipeline metadata {object_key}: {err}")),
            Err(err) => Err(format!(
                "failed to fetch pipeline metadata {object_key}: {err}"
            )),
        }
    }

    async fn load_pipeline_metadata_json(
        &self,
        object_key: &str,
    ) -> Result<Option<serde_json::Value>, String> {
        let storage = crate::adapters::storage::get_storage(self);
        storage
            .get_json_opt(object_key)
            .await
            .map_err(|err| err.to_string())
    }

    pub async fn delete_metadata(&self) {
        let tenant = self.get_tenant();
        let workspace = self.get_workspace_name();
        let pipeline = self.get_pipeline_name();
        let key = self.pipeline_metadata_object_path(&tenant, &workspace, &pipeline);
        let storage = crate::adapters::storage::get_storage(self);
        match storage.delete_object(&key).await {
            Ok(_) => info!("Deleted pipeline metadata: {}", key),
            Err(e) => error!("Failed to delete metadata: {}", e),
        }
    }

    pub async fn set_metadata(&self, pipeline_metadata: &PipelineMetadata, evolved: bool) {
        use once_cell::sync::Lazy as OnceLazy;
        static UPLOAD_LOCK: OnceLazy<tokio::sync::Mutex<()>> =
            OnceLazy::new(|| tokio::sync::Mutex::new(()));

        // NOTE: callers are responsible for updating in-memory METADATA via
        // METADATA.store() *before* calling this function.  Doing the store
        // here caused stale async snapshots (e.g. from namespace creation) to
        // regress already-evolved metadata, breaking serialization for records
        // processed during the regression window.

        let tenant = self.get_tenant();
        let workspace = self.get_workspace_name();
        let pipeline = self.get_pipeline_name();

        let key = self.pipeline_metadata_object_path(&tenant, &workspace, &pipeline);
        let json_value = match serde_json::to_value(pipeline_metadata) {
            Ok(v) => v,
            Err(e) => {
                error!("Failed to serialize metadata: {}", e);
                return;
            }
        };

        let storage = crate::adapters::storage::get_storage(self);
        let _guard = UPLOAD_LOCK.lock().await;
        match storage.put_json(&key, &json_value).await {
            Ok(_) => info!("Updated pipeline metadata: {}", key),
            Err(e) => error!("Failed to persist metadata: {}", e),
        }

        if evolved {
            let tx = Config::ensure_schema_sync_worker(self);
            for ns in pipeline_metadata.metadata.keys() {
                let _ = tx.send(SchemaPublicationRequest {
                    namespace: ns.clone(),
                    version: Config::schema_publication_version(self, ns),
                    config: self.clone(),
                });
            }
        }
    }

    // Stats configuration toggles (env-based defaults)
    pub fn stats_enabled(&self) -> bool {
        self.get_stats_config()
            .enabled
            .unwrap_or_else(|| Config::truth_value(&Config::getenv("STATS_ENABLED", "true")))
    }

    pub fn stats_hll_precision(&self) -> u8 {
        self.get_stats_config().hll_precision.unwrap_or_else(|| {
            Config::getenv("STATS_HLL_PRECISION", "12")
                .parse::<u8>()
                .unwrap_or(12)
        })
    }

    pub fn stats_histogram_enabled(&self) -> bool {
        self.get_stats_config()
            .histogram_enabled
            .unwrap_or_else(|| {
                Config::truth_value(&Config::getenv("STATS_HISTOGRAM_ENABLED", "true"))
            })
    }

    pub async fn write_namespace_stats_async(
        &self,
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

        let tenant = self.get_tenant();
        let workspace = self.get_workspace_name();
        let pipeline = self.get_pipeline_name();

        let key = format!(
            "{}/{}/{}/stats/{}.json",
            tenant, workspace, pipeline, namespace
        );

        let storage = crate::adapters::storage::get_storage(self);
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

        let _ = crate::sqlrt::registry::ensure_ns_entry(self, &pipeline, namespace, |current| {
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
        &self,
        namespace: &str,
        stats: &crate::discover::stats::NamespaceStats,
    ) {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            // Already in a runtime: spawn fire-and-forget to avoid blocking
            let ns = namespace.to_string();
            let snapshot = stats.clone();
            let this = self.clone();
            handle.spawn(async move {
                this.write_namespace_stats_async(&ns, &snapshot).await;
            });
        } else {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(self.write_namespace_stats_async(namespace, stats));
        }
    }

    pub async fn read_namespace_stats_async(&self, namespace: &str) -> Option<serde_json::Value> {
        let tenant = self.get_tenant();
        let workspace = self.get_workspace_name();
        let pipeline = self.get_pipeline_name();
        let key = format!(
            "{}/{}/{}/stats/{}.json",
            tenant, workspace, pipeline, namespace
        );

        let storage = crate::adapters::storage::get_storage(self);
        match storage.get_json_opt(&key).await {
            Ok(v) => v,
            Err(e) => {
                debug!("read_namespace_stats_async: {}", e);
                None
            }
        }
    }

    pub fn stats_flush_seconds(&self) -> u64 {
        self.get_stats_config().flush_seconds.unwrap_or_else(|| {
            Config::getenv("STATS_FLUSH_SECONDS", "5")
                .parse::<u64>()
                .unwrap_or(5)
        })
    }

    // Deprecated local cache helpers removed (S3 is canonical)

    pub fn catalog_llm_enabled(&self) -> bool {
        Self::truth_value(&Self::getenv("CATALOG_LLM_ENABLED", "true"))
    }

    fn schema_sync_debounce_duration(&self) -> std::time::Duration {
        std::time::Duration::from_millis(
            Config::getenv("SCHEMA_SYNC_DEBOUNCE_MS", "400")
                .parse::<u64>()
                .unwrap_or(400),
        )
    }

    fn schema_sync_drain_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(
            Config::getenv("SCHEMA_SYNC_DRAIN_TIMEOUT_SECONDS", "30")
                .parse::<u64>()
                .unwrap_or(30),
        )
    }

    fn drain_available_schema_sync_requests(
        rx: &mut UnboundedReceiver<SchemaPublicationRequest>,
        dirty: &mut HashMap<String, SchemaPublicationRequest>,
    ) -> bool {
        let mut closed = false;
        loop {
            match rx.try_recv() {
                Ok(request) => {
                    let key = request.dirty_key();
                    match dirty.get(&key) {
                        Some(existing) if existing.version >= request.version => {}
                        _ => {
                            dirty.insert(key, request);
                        }
                    }
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    closed = true;
                    break;
                }
            }
        }
        closed
    }

    fn ensure_schema_sync_worker(&self) -> UnboundedSender<SchemaPublicationRequest> {
        let mut guard = SCHEMA_SYNC_WORKER.lock().unwrap();
        if let Some((tx, _, _)) = guard.as_ref() {
            return tx.clone();
        }
        let (tx, mut rx): (
            UnboundedSender<SchemaPublicationRequest>,
            UnboundedReceiver<SchemaPublicationRequest>,
        ) = unbounded_channel();

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let mut deadletter_plugins: HashMap<
                    String,
                    Box<dyn crate::plugins::SchemaSink + Send + Sync>,
                > = HashMap::new();
                let mut dirty: HashMap<String, SchemaPublicationRequest> = HashMap::new();

                loop {
                    if dirty.is_empty() {
                        match rx.recv().await {
                            Some(request) => {
                                dirty.insert(request.dirty_key(), request);
                            }
                            None => break,
                        }
                    }
                    let debounce = dirty
                        .values()
                        .next()
                        .map(|request| request.config.schema_sync_debounce_duration())
                        .unwrap_or_default();
                    if !debounce.is_zero() {
                        tokio::time::sleep(debounce).await;
                    }
                    let channel_closed =
                        Config::drain_available_schema_sync_requests(&mut rx, &mut dirty);
                    let mut requests: Vec<SchemaPublicationRequest> =
                        dirty.drain().map(|(_, request)| request).collect();
                    requests.sort_by(|left, right| {
                        left.dirty_key().cmp(&right.dirty_key())
                    });
                    if requests.len() > 1 {
                        info!(
                            "Schema sync: coalesced {} namespace update requests",
                            requests.len()
                        );
                    }
                    let sync_timeout = std::time::Duration::from_secs(
                        Config::getenv("SCHEMA_SYNC_TIMEOUT_SECONDS", "120")
                            .parse::<u64>()
                            .unwrap_or(120),
                    );
                    for request in requests {
                    let ns = request.namespace.clone();
                    let schema_version = request
                        .version
                        .max(Config::schema_publication_version(&request.config, &ns));
                    let flatten = request.config.get_transform_flatten_events();
                    let md_snapshot = { METADATA.load().metadata.clone() };
                    let out_meta = if let Some(schema) = md_snapshot.get(&ns) {
                        Some(if flatten {
                            OutputMetadata::from_flatterened_metadata_for_namespace(&ns, schema)
                        } else {
                            let mut out_meta = OutputMetadata::from_metadata(schema);
                            OutputMetadata::repair_field_identity(&ns, &mut out_meta);
                            out_meta
                        })
                    } else {
                        crate::runtime_plugins::schema_state::runtime_schema_output_metadata(&ns)
                    };
                    if let Some(out_meta) = out_meta {

                        let deadletter_namespace =
                            crate::ingest::deadletter::table_name(&request.config);
                        let is_deadletter_ns = ns == deadletter_namespace;
                        let scope = request.config.primary_schema_scope();

                        if !is_deadletter_ns {
                            if let Err(err) = Config::coordinate_primary_schema_sync(
                                &request.config,
                                &ns,
                                schema_version,
                                &out_meta,
                                sync_timeout,
                            )
                            .await
                            {
                                warn!("Schema sync: failed for namespace {}: {}", ns, err);
                            }
                        }

                        if is_deadletter_ns {
                            if !deadletter_plugins.contains_key(&scope) {
                                let dl_sink_cfg = request
                                    .config
                                    .get_pipeline_deadletter_plugin_config()
                                    .ok()
                                    .flatten();
                                match request.config.get_pipeline_deadletter_schema_plugin_name() {
                                    Ok(Some(schema_plugin_name)) => {
                                        let runtime_config = dl_sink_cfg
                                            .clone()
                                            .and_then(|cfg| crate::runtime_plugins::protocol::RuntimeSchemaConfig::try_from(cfg).ok());
                                        let runtime_version =
                                            match request.config.get_pipeline_deadletter_schema_plugin_version()
                                            {
                                                Ok(runtime_version) => runtime_version,
                                                Err(err) => {
                                                    warn!(
                                                        "Schema sync: failed to resolve runtime deadletter schema plugin version: {}",
                                                        err
                                                    );
                                                    None
                                                }
                                            };
                                        if let Some(runtime_config) = runtime_config {
                                            match crate::runtime_plugins::discovery::resolve_runtime_plugin(
                                                crate::runtime_plugins::protocol::RuntimePluginKind::SchemaSink,
                                                &schema_plugin_name,
                                                runtime_version.as_deref(),
                                            )
                                            .await
                                            {
                                                Ok(resolved) => {
                                                    match crate::runtime_plugins::host::RuntimeSchemaSinkPlugin::new(
                                                        &request.config,
                                                        resolved,
                                                        request.config.get_pipeline_name(),
                                                        crate::runtime_plugins::protocol::RuntimeBinding::Deadletter,
                                                        runtime_config,
                                                    )
                                                    .await
                                                    {
                                                        Ok(plugin) => {
                                                            deadletter_plugins.insert(scope.clone(), Box::new(plugin));
                                                        }
                                                        Err(err) => {
                                                            warn!("Schema sync: failed to initialize runtime deadletter schema plugin: {}", err);
                                                        }
                                                    }
                                                }
                                                Err(err) => {
                                                    warn!("Schema sync: failed to resolve runtime deadletter schema manifest: {}", err);
                                                }
                                            }
                                        } else {
                                            warn!("Schema sync: runtime schema plugin configured but the deadletter sink does not expose a runtime schema config");
                                        }
                                    }
                                    Ok(None) => {
                                        debug!("Schema sync: no schema sink configured for deadletter output");
                                    }
                                    Err(err) => {
                                        warn!("Schema sync: failed to resolve deadletter schema plugin: {}", err);
                                    }
                                }
                            }
                            if let Some(plugin) = deadletter_plugins.get(&scope) {
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
                    }
                    if channel_closed {
                        break;
                    }
                }
            });
            let _ = done_tx.send(());
        });
        *guard = Some((tx.clone(), Some(handle), done_rx));
        tx
    }

    /// Drop the sender to close the channel, then wait for the worker to flush
    /// pending schema updates. The drain is bounded so `--once` cannot look
    /// hung forever after data and compaction have already completed.
    pub fn drain_schema_sync_worker(&self) {
        let mut guard = SCHEMA_SYNC_WORKER.lock().unwrap();
        if let Some((tx, handle, done_rx)) = guard.take() {
            drop(tx);
            if let Some(h) = handle {
                let drain_timeout = Config::schema_sync_drain_timeout(self);
                info!(
                    "Schema sync: waiting up to {:?} for worker drain",
                    drain_timeout
                );
                match done_rx.recv_timeout(drain_timeout) {
                    Ok(()) => {
                        if let Err(err) = h.join() {
                            warn!(
                                "Schema sync: worker thread panicked during drain: {:?}",
                                err
                            );
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        warn!(
                            "Schema sync: drain timed out after {:?}; continuing shutdown",
                            drain_timeout
                        );
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        if let Err(err) = h.join() {
                            warn!(
                                "Schema sync: worker thread panicked during drain: {:?}",
                                err
                            );
                        }
                    }
                }
            }
        }
    }

    pub fn sync_output_schema_namespace(&self, namespace: &str) {
        let tx = Config::ensure_schema_sync_worker(self);
        let _ = tx.send(SchemaPublicationRequest {
            namespace: namespace.to_string(),
            version: Config::schema_publication_version(self, namespace),
            config: self.clone(),
        });
    }

    fn schema_publication_version(&self, namespace: &str) -> u64 {
        let namespace_version = crate::ingest_work::namespace_schema_version(namespace)
            .max(crate::runtime_plugins::schema_state::runtime_schema_namespace_version(namespace));
        if namespace_version == 0 {
            crate::runtime_plugins::schema_state::current_pipeline_schema_version().max(1)
        } else {
            namespace_version
        }
    }

    pub(crate) fn primary_schema_scope(&self) -> String {
        format!(
            "{}:{}:{}:{}:{}",
            self.get_pipeline_data_dir(),
            self.get_workspace_name(),
            self.get_pipeline_name(),
            self.get_pipeline_output_sink_ref(),
            self.get_pipeline_schema_plugin_name()
        )
    }

    pub(crate) fn schema_publication_identity(&self, namespace: &str) -> String {
        format!("{}:{}", self.primary_schema_scope(), namespace)
    }

    async fn blocking_primary_schema_plugin(
        &self,
        scope: &str,
    ) -> Result<Arc<dyn crate::plugins::SchemaSink + Send + Sync>, String> {
        if let Some((_, plugin)) = BLOCKING_PRIMARY_SCHEMA_PLUGIN
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(cached_scope, _)| cached_scope == scope)
            .cloned()
        {
            return Ok(plugin);
        }
        let _init_guard = PRIMARY_SCHEMA_PLUGIN_INIT.lock().await;
        if let Some((_, plugin)) = BLOCKING_PRIMARY_SCHEMA_PLUGIN
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(cached_scope, _)| cached_scope == scope)
            .cloned()
        {
            return Ok(plugin);
        }

        let schema_plugin_name = self.get_pipeline_schema_plugin_name();
        let runtime_version = self.get_pipeline_schema_plugin_version().map_err(|err| {
            format!(
                "Schema sync: failed to resolve runtime schema plugin version: {}",
                err
            )
        })?;
        let runtime_config = self
            .get_pipeline_output_plugin_config()
            .map_err(|err| {
                format!(
                    "Schema sync: failed to resolve output plugin config: {}",
                    err
                )
            })
            .and_then(|cfg| {
                crate::runtime_plugins::protocol::RuntimeSchemaConfig::try_from(cfg).map_err(
                    |err| {
                        format!(
                            "Schema sync: failed to derive runtime schema config: {}",
                            err
                        )
                    },
                )
            })?;

        let resolved = crate::runtime_plugins::discovery::resolve_runtime_plugin(
            crate::runtime_plugins::protocol::RuntimePluginKind::SchemaSink,
            &schema_plugin_name,
            runtime_version.as_deref(),
        )
        .await
        .map_err(|err| {
            format!(
                "Schema sync: failed to resolve runtime schema manifest: {}",
                err
            )
        })?;

        let plugin = crate::runtime_plugins::host::RuntimeSchemaSinkPlugin::new(
            self,
            resolved,
            self.get_pipeline_name(),
            crate::runtime_plugins::protocol::RuntimeBinding::Primary,
            runtime_config,
        )
        .await
        .map_err(|err| {
            format!(
                "Schema sync: failed to initialize runtime schema plugin: {}",
                err
            )
        })?;

        let plugin: Arc<dyn crate::plugins::SchemaSink + Send + Sync> = Arc::new(plugin);
        *BLOCKING_PRIMARY_SCHEMA_PLUGIN.lock().unwrap() = Some((scope.to_string(), plugin.clone()));
        Ok(plugin)
    }

    async fn coordinate_primary_schema_sync(
        &self,
        namespace: &str,
        schema_version: u64,
        out_meta: &OutputMetadata,
        sync_timeout: std::time::Duration,
    ) -> Result<(), String> {
        let scope = self.primary_schema_scope();
        SCHEMA_COORDINATOR
            .coordinate(&scope, namespace, schema_version, || async {
                let schema_plugin_name = self.get_pipeline_schema_plugin_name();
                if schema_plugin_name.is_empty() {
                    debug!("Schema sync: no schema sink configured for primary output");
                    return Ok(());
                }

                let plugin = self.blocking_primary_schema_plugin(&scope).await?;
                let source_contract = crate::METADATA
                    .load()
                    .source_contract_for_namespace(namespace);
                let schema_request = crate::plugins::SchemaSyncRequest {
                    namespace,
                    compaction_id: "",
                    source_contract: source_contract.as_ref(),
                };
                info!(
                    "Schema sync: updating namespace {} version {}",
                    namespace, schema_version
                );
                match tokio::time::timeout(
                    sync_timeout,
                    plugin.sync_schema_request(schema_request, out_meta),
                )
                .await
                {
                    Ok(Ok(_)) => {
                        info!(
                            "Schema sync: synced namespace {} version {}",
                            namespace, schema_version
                        );
                        Ok(())
                    }
                    Ok(Err(err)) => Err(format!(
                        "Schema sync: failed for namespace {}: {}",
                        namespace, err
                    )),
                    Err(_) => Err(format!(
                        "Schema sync: timed out for namespace {} after {:?}",
                        namespace, sync_timeout
                    )),
                }
            })
            .await
    }

    pub async fn sync_output_schema_namespace_blocking(
        &self,
        namespace: &str,
    ) -> Result<(), String> {
        let ns = namespace.trim();
        if ns.is_empty() {
            return Err("namespace must not be empty".to_string());
        }

        let schema_plugin_name = self.get_pipeline_schema_plugin_name();
        if schema_plugin_name.is_empty() {
            debug!("Schema sync: no schema sink configured for primary output");
            return Ok(());
        }

        let flatten = self.get_transform_flatten_events();
        let md_snapshot = { METADATA.load().metadata.clone() };
        let out_meta = if let Some(schema) = md_snapshot.get(ns) {
            if flatten {
                OutputMetadata::from_flatterened_metadata_for_namespace(ns, schema)
            } else {
                let mut out_meta = OutputMetadata::from_metadata(schema);
                OutputMetadata::repair_field_identity(ns, &mut out_meta);
                out_meta
            }
        } else {
            crate::runtime_plugins::schema_state::runtime_schema_output_metadata(ns)
                .ok_or_else(|| format!("Schema sync: namespace {} missing from metadata", ns))?
        };

        let sync_timeout = std::time::Duration::from_secs(
            Config::getenv("SCHEMA_SYNC_TIMEOUT_SECONDS", "120")
                .parse::<u64>()
                .unwrap_or(120),
        );
        let schema_version = Config::schema_publication_version(self, ns);
        self.coordinate_primary_schema_sync(ns, schema_version, &out_meta, sync_timeout)
            .await
    }

    pub async fn init(&self) {
        // Enforce reserved name policy early
        self.assert_pipeline_not_reserved();
        // Enforce config dependency rules before pipeline runtime starts.
        self.assert_config_dependencies_valid();

        if crate::helpers::configuration::DATA_DIR_INIT_ONCE
            .get()
            .is_none()
        {
            info!("Initializing data directories...");
        }

        let data_dir = self.get_data_dir();
        let segment_dir = &format!("{}/segment_buffer", data_dir);
        match fs::create_dir_all(segment_dir) {
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

    pub fn get_enable_single_quote_parsing(&self) -> bool {
        let env_value = Config::getenv("SKIPPR_ENABLE_SINGLE_QUOTE_PARSING", "false");

        if env_value.to_lowercase() == "true" {
            return true;
        }

        let config = self.clone();

        let pipeline = match config.pipelines.get(self.pipeline_key()) {
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

    pub fn get_enable_unicode_parsing(&self) -> bool {
        let env_value = Config::getenv("SKIPPR_ENABLE_UNICODE_PARSING", "false");

        if env_value.to_lowercase() == "true" {
            return true;
        }

        let config = self.clone();

        let pipeline = match config.pipelines.get(self.pipeline_key()) {
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
    pub fn compute_namespace_md5(&self, meta: &Metadata) -> String {
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

    pub fn pipeline_llm_enabled(&self) -> bool {
        // env override
        if Self::getenv("LLM_ENABLED", "").len() > 0 {
            return Self::truth_value(&Self::getenv("LLM_ENABLED", "true"));
        }
        // pipeline setting
        let cfg = self.clone();
        let pn = self.get_pipeline_name();
        if let Some(p) = cfg.pipelines.get(pn.as_str()) {
            if let Some(sl) = &p.semantic_layer {
                return sl.llm_enabled.unwrap_or(true);
            }
        }
        true
    }

    pub fn pipeline_llm_debounce_ms(&self) -> u64 {
        if let Ok(v) = Self::getenv("LLM_DEBOUNCE_MS", "").parse::<u64>() {
            return v;
        }
        let cfg = self.clone();
        let pn = self.get_pipeline_name();
        if let Some(p) = cfg.pipelines.get(pn.as_str()) {
            if let Some(sl) = &p.semantic_layer {
                return sl.llm_debounce_ms.unwrap_or(1500);
            }
        }
        1500
    }

    pub fn get_pipeline_cache_dir(&self) -> String {
        // get_data_dir() is already the per-pipeline root (no extra suffix).
        let path = self.get_data_dir();
        let _ = std::fs::create_dir_all(&path);
        path
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use serial_test::serial;

    use super::*;

    fn schema_sync_test_config(data_dir: &str) -> Config {
        let config: Config = serde_json::from_value(serde_json::json!({
            "skippr": { "workspace": "quickstart" },
            "pipelines": {
                "orders": {
                    "data_source": "data_sources.sample",
                    "data_dir": data_dir
                }
            },
            "data_sources": {
                "sample": { "S3": { "s3_bucket": "b", "s3_prefix": "p" } }
            }
        }))
        .unwrap();
        config.bind_pipeline("orders")
    }

    #[test]
    #[serial]
    fn getenv_reads_envcache_when_process_env_is_unset() {
        std::env::remove_var("TRANSFORM_BATCH_PARTITION_FIELDS");
        Config::reset_envcache();
        Config::set_evncache("TRANSFORM_BATCH_PARTITION_FIELDS", "foo");
        assert_eq!(
            Config::getenv("TRANSFORM_BATCH_PARTITION_FIELDS", "NULL_VALUE"),
            "foo"
        );
        Config::reset_envcache();
    }

    #[test]
    fn schema_sync_worker_does_not_freeze_first_caller_config() {
        let src = include_str!("configuration.rs");
        let (head, worker_and_rest) = src
            .split_once("fn ensure_schema_sync_worker")
            .expect("ensure_schema_sync_worker");
        let worker = worker_and_rest
            .split("pub fn drain_schema_sync_worker")
            .next()
            .unwrap();
        assert!(
            !worker.contains("let this = self.clone()"),
            "schema sync worker must not capture the first caller's Config"
        );
        let request_struct = head
            .split("struct SchemaPublicationRequest")
            .nth(1)
            .unwrap()
            .split('}')
            .next()
            .unwrap();
        assert!(
            request_struct.contains("config: Config"),
            "SchemaPublicationRequest must carry the caller's Config"
        );
        assert!(
            worker.contains("request.config"),
            "schema sync worker must apply the request's Config, not a spawn freeze"
        );
    }

    #[test]
    fn primary_schema_scope_isolates_session_data_dir() {
        let a = schema_sync_test_config("/tmp/skipprd-schema-a");
        let b = schema_sync_test_config("/tmp/skipprd-schema-b");
        assert_ne!(
            a.primary_schema_scope(),
            b.primary_schema_scope(),
            "two Sessions with different data_dirs must not share a schema-sync scope"
        );
        assert!(a.primary_schema_scope().contains("/tmp/skipprd-schema-a"));
        assert!(b.primary_schema_scope().contains("/tmp/skipprd-schema-b"));
    }

    #[test]
    fn schema_sync_request_drain_coalesces_duplicate_namespaces() {
        let config = schema_sync_test_config("/tmp/skipprd-schema-a");
        let (tx, mut rx) = unbounded_channel();
        tx.send(SchemaPublicationRequest {
            namespace: "events".to_string(),
            version: 1,
            config: config.clone(),
        })
        .unwrap();
        tx.send(SchemaPublicationRequest {
            namespace: "events".to_string(),
            version: 2,
            config: config.clone(),
        })
        .unwrap();
        tx.send(SchemaPublicationRequest {
            namespace: "users".to_string(),
            version: 1,
            config: config.clone(),
        })
        .unwrap();

        let mut dirty = HashMap::new();
        let closed = Config::drain_available_schema_sync_requests(&mut rx, &mut dirty);

        assert!(!closed);
        assert_eq!(dirty.len(), 2);
        let events = dirty
            .values()
            .find(|request| request.namespace == "events")
            .unwrap();
        let users = dirty
            .values()
            .find(|request| request.namespace == "users")
            .unwrap();
        assert_eq!(events.version, 2);
        assert_eq!(users.version, 1);
        assert_eq!(
            events.config.get_pipeline_data_dir(),
            "/tmp/skipprd-schema-a"
        );
    }

    #[test]
    fn schema_sync_request_drain_keeps_two_session_configs() {
        let a = schema_sync_test_config("/tmp/skipprd-schema-a");
        let b = schema_sync_test_config("/tmp/skipprd-schema-b");
        let (tx, mut rx) = unbounded_channel();
        tx.send(SchemaPublicationRequest {
            namespace: "events".to_string(),
            version: 1,
            config: a,
        })
        .unwrap();
        tx.send(SchemaPublicationRequest {
            namespace: "events".to_string(),
            version: 1,
            config: b,
        })
        .unwrap();

        let mut dirty = HashMap::new();
        let closed = Config::drain_available_schema_sync_requests(&mut rx, &mut dirty);

        assert!(!closed);
        assert_eq!(dirty.len(), 2);
        let mut dirs: Vec<String> = dirty
            .values()
            .map(|request| request.config.get_pipeline_data_dir())
            .collect();
        dirs.sort();
        assert_eq!(
            dirs,
            vec![
                "/tmp/skipprd-schema-a".to_string(),
                "/tmp/skipprd-schema-b".to_string()
            ]
        );
    }

    #[test]
    fn schema_sync_request_drain_flushes_when_sender_closes() {
        let config = schema_sync_test_config("/tmp/skipprd-schema-a");
        let (tx, mut rx) = unbounded_channel();
        tx.send(SchemaPublicationRequest {
            namespace: "events".to_string(),
            version: 1,
            config: config.clone(),
        })
        .unwrap();
        tx.send(SchemaPublicationRequest {
            namespace: "events".to_string(),
            version: 1,
            config,
        })
        .unwrap();
        drop(tx);

        let mut dirty = HashMap::new();
        let closed = Config::drain_available_schema_sync_requests(&mut rx, &mut dirty);

        assert!(closed);
        assert_eq!(dirty.len(), 1);
        let events = dirty
            .values()
            .find(|request| request.namespace == "events")
            .unwrap();
        assert_eq!(events.version, 1);
    }

    #[test]
    #[serial]
    fn schema_sync_drain_timeout_is_env_configurable() {
        let original = std::env::var("SCHEMA_SYNC_DRAIN_TIMEOUT_SECONDS").ok();
        ENV_CACHE.write().clear();
        std::env::set_var("SCHEMA_SYNC_DRAIN_TIMEOUT_SECONDS", "7");

        assert_eq!(
            Config::new().schema_sync_drain_timeout(),
            std::time::Duration::from_secs(7)
        );

        match original {
            Some(value) => std::env::set_var("SCHEMA_SYNC_DRAIN_TIMEOUT_SECONDS", value),
            None => std::env::remove_var("SCHEMA_SYNC_DRAIN_TIMEOUT_SECONDS"),
        }
        ENV_CACHE.write().clear();
    }

    #[test]
    #[serial]
    fn resolves_whole_value_env_refs_recursively() {
        std::env::set_var("SKIPPR_TEST_CONNECTION", "server=tcp:127.0.0.1");
        std::env::set_var("SKIPPR_TEST_TABLE", "dbo.customers");
        std::env::set_var("SKIPPR_TEST_ACCOUNT", "ACCT");

        let mut value = json!({
            "data_sources": {
                "mssql": {
                    "Mssql": {
                        "connection_string": "${SKIPPR_TEST_CONNECTION}",
                        "tables": ["${SKIPPR_TEST_TABLE}", "dbo.orders"],
                        "literal": "prefix ${SKIPPR_TEST_TABLE}"
                    }
                }
            },
            "data_sinks": {
                "snowflake": {
                    "Snowflake": {
                        "account": "${SKIPPR_TEST_ACCOUNT}"
                    }
                }
            }
        });

        Config::resolve_env_refs_in_json_value(&mut value).expect("resolve env refs");

        assert_eq!(
            value["data_sources"]["mssql"]["Mssql"]["connection_string"],
            "server=tcp:127.0.0.1"
        );
        assert_eq!(
            value["data_sources"]["mssql"]["Mssql"]["tables"][0],
            "dbo.customers"
        );
        assert_eq!(
            value["data_sources"]["mssql"]["Mssql"]["literal"],
            "prefix ${SKIPPR_TEST_TABLE}"
        );
        assert_eq!(
            value["data_sinks"]["snowflake"]["Snowflake"]["account"],
            "ACCT"
        );

        std::env::remove_var("SKIPPR_TEST_CONNECTION");
        std::env::remove_var("SKIPPR_TEST_TABLE");
        std::env::remove_var("SKIPPR_TEST_ACCOUNT");
    }

    #[test]
    #[serial]
    fn missing_env_ref_reports_config_path() {
        std::env::remove_var("SKIPPR_TEST_MISSING");
        let mut value = json!({
            "data_sources": {
                "mssql": {
                    "Mssql": {
                        "connection_string": "${SKIPPR_TEST_MISSING}"
                    }
                }
            }
        });

        let err = Config::resolve_env_refs_in_json_value(&mut value).expect_err("missing var");

        assert!(err.contains("${SKIPPR_TEST_MISSING}"));
        assert!(err.contains("data_sources.mssql.Mssql.connection_string"));
    }

    #[test]
    #[serial]
    fn build_config_resolves_customer_style_plugin_env_refs() {
        let original_config_file = std::env::var("SKIPPR_CONFIG_FILE").ok();
        let original_connection = std::env::var("MSSQL_CONNECTION_STRING").ok();
        let original_account = std::env::var("SNOWFLAKE_ACCOUNT").ok();
        ENV_CACHE.write().clear();

        std::env::set_var(
            "MSSQL_CONNECTION_STRING",
            "server=tcp:127.0.0.1,1433;database=testdb",
        );
        std::env::set_var("SNOWFLAKE_ACCOUNT", "ACCT");

        let config_path = std::env::temp_dir().join(format!(
            "skippr-customer-style-{}.yml",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(
            &config_path,
            r#"
skippr:
  workspace: mssql_migration

pipelines:
  mssql-migration:
    data_source: data_sources.mssql
    data_sink: data_sinks.snowflake

data_sources:
  mssql:
    Mssql:
      connection_string: ${MSSQL_CONNECTION_STRING}
      tables: ["dbo.customers"]

data_sinks:
  snowflake:
    Snowflake:
      account: ${SNOWFLAKE_ACCOUNT}
      user: test_user
      database: ANALYTICS
      schema: RAW
      warehouse: COMPUTE_WH
"#,
        )
        .expect("write config");
        std::env::set_var("SKIPPR_CONFIG_FILE", &config_path);
        let config = Config::build_config().bind_pipeline("mssql-migration");

        let input = config
            .get_pipeline_input_plugin_config()
            .expect("input config");
        let output = config
            .get_pipeline_output_plugin_config()
            .expect("output config");
        assert_eq!(input.plugin_name, "Mssql");
        assert_eq!(
            input
                .config
                .get("connection_string")
                .and_then(|value| value.as_str()),
            Some("server=tcp:127.0.0.1,1433;database=testdb")
        );
        assert_eq!(output.plugin_name, "Snowflake");
        assert_eq!(
            output
                .config
                .get("account")
                .and_then(|value| value.as_str()),
            Some("ACCT")
        );

        let _ = std::fs::remove_file(&config_path);

        match original_config_file {
            Some(value) => std::env::set_var("SKIPPR_CONFIG_FILE", value),
            None => std::env::remove_var("SKIPPR_CONFIG_FILE"),
        }
        match original_connection {
            Some(value) => std::env::set_var("MSSQL_CONNECTION_STRING", value),
            None => std::env::remove_var("MSSQL_CONNECTION_STRING"),
        }
        match original_account {
            Some(value) => std::env::set_var("SNOWFLAKE_ACCOUNT", value),
            None => std::env::remove_var("SNOWFLAKE_ACCOUNT"),
        }
        ENV_CACHE.write().clear();
    }

    // Tests for our new JSON parsing configuration options
    #[test]
    fn test_enable_single_quote_parsing() {
        // Test environment variable override
        std::env::set_var("SKIPPR_ENABLE_SINGLE_QUOTE_PARSING", "true");
        assert_eq!(Config::new().get_enable_single_quote_parsing(), true);

        std::env::set_var("SKIPPR_ENABLE_SINGLE_QUOTE_PARSING", "false");
        assert_eq!(Config::new().get_enable_single_quote_parsing(), false);

        // Clean up
        std::env::remove_var("SKIPPR_ENABLE_SINGLE_QUOTE_PARSING");
    }

    #[test]
    fn test_enable_unicode_parsing() {
        // Test environment variable override
        std::env::set_var("SKIPPR_ENABLE_UNICODE_PARSING", "true");
        assert_eq!(Config::new().get_enable_unicode_parsing(), true);

        std::env::set_var("SKIPPR_ENABLE_UNICODE_PARSING", "false");
        assert_eq!(Config::new().get_enable_unicode_parsing(), false);

        // Clean up
        std::env::remove_var("SKIPPR_ENABLE_UNICODE_PARSING");
    }

    #[test]
    #[serial]
    fn offset_store_partition_key_is_derived() {
        ENV_CACHE.write().clear();
        std::env::set_var("TENANT", "tenant-a");
        std::env::set_var("WORKSPACE_NAME", "workspace-b");
        let config = Config::new().bind_pipeline("google_analytics");
        assert_eq!(
            config.offset_store_partition_key(),
            "tenant-a#workspace-b#google_analytics"
        );
        std::env::remove_var("TENANT");
        std::env::remove_var("WORKSPACE_NAME");
    }

    #[test]
    fn skipprd_el_storage_mode_uses_explicit_name() {
        let original_env = std::env::var("SKIPPRD_EL_STORAGE_MODE").ok();
        ENV_CACHE.write().clear();
        std::env::remove_var("SKIPPRD_EL_STORAGE_MODE");

        let mut config = Config::new();
        config.skippr = Some(Skippr {
            workspace: None,
            tenant: None,
            skippr_s3_bucket: None,
            skipprd_el_storage_mode: Some("local".to_string()),
            wal_s3_bucket: None,
            offset_store: None,
            offset_dynamodb_table: None,
        });

        assert_eq!(config.get_storage_mode(), "local");

        match original_env {
            Some(value) => std::env::set_var("SKIPPRD_EL_STORAGE_MODE", value),
            None => std::env::remove_var("SKIPPRD_EL_STORAGE_MODE"),
        }
        ENV_CACHE.write().clear();
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
            cdc: None,
        };
        let config = Config {
            skippr: Some(Skippr {
                workspace: None,
                tenant: None,
                skippr_s3_bucket: None,
                skipprd_el_storage_mode: None,
                wal_s3_bucket: None,
                offset_store: None,
                offset_dynamodb_table: None,
            }),
            pipelines: HashMap::new(),
            data_sources: None,
            data_sinks: None,
            deadletter_sinks: Some(HashMap::new()),
            schema_sinks: None,
            dbt: None,
            vector_sources: None,
            active_pipeline: None,
        };

        assert!(
            Config::resolve_deadletter_plugin_config_for(&config, &pipeline)
                .unwrap()
                .is_none()
        );
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
            cdc: None,
        };
        let config = Config {
            skippr: Some(Skippr {
                workspace: None,
                tenant: None,
                skippr_s3_bucket: None,
                skipprd_el_storage_mode: None,
                wal_s3_bucket: None,
                offset_store: None,
                offset_dynamodb_table: None,
            }),
            pipelines: HashMap::new(),
            data_sources: None,
            data_sinks: None,
            deadletter_sinks: Some(HashMap::new()),
            schema_sinks: None,
            dbt: None,
            vector_sources: None,
            active_pipeline: None,
        };

        assert!(Config::resolve_deadletter_plugin_config_for(&config, &pipeline).is_err());
        assert!(!Config::deadletter_config_violations_for(&config, &pipeline).is_empty());
    }

    #[test]
    fn malformed_pipeline_registry_ref_returns_friendly_error() {
        let config: Config = serde_json::from_value(json!({
            "skippr": {
                "workspace": "default"
            },
            "pipelines": {
                "bike_hire": {
                    "data_source": "s3_bike_hire",
                    "data_sink": "data_sinks.snowflake"
                }
            },
            "data_sources": {
                "s3_bike_hire": {
                    "S3": {
                        "s3_bucket": "bucket",
                        "s3_prefix": "prefix/"
                    }
                }
            },
            "data_sinks": {
                "snowflake": {
                    "Snowflake": {
                        "account": "acct"
                    }
                }
            }
        }))
        .unwrap();
        let pipeline = config.pipelines.get("bike_hire").unwrap();

        let err = Config::validate_pipeline_registry_refs_for(&config, "bike_hire", pipeline)
            .expect_err("unqualified data_source should be rejected");

        assert!(err.contains("pipeline 'bike_hire'"));
        assert!(err.contains("data_source"));
        assert!(err.contains("Expected 'data_sources.<name>'"));
    }

    #[test]
    fn missing_pipeline_input_config_names_missing_registry_entry() {
        ENV_CACHE.write().clear();

        let config: Config = serde_json::from_value(json!({
            "skippr": {
                "workspace": "default"
            },
            "pipelines": {
                "bike_hire": {
                    "data_source": "data_sources.missing",
                    "data_sink": "data_sinks.output"
                }
            },
            "data_sources": {},
            "data_sinks": {
                "output": {
                    "Snowflake": {
                        "account": "acct"
                    }
                }
            }
        }))
        .unwrap();

        let config = config.bind_pipeline("bike_hire");
        let err = config
            .get_pipeline_input_plugin_config()
            .expect_err("missing input registry entry should be rejected");

        assert!(err.contains("pipeline 'bike_hire'"));
        assert!(err.contains("data_sources.missing"));
        assert!(err.contains("not defined in data_sources"));

        ENV_CACHE.write().clear();
    }

    #[test]
    #[serial]
    fn invalid_numeric_env_is_reported_as_config_violation() {
        let original = std::env::var("BUFFER_THRESHOLD_BYTES").ok();
        ENV_CACHE.write().clear();
        std::env::set_var("BUFFER_THRESHOLD_BYTES", "lots");

        let violations = Config::new().get_config_dependency_violations();

        assert!(violations.iter().any(|violation| {
            violation.contains("BUFFER_THRESHOLD_BYTES")
                && violation.contains("Expected an unsigned integer")
        }));

        match original {
            Some(value) => std::env::set_var("BUFFER_THRESHOLD_BYTES", value),
            None => std::env::remove_var("BUFFER_THRESHOLD_BYTES"),
        }
        ENV_CACHE.write().clear();
    }

    #[test]
    #[serial]
    fn wal_bytes_per_file_defaults_to_pipeline_buffer_target() {
        let original_wal_bytes = std::env::var("WAL_BYTES_PER_FILE").ok();
        let original_buffer_bytes = std::env::var("BUFFER_THRESHOLD_BYTES").ok();
        ENV_CACHE.write().clear();
        std::env::remove_var("WAL_BYTES_PER_FILE");
        std::env::remove_var("BUFFER_THRESHOLD_BYTES");

        let config: Config = serde_yaml::from_str(
            r#"
skippr:
  workspace: default
pipelines:
  small:
    buffer_threshold_bytes: 1024
  bike_hire:
    buffer_threshold_bytes: 20000000
  large:
    buffer_threshold_bytes: 134217728
"#,
        )
        .unwrap();

        ENV_CACHE.write().clear();
        assert_eq!(
            config.bind_pipeline("small").get_wal_bytes_per_file(),
            4 * 1024 * 1024
        );

        ENV_CACHE.write().clear();
        assert_eq!(
            config.bind_pipeline("bike_hire").get_wal_bytes_per_file(),
            20_000_000
        );

        ENV_CACHE.write().clear();
        assert_eq!(
            config.bind_pipeline("large").get_wal_bytes_per_file(),
            64 * 1024 * 1024
        );

        match original_wal_bytes {
            Some(value) => std::env::set_var("WAL_BYTES_PER_FILE", value),
            None => std::env::remove_var("WAL_BYTES_PER_FILE"),
        }
        match original_buffer_bytes {
            Some(value) => std::env::set_var("BUFFER_THRESHOLD_BYTES", value),
            None => std::env::remove_var("BUFFER_THRESHOLD_BYTES"),
        }
        ENV_CACHE.write().clear();
    }

    #[test]
    #[serial]
    fn wal_bytes_per_file_env_override_wins() {
        let original_wal_bytes = std::env::var("WAL_BYTES_PER_FILE").ok();
        ENV_CACHE.write().clear();
        std::env::set_var("WAL_BYTES_PER_FILE", "8388608");

        let config: Config = serde_yaml::from_str(
            r#"
skippr:
  workspace: default
pipelines:
  bike_hire:
    buffer_threshold_bytes: 20000000
"#,
        )
        .unwrap();

        assert_eq!(
            config.bind_pipeline("bike_hire").get_wal_bytes_per_file(),
            8 * 1024 * 1024
        );

        match original_wal_bytes {
            Some(value) => std::env::set_var("WAL_BYTES_PER_FILE", value),
            None => std::env::remove_var("WAL_BYTES_PER_FILE"),
        }
        ENV_CACHE.write().clear();
    }

    #[test]
    #[serial]
    fn invalid_config_file_parse_returns_friendly_error() {
        let original_config_file = std::env::var("SKIPPR_CONFIG_FILE").ok();
        let tempdir = tempfile::tempdir().expect("create tempdir");
        let config_path = tempdir.path().join("skippr.yml");
        std::fs::write(&config_path, "skippr:\n  workspace: [").expect("write config");
        std::env::set_var("SKIPPR_CONFIG_FILE", &config_path);

        let err = Config::try_build_config().expect_err("invalid yaml should be rejected");

        assert!(err.contains("Failed to parse YAML config"));
        assert!(err.contains(config_path.to_string_lossy().as_ref()));

        match original_config_file {
            Some(value) => std::env::set_var("SKIPPR_CONFIG_FILE", value),
            None => std::env::remove_var("SKIPPR_CONFIG_FILE"),
        }
        ENV_CACHE.write().clear();
    }

    #[test]
    #[serial]
    fn plugin_versions_are_resolved_per_active_plugin_config() {
        ENV_CACHE.write().clear();

        let config: Config = serde_yaml::from_str(
            r#"
skippr:
  workspace: default
  tenant: default
  skippr_s3_bucket: skippr-e2e-sample-data-output
pipelines:
  bike_hire:
    data_source: data_sources.input
    data_sink: data_sinks.output
    deadletter_sink: deadletter_sinks.deadletters
data_sources:
  input:
    S3:
      version: 1.2.3
      s3_bucket: source-bucket
      s3_prefix: input/
data_sinks:
  output:
    schema_sink: schema_sinks.output_schema
    Athena:
      version: 2.3.4
      athena_workgroup_name: bikehire
      s3_bucket: output-bucket
      athena_results_s3_bucket: output-bucket
      s3_prefix: bikehire
deadletter_sinks:
  deadletters:
    schema_sink: schema_sinks.deadletter_schema
    S3:
      version: 4.5.6
      s3_bucket: deadletters-bucket
      s3_prefix: deadletters/
schema_sinks:
  output_schema:
    Glue:
      version: 3.4.5
      glue_database_name: datalake
  deadletter_schema:
    Glue:
      version: 5.6.7
      glue_database_name: deadletters
"#,
        )
        .unwrap();

        assert_eq!(
            config
                .bind_pipeline("bike_hire")
                .get_pipeline_input_plugin_version()
                .unwrap()
                .as_deref(),
            Some("1.2.3")
        );
        assert_eq!(
            config
                .bind_pipeline("bike_hire")
                .get_pipeline_output_plugin_version()
                .unwrap()
                .as_deref(),
            Some("2.3.4")
        );
        assert_eq!(
            config
                .bind_pipeline("bike_hire")
                .get_pipeline_schema_plugin_version()
                .unwrap()
                .as_deref(),
            Some("3.4.5")
        );
        assert_eq!(
            config
                .bind_pipeline("bike_hire")
                .get_pipeline_deadletter_plugin_version()
                .unwrap()
                .as_deref(),
            Some("4.5.6")
        );
        assert_eq!(
            config
                .bind_pipeline("bike_hire")
                .get_pipeline_deadletter_schema_plugin_version()
                .unwrap()
                .as_deref(),
            Some("5.6.7")
        );

        ENV_CACHE.write().clear();
    }

    #[test]
    #[serial]
    fn wal_storage_cli_overrides_env() {
        ENV_CACHE.write().clear();
        let original = std::env::var("WAL_STORAGE").ok();
        std::env::set_var("WAL_STORAGE", "disk");
        Config::set_wal_storage("s3");
        assert_eq!(
            Config::parse_wal_storage().unwrap(),
            crate::helpers::wal_storage::WalStorage::S3
        );
        match original {
            Some(value) => std::env::set_var("WAL_STORAGE", value),
            None => std::env::remove_var("WAL_STORAGE"),
        }
        ENV_CACHE.write().clear();
    }

    #[test]
    #[serial]
    fn unknown_wal_storage_is_rejected() {
        ENV_CACHE.write().clear();
        let original = std::env::var("WAL_STORAGE").ok();
        std::env::set_var("WAL_STORAGE", "memory");
        Config::set_evncache("WAL_STORAGE", "memory");
        assert!(matches!(
            Config::parse_wal_storage(),
            Err(crate::helpers::wal_storage::ConfigError::InvalidWalStorage(
                _
            ))
        ));
        match original {
            Some(value) => std::env::set_var("WAL_STORAGE", value),
            None => std::env::remove_var("WAL_STORAGE"),
        }
        ENV_CACHE.write().clear();
    }

    fn parse_skippr_yml_like_skipprd(yaml: &str) -> Result<Config, String> {
        let config: serde_value::Value = serde_yaml::from_str(yaml)
            .map_err(|err| format!("Failed to parse YAML config: {err}"))?;
        let string_val = serde_json::to_string(&config)
            .map_err(|err| format!("Failed to normalize config: {err}"))?;
        serde_json::from_str(&string_val)
            .map_err(|err| format!("Invalid Skippr configuration: {err}"))
    }

    #[test]
    fn rust_line_continuation_stripped_yaml_is_rejected() {
        // Live WAL dump from gen 42 host-0: Rust `"\n\"` stripped every YAML indent.
        let yaml = "skippr:\nworkspace: platform\ntenant: system\n\npipelines:\notel-traces:\ndata_source: data_sources.otlp_traces\ndata_sink: data_sinks.lake\n";
        let err = parse_skippr_yml_like_skipprd(yaml).expect_err("stripped YAML must not parse");
        assert!(
            err.contains("null") && err.contains("map"),
            "expected null-map Config error, got {err}"
        );
    }

    #[test]
    fn platform_otel_cookbook_schema_sink_sibling_yaml_parses() {
        let yaml = include_str!("../../examples/otel/skippr.yml");
        let config = parse_skippr_yml_like_skipprd(yaml).expect("cookbook skippr.yml must parse");
        let lake = config
            .data_sinks
            .as_ref()
            .and_then(|sinks| sinks.get("lake"))
            .expect("data_sinks.lake");
        assert_eq!(
            lake.schema_sink.as_deref(),
            Some("schema_sinks.iceberg_catalog")
        );
    }

    #[test]
    fn pipeline_without_data_sink_validates() {
        let config: Config = serde_json::from_value(json!({
            "skippr": { "workspace": "quickstart", "skipprd_el_storage_mode": "local" },
            "pipelines": {
                "bikehire": { "data_source": "data_sources.sample" }
            },
            "data_sources": {
                "sample": { "S3": { "s3_bucket": "skippr-public-sample-data", "s3_prefix": "bike-hire" } }
            }
        }))
        .unwrap();
        let pipeline = config.pipelines.get("bikehire").unwrap();
        Config::validate_pipeline_registry_refs_for(&config, "bikehire", pipeline)
            .expect("data_sink is optional");
    }

    #[test]
    fn pipeline_without_data_source_still_fails() {
        let config: Config = serde_json::from_value(json!({
            "skippr": { "workspace": "quickstart" },
            "pipelines": {
                "bikehire": { "data_sink": "data_sinks.warehouse" }
            },
            "data_sinks": {
                "warehouse": { "Snowflake": { "account": "acct" } }
            }
        }))
        .unwrap();
        let pipeline = config.pipelines.get("bikehire").unwrap();
        let err = Config::validate_pipeline_registry_refs_for(&config, "bikehire", pipeline)
            .expect_err("data_source remains required");
        assert!(err.contains("data_source is required"));
    }
}
