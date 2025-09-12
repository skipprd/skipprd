use std::collections::HashMap;
use std::fmt::{Debug};
use std::fs;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};

use std::path::Path;

use std::sync::{Arc};
use yaml_rust::YamlLoader;

use dashmap::DashMap;
use ini::configparser::ini::Ini;
use lazy_static::lazy_static;
use once_cell::sync::Lazy;

// use aws_config::profile::profile_file::ProfileFileKind::Config;
use serde_derive::{Deserialize};

use serde_json::{Value};

use crate::discover::{Metadata, OutputMetadata, PipelineMetadata};
use crate::{METADATA};


use crate::helpers::s3;

use crate::plugins::athena::{AwsAthena, DataOutputAwsAthenaPluginConfig};

use toml;
use crate::helpers::timed_rwlock::TimedRwLock;
use crate::ingest::fast_ingest::{create_default_nested_message, DEFAULT_NESTED_MESSAGE};
use crate::ingest_work::Ingest;
use tokio::sync::mpsc::{UnboundedSender, UnboundedReceiver, unbounded_channel};
use crate::plugins::file_input::{DataSourceLocalFilePluginConfig};
use crate::plugins::s3_input::DataSourceS3PluginConfig;
// use crate::plugins::s3_inventory::{DataSourceS3InventoryPluginConfig};

lazy_static! {
    static ref ENV_CACHE: TimedRwLock<DashMap<String, String>> = TimedRwLock::new("env_cache".to_string(), DashMap::new());
}

const DEFAULT_CONFIG: &'static str = "NULL_VALUE";

#[derive(Debug, Deserialize, Clone)]
pub struct Skippr {
    pub api_token: Option<String>,
    pub workspace: Option<String>,
    pub tenant: Option<String>,
    pub skippr_s3_bucket: Option<String>,
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
}

#[derive(Debug, Deserialize, Clone)]
pub enum PluginConfig {
    S3(DataSourceS3PluginConfig),
    // s3_inventory(DataSourceS3InventoryPluginConfig),
    Athena(DataOutputAwsAthenaPluginConfig),
    File(DataSourceLocalFilePluginConfig),
}

impl PluginConfig {
    pub fn format(&self) -> String {
        match self {
            PluginConfig::S3(s3_config) => s3_config.format.clone().or(Some("json".to_string())).as_ref().unwrap().clone(),
            // PluginConfig::s3_inventory(s3_inventory_config) => s3_inventory_config.format.clone().or(Some("json".to_string())).as_ref().unwrap().clone(),
            PluginConfig::Athena(athena_config) => athena_config.format.clone().or(Some("json".to_string())).as_ref().unwrap().clone(),
            PluginConfig::File(file_config) => file_config.format.clone().or(Some("json".to_string())).unwrap(),
        }
    }

    pub fn plugin_name(&self) -> Option<String> {
        match self {
            PluginConfig::S3(_s3_config) => Some("S3".to_string()),
            // PluginConfig::s3_inventory(_s3_inventory_config) => Some("s3_inventory".to_string()),
            PluginConfig::Athena(_athena_config) => Some("Athena".to_string()),
            PluginConfig::File(_file_config) => Some("File".to_string()),
        }
    }

    pub fn batch_size_bytes(&self) -> Option<i64> {
        match self {
            PluginConfig::S3(s3_config) => s3_config.batch_size_bytes.clone(),
            // PluginConfig::s3_inventory(s3_inventory_config) => s3_inventory_config.batch_size_bytes.clone(),
            PluginConfig::Athena(_athena_config) => None,
            PluginConfig::File(file_config) => file_config.batch_size_bytes.clone(),
        }
    }

    pub fn batch_size_seconds(&self) -> Option<i64> {
        match self {
            PluginConfig::S3(s3_config) => s3_config.batch_size_seconds.clone(),
            // PluginConfig::s3_inventory(s3_inventory_config) => s3_inventory_config.batch_size_seconds.clone(),
            PluginConfig::Athena(_athena_config) => None,
            PluginConfig::File(file_config) => file_config.batch_size_seconds.clone(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct Pipeline {
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
    pub input: Option<String>,
    pub output: Option<String>,
    pub schema: Option<String>,
    pub deadletter: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub skippr: Option<Skippr>,
    pub pipelines: HashMap<String, Pipeline>,
    pub data_inputs: Option<HashMap<String, PluginConfig>>,
    pub data_outputs: Option<HashMap<String, PluginConfig>>,
    pub data_deadletters: Option<HashMap<String, PluginConfig>>,
    pub schema_outputs: Option<HashMap<String, PluginConfig>>,
}

pub static APP_CONFIG: Lazy<Arc<TimedRwLock<Option<Config>>>> = Lazy::new(|| Arc::new(TimedRwLock::new("config".to_string(),None)));
pub static PIPELINE_NAME: Lazy<Arc<TimedRwLock<String>>> = Lazy::new(|| Arc::new(TimedRwLock::new("pipeline_name".to_string(),"default".to_string())));

#[allow(dead_code)]
impl Config {

    pub fn new() -> Config {
        Config {
            skippr: Some(Skippr {
                api_token: None,
                workspace: None,
                tenant: None,
                skippr_s3_bucket: None,
            }),
            pipelines: HashMap::new(),
            data_inputs: None,
            data_outputs: None,
            data_deadletters: None,
            schema_outputs: None,
        }
    }

    pub fn find_config_file() -> String {

        let config_file = Config::getenv("SKIPPR_CONFIG_FILE", "");

        if config_file != "" {
            if Path::new(&config_file).exists() {
                return config_file.to_string()
            }
        }

        let valid_locations = vec![
            "./skippr.yml",
            "./skippr.yaml",
            "./skippr.toml",
            "./skippr.json"
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

    fn parse_skippr_profile() {

        // get SKIPPR_PROFILE env var
        let profile_name = Config::getenv("SKIPPR_PROFILE", "default");

        // Parse credentials file
        let credentials_file_path = format!("{}/.skippr/credentials", std::env::var("HOME").unwrap_or("~".to_string()));
        let credentials_file_contents = fs::read_to_string(&credentials_file_path).unwrap_or(String::new());

        if credentials_file_contents.is_empty() {
            println!("No credentials file found at {}", credentials_file_path);
            return;
        }

        let mut ini = Ini::new();
        ini.read(credentials_file_contents).unwrap_or_else(|_| {
            panic!("Error parsing credentials file");
        });

        if ini.sections().contains(&profile_name) == false {
            if profile_name != "default" {
                panic!("Profile '{}' not found in credentials file {}", profile_name, credentials_file_path);
            } else { // support local work without a profile if user has not set SKIPPR_PROFILE
                println!("Profile '{}' not found in credentials file {}", profile_name, credentials_file_path);
            }
            return;
        }

        // check if profile exists
        if ini.sections().contains(&profile_name) == true
            || profile_name == "default" {

            let workspace = ini.get(&profile_name, "workspace").expect(&format!("'workspace' not found for profile '{}' in credentials file {}", profile_name, credentials_file_path));
            let api_token = ini.get(&profile_name, "api_token").expect(&format!("'api_token' not found for profile '{}' in credentials file {}", profile_name, credentials_file_path));

            // Update app config
            let mut app_config = APP_CONFIG.write();
            let app_config = app_config.as_mut().unwrap();

            // app_config.skippr.workspace = Some(workspace);
            // app_config.skippr.api_token = Some(api_token);
            match app_config.skippr.as_mut() {
                Some(skippr) => {
                    skippr.workspace = Some(workspace);
                    skippr.api_token = Some(api_token);
                }
                None => {
                    app_config.skippr = Some(Skippr {
                        workspace: Some(workspace),
                        api_token: Some(api_token),
                        skippr_s3_bucket: None,
                        tenant: None,
                    });
                }
            }

        } else {
            panic!("Profile '{}' not found in credentials file {}", profile_name, credentials_file_path);
        }

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
                Config::parse_skippr_profile();
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

        Config::parse_skippr_profile();

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

    pub fn get_pipeline_input_plugin_name() -> String {

        if Config::get_envcache("DATA_SOURCE_PLUGIN_NAME") != "" {
            return Config::get_envcache("DATA_SOURCE_PLUGIN_NAME")
        } else {
            let config = Config::get();

            let pipline = match config.pipelines.get(PIPELINE_NAME.read().as_str()) {
                Some(pipeline) => {
                    pipeline
                }
                None => {
                    let plugin_name = Config::getenv("DATA_SOURCE_PLUGIN_NAME", "");

                    Config::set_evncache("DATA_SOURCE_PLUGIN_NAME", &plugin_name.clone());

                    return plugin_name;
                }
            };

            if pipline.input.is_some() {
                // split dot string
                let input_plugin_name = pipline.input.as_ref().unwrap().split('.').collect::<Vec<&str>>()[1].to_string();

                let res = match config.data_inputs.as_ref() {
                    Some(data_inputs) => {
                        match data_inputs.get(&input_plugin_name) {
                            Some(plugin_config) => {
                                plugin_config.plugin_name().clone().or(Some("".to_string())).unwrap()
                            },
                            None => {
                                Config::getenv("DATA_SOURCE_PLUGIN_NAME", "")
                            }
                        }
                    }
                    None => {
                        Config::getenv("DATA_SOURCE_PLUGIN_NAME", "")
                    }
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
            return Config::get_envcache("DATA_OUTPUT_PLUGIN_NAME")
        } else {
            let config = Config::get();

            let pipline = match config.pipelines.get(PIPELINE_NAME.read().as_str()) {
                Some(pipeline) => {
                    pipeline
                }
                None => {
                    let plugin_name = Config::getenv("DATA_OUTPUT_PLUGIN_NAME", "");
                    Config::set_evncache("DATA_OUTPUT_PLUGIN_NAME", &plugin_name.clone());
                    return plugin_name;
                }
            };

            if pipline.output.is_some() {
                // split dot string
                let output_plugin_name = pipline.output.as_ref().unwrap().split('.').collect::<Vec<&str>>()[1].to_string();

                let res = match config.data_outputs.as_ref() {
                    Some(data_outputs) => {
                        match data_outputs.get(&output_plugin_name) {
                            Some(plugin_config) => {
                                plugin_config.plugin_name().clone().or(Some("".to_string())).unwrap()
                            },
                            None => {
                                Config::getenv("DATA_OUTPUT_PLUGIN_NAME", "")
                            }
                        }
                    }
                    None => {
                        Config::getenv("DATA_OUTPUT_PLUGIN_NAME", "")
                    }
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

    pub fn get_pipeline_schema_plugin_name() -> String {
        if Config::get_envcache("DATA_SCHEMA_PLUGIN_NAME") != "" {
            return Config::get_envcache("DATA_SCHEMA_PLUGIN_NAME")
        } else {
            let config = Config::get();

            let pipline = match config.pipelines.get(PIPELINE_NAME.read().as_str()) {
                Some(pipeline) => {
                    pipeline
                }
                None => {
                    let plugin_name = Config::getenv("DATA_SCHEMA_PLUGIN_NAME", "");
                    Config::set_evncache("DATA_SCHEMA_PLUGIN_NAME", &plugin_name.clone());
                    return plugin_name;
                }
            };

            if pipline.schema.is_some() {
                // split dot string
                let input_plugin_name = pipline.schema.as_ref().unwrap().split('.').collect::<Vec<&str>>()[1].to_string();

                let res = match config.schema_outputs.as_ref() {
                    Some(schema_outputs) => {
                        match schema_outputs.get(&input_plugin_name) {
                            Some(plugin_config) => {
                                plugin_config.plugin_name().clone().or(Some("".to_string())).unwrap()
                            },
                            None => {
                                Config::getenv("DATA_SCHEMA_PLUGIN_NAME", "")
                            }
                        }
                    }
                    None => {
                        Config::getenv("DATA_SCHEMA_PLUGIN_NAME", "")
                    }
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

    pub fn get_pipeline_deadletter_plugin_name() -> String {
        if Config::get_envcache("DATA_DEADLETTER_PLUGIN_NAME") != "" {
            return Config::get_envcache("DATA_DEADLETTER_PLUGIN_NAME")
        } else {
            let config = Config::get();

            let pipline = match config.pipelines.get(PIPELINE_NAME.read().as_str()) {
                Some(pipeline) => {
                    pipeline
                }
                None => {
                    let plugin_name = Config::getenv("DATA_DEADLETTER_PLUGIN_NAME", "");
                    Config::set_evncache("DATA_DEADLETTER_PLUGIN_NAME", &plugin_name.clone());
                    return plugin_name;
                }
            };

            if pipline.deadletter.is_some() {
                // split dot string
                let deadletter_plugin_name = pipline.deadletter.as_ref().unwrap().split('.').collect::<Vec<&str>>()[1].to_string();

                let res = match config.data_deadletters.as_ref() {
                    Some(data_deadletters) => {
                        match data_deadletters.get(&deadletter_plugin_name) {
                            Some(plugin_config) => {
                                plugin_config.plugin_name().clone().or(Some("".to_string())).unwrap()
                            },
                            None => {
                                Config::getenv("DATA_DEADLETTER_PLUGIN_NAME", "")
                            }
                        }
                    }
                    None => {
                        Config::getenv("DATA_DEADLETTER_PLUGIN_NAME", "")
                    }
                };

                Config::set_evncache("DATA_DEADLETTER_PLUGIN_NAME", &res.clone());
                res

            } else {
                let res = Config::getenv("DATA_DEADLETTER_PLUGIN_NAME", "");
                Config::set_evncache("DATA_DEADLETTER_PLUGIN_NAME", &res.clone());
                res
            }
        }
    }

    pub fn get_skippr_api_token() -> String {
        if Config::get_envcache("SKIPPR_API_TOKEN") != "" {
            if Config::get_envcache("SKIPPR_API_TOKEN") == DEFAULT_CONFIG {
                return "".to_string();
            }
            return Config::get_envcache("SKIPPR_API_TOKEN")
        } else {
            let config = Config::get();

            let token = Config::getenv("SKIPPR_API_TOKEN", DEFAULT_CONFIG);

            let token = config.skippr.or(Some(
                Skippr {
                    api_token: Some(token.clone()),
                    workspace: None,
                    tenant: None,
                    skippr_s3_bucket: None,
                }
            )).unwrap().api_token.as_ref().or(Some(&token)).unwrap().to_string();

            Config::set_evncache("SKIPPR_API_TOKEN", &token.clone());
            token
        }
    }

    pub fn get_skippr_s3_bucket() -> String {
        if Config::get_envcache("SKIPPR_S3_BUCKET") != "" {
            return Config::get_envcache("SKIPPR_S3_BUCKET")
        } else {
            let config = Config::get();

            let default_bucket = Config::getenv("SKIPPR_S3_BUCKET", "");

            let bucket = match config.skippr {
                Some(skippr) => {
                    match skippr.skippr_s3_bucket.as_ref() {
                        Some(bucket) => bucket.to_string(),
                        None => default_bucket
                    }
                }
                None => {
                    default_bucket
                }
            };

            Config::set_evncache("SKIPPR_S3_BUCKET", &bucket.clone());
            bucket
        }
    }

    // WAL storage selection: "s3" (default) or "disk"
    pub fn get_wal_storage() -> String {
        if Config::get_envcache("WAL_STORAGE") != "" {
            return Config::get_envcache("WAL_STORAGE")
        } else {
            let val = Config::getenv("WAL_STORAGE", "s3");
            Config::set_evncache("WAL_STORAGE", &val);
            val
        }
    }

    pub fn set_wal_storage(value: &str) {
        Config::setenv("WAL_STORAGE", value);
    }

    // WAL S3 bucket (fallback to SKIPPR_S3_BUCKET)
    pub fn get_wal_s3_bucket() -> String {
        if Config::get_envcache("WAL_S3_BUCKET") != "" {
            return Config::get_envcache("WAL_S3_BUCKET")
        } else {
            let fallback = Config::get_skippr_s3_bucket();
            let bucket = Config::getenv("WAL_S3_BUCKET", &fallback);
            Config::set_evncache("WAL_S3_BUCKET", &bucket);
            bucket
        }
    }

    // WAL prefix (default derived: {tenant}/{workspace}/{pipeline}/wal)
    pub fn get_wal_s3_prefix() -> String {
        let default_prefix = format!(
            "{}/{}/{}/wal",
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
            return Config::get_envcache("WAL_BYTES_PER_FILE").parse::<u64>().unwrap_or(4 * 1024 * 1024)
        } else {
            let val = Config::getenv("WAL_BYTES_PER_FILE", &(4 * 1024 * 1024).to_string());
            Config::set_evncache("WAL_BYTES_PER_FILE", &val);
            val.parse::<u64>().unwrap_or(4 * 1024 * 1024)
        }
    }

    pub fn get_wal_max_delay_seconds() -> u64 {
        if Config::get_envcache("WAL_MAX_DELAY_SECONDS") != "" {
            return Config::get_envcache("WAL_MAX_DELAY_SECONDS").parse::<u64>().unwrap_or(60)
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
            Some(pipeline) => {
                pipeline
            }
            None => {
                return Pipeline {
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
                    input: None,
                    output: None,
                    schema: None,
                    deadletter: None,
                }
            }
        };

        pipeline.clone()
    }

    pub fn get_transform_config() -> Transform {

        let pipline = Config::get_pipeline_config();

        match pipline.transform.as_ref() {
            Some(transform) => {
                transform.clone()
            }
            None => {
                Transform {
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
                }
            }
        }
    }

    pub fn get_transform_batch_partition_fields() -> String {
        if Config::get_envcache("TRANSFORM_BATCH_PARTITION_FIELDS") != "" {
            if Config::get_envcache("TRANSFORM_BATCH_PARTITION_FIELDS") == DEFAULT_CONFIG {
                return "".to_string();
            }
            return Config::get_envcache("TRANSFORM_BATCH_PARTITION_FIELDS")
        } else {

            let pipline = Config::get_pipeline_config();

            let default_batch_partition_fields = &Config::getenv("TRANSFORM_BATCH_PARTITION_FIELDS", DEFAULT_CONFIG);
            let batch_partition_fields = match pipline.transform.as_ref() {
                Some(transform) => {
                    transform.batch_partition_fields.as_ref().unwrap_or(default_batch_partition_fields)
                }
                None => {
                    default_batch_partition_fields
                }
            };

            Config::set_evncache("TRANSFORM_BATCH_PARTITION_FIELDS", &batch_partition_fields.clone());
            batch_partition_fields.to_string()
        }
    }
    
    pub fn get_partition_allowed_values() -> String {
        if Config::get_envcache("TRANSFORM_PARTITION_ALLOWED_VALUES") != "" {
            if Config::get_envcache("TRANSFORM_PARTITION_ALLOWED_VALUES") == DEFAULT_CONFIG {
                return "".to_string();
            }
            return Config::get_envcache("TRANSFORM_PARTITION_ALLOWED_VALUES")
        } else {
            let pipline = Config::get_pipeline_config();

            let default_partition_allowed_values = &Config::getenv("TRANSFORM_PARTITION_ALLOWED_VALUES", DEFAULT_CONFIG);
            let partition_allowed_values = match pipline.transform.as_ref() {
                Some(transform) => {
                    transform.partition_allowed_values.as_ref().unwrap_or(default_partition_allowed_values)
                }
                None => {
                    default_partition_allowed_values
                }
            };

            Config::set_evncache("TRANSFORM_PARTITION_ALLOWED_VALUES", &partition_allowed_values.clone());

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
            return Config::get_envcache("TRANSFORM_NAMESPACE_FIELDS")
        } else {
            let pipline = Config::get_pipeline_config();

            let default_namespace_fields = &Config::getenv("TRANSFORM_NAMESPACE_FIELDS", DEFAULT_CONFIG);
            let namespace_fields = match pipline.transform.as_ref() {
                Some(transform) => {
                    transform.namespace_fields.as_ref().unwrap_or(default_namespace_fields)
                }
                None => {
                    default_namespace_fields
                }
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
            return Config::truth_value(&Config::get_envcache("TRANSFORM_FLATTEN_EVENTS"))
        } else {
            let pipline = Config::get_pipeline_config();

            let default_flatten_events = &Config::getenv("TRANSFORM_FLATTEN_EVENTS", DEFAULT_CONFIG);
            let flatten_events = match pipline.transform.as_ref() {
                Some(transform) => {
                    transform.flatten_events.as_ref().unwrap_or(default_flatten_events)
                }
                None => {
                    default_flatten_events
                }
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
            return Config::get_envcache("TRANSFORM_RECORD_FIELD_PATH")
        } else {

            let pipline = Config::get_pipeline_config();

            let default_record_field_path = &Config::getenv("TRANSFORM_RECORD_FIELD_PATH", DEFAULT_CONFIG);

            let record_field_path = match pipline.transform.as_ref() {
                Some(transform) => {
                    transform.record_field_path.as_ref().unwrap_or(default_record_field_path)
                }
                None => {
                    default_record_field_path
                }
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
            return Config::get_envcache("TRANSFORM_BATCH_TIME_FIELDS")
        } else {

            let pipline = Config::get_pipeline_config();

            let default_batch_time_fields = &Config::getenv("TRANSFORM_BATCH_TIME_FIELDS", DEFAULT_CONFIG);

            let batch_time_fields = match pipline.transform.as_ref() {
                Some(transform) => {
                    transform.batch_time_fields.as_ref().unwrap_or(default_batch_time_fields)
                }
                None => {
                    default_batch_time_fields
                }
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
            return Config::get_envcache("TRANSFORM_BATCH_TIME_UNIT")
        } else {

            let pipline = Config::get_pipeline_config();

            let default_batch_time_unit = &Config::getenv("TRANSFORM_BATCH_TIME_UNIT", DEFAULT_CONFIG);

            let batch_time_unit = match pipline.transform.as_ref() {
                Some(transform) => {
                    transform.batch_time_unit.as_ref().unwrap_or(default_batch_time_unit)
                }
                None => {
                    default_batch_time_unit
                }
            };

            Config::set_evncache("TRANSFORM_BATCH_TIME_UNIT", &batch_time_unit.clone());
            batch_time_unit.to_string()
        }
    }

    pub fn get_time_partition_prefix() -> Option<String> {
        if Config::get_envcache("TRANSFORM_TIME_PARTITION_PREFIX") != "" {
            if Config::get_envcache("TRANSFORM_TIME_PARTITION_PREFIX") == DEFAULT_CONFIG {
                return None;
            }
            return Some(Config::get_envcache("TRANSFORM_TIME_PARTITION_PREFIX"))
        } else {

            let pipline = Config::get_pipeline_config();

            let default = &Config::getenv("TRANSFORM_TIME_PARTITION_PREFIX", DEFAULT_CONFIG);

            let batch_time_unit = match pipline.transform.as_ref() {
                Some(transform) => {
                    match transform.time_partition_prefix.as_ref() {
                        Some(time_partition_prefix) => {
                            Config::set_evncache("TRANSFORM_TIME_PARTITION_PREFIX", &time_partition_prefix.clone());
                            Some(time_partition_prefix.to_string())
                        }
                        None => {
                            Config::set_evncache("TRANSFORM_TIME_PARTITION_PREFIX", &default.clone());
                            None
                        }
                       
                    }
                }
                None => {
                    None
                }
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
            return Config::get_envcache("SYNC_FREQUENCY").parse::<u64>().unwrap();
        } else {

            let pipline = Config::get_pipeline_config();

            let default_sync_frequency = &Config::getenv("SYNC_FREQUENCY", &DEFAULT.to_string()).parse::<u64>().unwrap();

            let sync_frequency = pipline.sync_frequency_seconds.as_ref().unwrap_or(default_sync_frequency);
            
            Config::set_evncache("SYNC_FREQUENCY", &sync_frequency.clone().to_string());
            sync_frequency.clone()
        }
    }
    
    pub fn get_pipeline_chaos_mode() -> bool {
        if Config::get_envcache("SKIPPR_CHAOS_MODE") != "" {
            if Config::get_envcache("SKIPPR_CHAOS_MODE") == DEFAULT_CONFIG {
                return false;
            }
            return Config::truth_value(&Config::get_envcache("SKIPPR_CHAOS_MODE"))
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
            return Config::get_envcache("DATA_DIR")
        } else {
            let config = Config::get();

            let default_data_dir = Config::getenv("DATA_DIR", "./data");
            // let default_data_dir = "./data".to_string();

            let pipeline_dir = match config.pipelines.get(PIPELINE_NAME.read().as_str()) {
                Some(pipeline) => {

                    pipeline.data_dir.as_ref().unwrap_or(&default_data_dir).to_string()
                }
                None => {
                    default_data_dir
                }
            };

            Config::set_evncache("DATA_DIR", &pipeline_dir.clone());
            pipeline_dir
        }
    }

    pub fn get_pipeline_env() -> String {
        if Config::get_envcache("SKIPPR_ENV") != "" {
            return Config::get_envcache("SKIPPR_ENV")
        } else {
            
            let config = Config::get();

            let default_env = Config::getenv("SKIPPR_ENV", "prod");
            let pipeline_env = match config.pipelines.get(PIPELINE_NAME.read().as_str()) {
                Some(pipeline) => {
                    pipeline.env.as_ref().unwrap_or(&default_env).to_string()
                }
                None => {
                    default_env
                }
            };

            Config::set_evncache("SKIPPR_ENV", &pipeline_env.clone());
            pipeline_env
        }
    }

    pub fn get_auto_approve() -> bool {
        if Config::get_envcache("SCHEMA_AUTO_APPROVE") != "" {
            return Config::truth_value(&Config::get_envcache("SCHEMA_AUTO_APPROVE"))
        } else {
            let pipeline = Config::get_pipeline_config();

            let default_auto_approve = Config::getenv("SCHEMA_AUTO_APPROVE", "true");
            let auto_approve = pipeline.auto_approve.as_ref().unwrap_or(&default_auto_approve);

            Config::set_evncache("SCHEMA_AUTO_APPROVE", &auto_approve.clone());
            
            Config::truth_value(auto_approve)
        }
    }

     pub fn get_reset_offsets() -> bool {
        if Config::get_envcache("RESET_OFFSETS") != "" {
            return Config::truth_value(&Config::get_envcache("RESET_OFFSETS"))
        } else {
            let pipeline = Config::get_pipeline_config();

            let default_auto_approve = &Config::getenv("RESET_OFFSETS", "false");
            let auto_approve = pipeline.reset_offsets.as_ref().unwrap_or(&default_auto_approve);

            Config::set_evncache("RESET_OFFSETS", &auto_approve.clone());
            
            Config::truth_value(auto_approve)
        }
    }

     pub fn get_reset_metadata() -> bool {
        if Config::get_envcache("RESET_METADATA") != "" {
            return Config::truth_value(&Config::get_envcache("RESET_METADATA"))
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
            return Config::get_envcache("BUFFER_THRESHOLD_BYTES").parse::<u64>().unwrap()
        } else {

            let pipline = Config::get_pipeline_config();

            let default = Config::getenv("BUFFER_THRESHOLD_BYTES", "10485760").parse::<u64>().unwrap();

            let buffer_threshold_bytes = match pipline.buffer_threshold_bytes.as_ref() {
                Some(buffer_threshold_bytes) => {
                    buffer_threshold_bytes
                }
                None => {
                    &default
                }
            };

            Config::set_evncache("BUFFER_THRESHOLD_BYTES", &buffer_threshold_bytes.to_string());
            buffer_threshold_bytes.clone()
        }
    }

    pub fn get_pipeline_buffer_disk_threshold_bytes() -> u64 {
        if Config::get_envcache("BUFFER_DISK_THRESHOLD_BYTES") != "" {
            return Config::get_envcache("BUFFER_DISK_THRESHOLD_BYTES").parse::<u64>().unwrap()
        } else {

            let pipline = Config::get_pipeline_config();

            // Low default to support AWS Lambda by default
            let four_gb = "4294967296";

            let default = Config::getenv("BUFFER_DISK_THRESHOLD_BYTES", four_gb).parse::<u64>().unwrap();

            let buffer_disk_threshold_bytes = match pipline.buffer_disk_threshold_bytes.as_ref() {
                Some(buffer_disk_threshold_bytes) => {
                    buffer_disk_threshold_bytes
                }
                None => {
                    &default
                }
            };

            Config::set_evncache("BUFFER_DISK_THRESHOLD_BYTES", &buffer_disk_threshold_bytes.to_string());
            buffer_disk_threshold_bytes.clone()
        }
    }

    pub fn get_pipeline_buffer_threshold_seconds() -> u64 {
        if Config::get_envcache("BUFFER_THRESHOLD_SECONDS") != "" {
            return Config::get_envcache("BUFFER_THRESHOLD_SECONDS").parse::<u64>().unwrap()
        } else {

            let pipline = Config::get_pipeline_config();

            let default = Config::getenv("BUFFER_THRESHOLD_SECONDS", "60").parse::<u64>().unwrap();

            let buffer_threshold_seconds = match pipline.buffer_threshold_seconds.as_ref() {
                Some(buffer_threshold_seconds) => {
                    buffer_threshold_seconds
                }
                None => {
                    &default
                }
            };

            Config::set_evncache("BUFFER_THRESHOLD_SECONDS", &buffer_threshold_seconds.to_string());
            buffer_threshold_seconds.clone()
        }

    }

    pub fn get_pipline_plugin_config(plugin_type: &str) -> Result<PluginConfig, String> {

        let pipeline_config = Config::get_pipeline_config();

        let config = Config::get();

        match plugin_type {
            "input" => {
                if let Some(data_inputs) = config.data_inputs {

                    let input_name = match pipeline_config.input.as_ref() {
                        Some(input) => {
                            input.split('.').collect::<Vec<&str>>()[1].to_string()
                        }
                        None => {
                            return Err("Input not found".to_string())
                        }
                    };

                    if let Some(config) = data_inputs.get(&input_name) {
                        Ok(config.clone())
                    } else {
                        Err("Input not found".to_string())
                    }
                } else {
                    Err("Input not found".to_string())
                }
            }
            "output" => {
                if let Some(data_outputs) = config.data_outputs {

                    let input_name = match pipeline_config.output.as_ref() {
                        Some(input) => {
                            input.split('.').collect::<Vec<&str>>()[1].to_string()
                        }
                        None => {
                            return Err("Output not found".to_string())
                        }
                    };

                    if let Some(config) = data_outputs.get(&input_name) {
                        Ok(config.clone())
                    } else {
                        Err("Output not found".to_string())
                    }
                } else {
                    Err("Output not found".to_string())
                }
            }
            "deadletter" => {
                if let Some(data_deadletters) = config.data_deadletters {

                    let input_name = match pipeline_config.deadletter.as_ref() {
                        Some(input) => {
                            input.split('.').collect::<Vec<&str>>()[1].to_string()
                        }
                        None => {
                            return Err("Deadletter not found".to_string())
                        }
                    };

                    if let Some(config) = data_deadletters.get(&input_name) {
                        Ok(config.clone())
                    } else {
                        Err("Deadletter not found".to_string())
                    }
                } else {
                    Err("Deadletter not found".to_string())
                }
            }
            "schema" => {
                if let Some(schema_outputs) = config.schema_outputs {

                    let input_name = match pipeline_config.schema.as_ref() {
                        Some(input) => {
                            input.split('.').collect::<Vec<&str>>()[1].to_string()
                        }
                        None => {
                            return Err("Schema not found".to_string())
                        }
                    };

                    if let Some(config) = schema_outputs.get(&input_name) {
                        Ok(config.clone())
                    } else {
                        Err("Schema not found".to_string())
                    }
                } else {
                    Err("Schema not found".to_string())
                }
            }
            _ => Err(format!("Invalid plugin type: {}", plugin_type)),
        }
    }

    // Function to access the config anywhere in the code.
    pub fn get() -> Config {
        match APP_CONFIG.read().as_ref() {
            Some(app_config) => {
                app_config.clone()
            }
            None => {
                Config::new()
            }
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
            },
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
                    println!("Directory: {}", path.display());
                    Config::list_dir_contents(path.clone())
                        .expect(format!("Couldn't list dir {}", path.display()).as_str());
                } else {
                    println!("File: {}", path.display());
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
            return Config::get_envcache("WORKSPACE_NAME")
        } else {
            let config = Config::get();

            let default_token = Config::getenv("WORKSPACE_NAME", "default");

            let workspace_name = match config.skippr.unwrap()
                .workspace.as_ref() {
                Some(workspace) => {
                    workspace.to_string()
                }
                None => {
                    default_token
                }
            };

            Config::set_evncache("WORKSPACE_NAME", &workspace_name.clone());
            workspace_name
        }
    }

    pub fn get_tenant() -> String {
        if Config::get_envcache("TENANT") != "" {
            return Config::get_envcache("TENANT")
        } else {
            let config = Config::get();

            let default_tenant = Config::getenv("TENANT", "default");

            let tenant = match config.skippr {
                Some(skippr) => {
                    match skippr.tenant.as_ref() {
                        Some(tenant) => tenant.to_string(),
                        None => default_tenant
                    }
                }
                None => {
                    default_tenant
                }
            };

            Config::set_evncache("TENANT", &tenant.clone());
            tenant
        }
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

    pub async fn get_metadata() -> Result<PipelineMetadata, bool> {

        if Config::get_transform_batch_time_unit() != ""
            && Config::get_transform_batch_time_fields() == ""
        {
            println!("ERROR: Config: 'TRANSFORM_BATCH_TIME_FIELDS' must be since you've set: 'TRANSFORM_BATCH_TIME_UNIT'.");
        }

        let data_dir = Config::get_data_dir();

        let tenant = Self::get_tenant();
        let workspace = Self::get_workspace_name();
        let pipeline = Self::get_pipeline_name();
        let env = Config::get_pipeline_env();

        let metadata_dir = format!("{}/metadata/skippr_{}/workspace_{}/pipeline_{}", data_dir, env, workspace, pipeline);
        let metadata_path = format!("{}/metadata.json", metadata_dir);

        // Try S3 first, then fallback to local metadata if S3 is empty
        let s3_key = format!("{}/{}/{}/metadata/metadata.json", tenant, workspace, pipeline);
        
        let pipeline_metadata: Result<PipelineMetadata, bool> = match s3::get_json(&s3_key).await {
            Ok(json_value) => {
                match serde_json::from_value::<PipelineMetadata>(json_value) {
                    Ok(pipeline_metadata) => {
                        println!("Loaded metadata from S3");
                        // Cache to local file
                        Config::set_metadata(&pipeline_metadata, false).await;
                        Ok(pipeline_metadata)
                    },
                    Err(_) => {
                        println!("Failed to parse metadata from S3, trying local cache");
                        // Fallback to local metadata
                        match fs::metadata(&metadata_path) {
                            Ok(local_metadata) => {
                                if local_metadata.is_file() {
                                    let file = File::open(&metadata_path).unwrap();
                                    let reader = BufReader::new(file);
                                    match serde_json::from_reader::<_, PipelineMetadata>(reader) {
                                        Ok(metadata) => {
                                            println!("Loaded metadata from local cache, uploading to S3");
                                            // Upload local metadata to S3 for future use
                                            Config::set_metadata(&metadata, false).await;
                                            Ok(metadata)
                                        }
                                        Err(e) => {
                                            println!("Failed to parse local metadata ({}), recreating new metadata", e);
                                            Err(false)
                                        }
                                    }
                                } else {
                                    Err(false)
                                }
                            },
                            Err(_) => {
                                println!("No local metadata found either, creating new metadata");
                                Err(false)
                            }
                        }
                    }
                }
            },
            Err(_) => {
                println!("Metadata not found in S3, checking local cache");
                // Fallback to local metadata
                match fs::metadata(&metadata_path) {
                    Ok(local_metadata) => {
                        if local_metadata.is_file() {
                            let file = File::open(&metadata_path).unwrap();
                            let reader = BufReader::new(file);
                            match serde_json::from_reader::<_, PipelineMetadata>(reader) {
                                Ok(metadata) => {
                                    println!("Loaded metadata from local cache, uploading to S3");
                                    // Upload local metadata to S3 for future use
                                    Config::set_metadata(&metadata, false).await;
                                    Ok(metadata)
                                }
                                Err(e) => {
                                    println!("Failed to parse local metadata ({}), creating new metadata", e);
                                    Err(false)
                                }
                            }
                        } else {
                            Err(false)
                        }
                    },
                    Err(_) => {
                        println!("No metadata found locally or in S3, creating new metadata");
                        Err(false)
                    }
                }
            }
        };

        let pipeline_metadata: Result<PipelineMetadata, bool> = match pipeline_metadata {
            Ok(mut metadata) => {

                // bit of a hack to store the pipeline config that we need to maintain.
                // useful when running SQL DDL commands locally, where the pipeline yml config is not present.
                // For example, SCHEMA DUMP needs to know whether to output the flattened or nested schema.
                match &Config::get_transform_config().flatten_events {
                    Some(val) => {
                        metadata.flattened = Config::truth_value(val);
                        Ok(metadata)
                    },
                    None => {
                        metadata.flattened = false;
                        Ok(metadata)
                    },
                }
            },
            Err(_) => {
                Ok(PipelineMetadata::new())
            }
        };

        pipeline_metadata
    }

    pub async fn delete_metadata() {
        let tenant = Self::get_tenant();
        let workspace = Self::get_workspace_name();
        let pipeline = Self::get_pipeline_name();

        let s3_key = format!("{}/{}/{}/metadata/metadata.json", tenant, workspace, pipeline);

        match s3::delete_object(&s3_key).await {
            Ok(_) => {
                println!("Deleted pipeline metadata from S3: {}", s3_key);
            }
            Err(err) => {
                println!("Failed to delete metadata from S3: {:?}", err);
            }
        }
    }

    pub async fn set_metadata(pipeline_metadata: &PipelineMetadata, evolved: bool) {

        let tenant = Self::get_tenant();
        let workspace = Self::get_workspace_name();
        let pipeline = Self::get_pipeline_name();

        let data_dir = Config::get_data_dir();
        let env = Config::get_pipeline_env();

        let metadata_dir = format!("{}/metadata/skippr_{}/workspace_{}/pipeline_{}", data_dir, env, workspace, pipeline);
        let metadata_path = format!("{}/metadata.json", metadata_dir);
        
        // ensure directory exists
        match fs::create_dir_all(&metadata_dir) {
            Ok(_g) => {}
            Err(err) => panic!(
                "Error creating metadata dir {}, does the host path exist? {:?}",
                metadata_dir, err
            ),
        }

        // Clobber local file
        let file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(true)
            .open(&metadata_path)
            .unwrap();
        
        let mut writer = BufWriter::new(file);
        writer.write_all(serde_json::to_string(&pipeline_metadata).unwrap().as_bytes()).unwrap();
        
        // Upload to S3
        let s3_key = format!("{}/{}/{}/metadata/metadata.json", tenant, workspace, pipeline);
        let json_value = serde_json::to_value(pipeline_metadata).unwrap();
        
        match s3::put_json(&s3_key, &json_value).await {
            Ok(_) => {
                println!("Updated pipeline metadata in S3: {}", s3_key);
            }
            Err(err) => {
                println!("Failed to upload metadata to S3: {:?}", err);
                // Don't exit here, just log the error
            }
        }

        if evolved {

            {
                METADATA.write().clone_from(&pipeline_metadata);
            }

            Config::sync_schema(&pipeline_metadata.metadata).await;
        }
    }

    // Schema update worker
    fn ensure_schema_worker() -> UnboundedSender<String> {
        use once_cell::sync::Lazy as OnceLazy;
        static SENDER: OnceLazy<std::sync::Mutex<Option<UnboundedSender<String>>>> = OnceLazy::new(|| std::sync::Mutex::new(None));
        {
            let mut guard = SENDER.lock().unwrap();
            if let Some(tx) = guard.as_ref() { return tx.clone(); }
            let (tx, mut rx): (UnboundedSender<String>, UnboundedReceiver<String>) = unbounded_channel();
            *guard = Some(tx.clone());

            // Coalesce pending namespaces
            let pending: DashMap<String, ()> = DashMap::new();
            tokio::spawn(async move {
                while let Some(ns) = rx.recv().await {
                    if pending.insert(ns.clone(), ()).is_some() { continue; }
                    // small debounce window
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    // process and clear
                    let flatten = Config::get_transform_flatten_events();
                    let md_snapshot = { METADATA.read().metadata.clone() };
                    if let Some(schema) = md_snapshot.get(&ns) {
                        // template update
                        let default_message = create_default_nested_message(&schema.fields);
                        {
                            let mut lock = DEFAULT_NESTED_MESSAGE.write();
                            lock.insert(ns.clone(), default_message);
                        }
                        // arrow schema publish
                        let _ = Ingest::prepare_arrow_schema_with_metadata(&ns, &md_snapshot, flatten);
                        // optional Athena
                        if Config::get_pipeline_output_plugin_name() == "Athena" {
                            let out_meta = if flatten { OutputMetadata::from_flatterened_metadata(schema) } else { OutputMetadata::from_metadata(schema) };
                            let _ = AwsAthena::create_or_update_schema(&ns, &out_meta).await;
                        }
                    }
                    pending.remove(&ns);
                }
            });
            return tx;
        }
    }

    pub async fn sync_schema(metadata: &HashMap<String, Metadata>) {

        // Enqueue namespaces for background processing and return immediately
        let tx = Config::ensure_schema_worker();
        for (namespace, _schema) in metadata.into_iter() {
            let _ = tx.send(namespace.clone());
        }
    }

    pub async fn init() {


        
        let data_dir = Config::get_data_dir();
        let ingest_dir = &format!("{}/ingest_buffer", data_dir);
        let deadletter_dir = &format!("{}/deadletter_buffer", data_dir);
        let output_dir = &format!("{}/output_buffer", data_dir);
        match fs::create_dir(deadletter_dir) {
            Ok(_g) => {}
            Err(_err) => {}
        }
        match fs::create_dir(format!("{}/done", deadletter_dir)) {
            Ok(_g) => {}
            Err(_err) => {}
        }
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
}
