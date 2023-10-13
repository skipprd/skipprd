use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt::Debug;
use std::fs;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read};
use std::ops::Deref;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use yaml_rust::YamlLoader;

use nix::libc::exit;

use std::time::{Duration, Instant};
use lazy_static::lazy_static;
use once_cell::sync::Lazy;

// use aws_config::profile::profile_file::ProfileFileKind::Config;
use serde_derive::{Deserialize, Serialize};

use serde_json::{json, Value};

use reqwest::header::HeaderValue;


use reqwest::header::{HeaderMap, HeaderName};
use reqwest::{Client, StatusCode};

use crate::discover::Metadata;
use crate::{flatten_metadata, METRICS, RUNNING};
use crate::helpers::Helpers;

use crate::helpers::license::{HAS_LICENSE, LicenseChecker, TENANT_ID};

use crate::plugins::athena::{AwsAthena, DataOutputAwsAthenaPluginConfig};

use toml;
use crate::helpers::timed_rwlock::TimedRwLock;
use crate::plugins::file_input::{DataSourceLocalFilePlugin, DataSourceLocalFilePluginConfig};
use crate::plugins::s3_input::DataSourceS3PluginConfig;


#[derive(Debug, Deserialize, Clone)]
pub struct Skippr {
    pub api_token: Option<String>,
    pub workspace: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Transform {
    pub batch_time_fields: Option<String>,
    pub batch_time_unit: Option<String>,
    pub flatten_events: Option<String>,
    pub record_field_path: Option<String>,
    pub batch_partition_fields: Option<String>,
    pub namespace_fields: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub enum PluginConfig {
    s3(DataSourceS3PluginConfig),
    athena(DataOutputAwsAthenaPluginConfig),
    file(DataSourceLocalFilePluginConfig),
}

impl PluginConfig {
    pub fn format(&self) -> String {
        match self {
            PluginConfig::s3(s3_config) => s3_config.format.clone().or(Some("json".to_string())).as_ref().unwrap().clone(),
            PluginConfig::athena(athena_config) => athena_config.format.clone().or(Some("json".to_string())).as_ref().unwrap().clone(),
            PluginConfig::file(file_config) => file_config.format.clone().or(Some("json".to_string())).unwrap(),
        }
    }

    pub fn plugin_name(&self) -> Option<String> {
        match self {
            PluginConfig::s3(s3_config) => Some("s3".to_string()),
            PluginConfig::athena(athena_config) => Some("athena".to_string()),
            PluginConfig::file(file_config) => Some("file".to_string()),
        }
    }

    pub fn batch_size_bytes(&self) -> Option<i64> {
        match self {
            PluginConfig::s3(s3_config) => s3_config.batch_size_bytes.clone(),
            PluginConfig::athena(athena_config) => athena_config.batch_size_bytes.clone(),
            PluginConfig::file(file_config) => file_config.batch_size_bytes.clone(),
        }
    }

    pub fn batch_size_seconds(&self) -> Option<i64> {
        match self {
            PluginConfig::s3(s3_config) => s3_config.batch_size_seconds.clone(),
            PluginConfig::athena(athena_config) => athena_config.batch_size_seconds.clone(),
            PluginConfig::file(file_config) => file_config.batch_size_seconds.clone(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct Pipeline {
    pub auto_approve: Option<String>,
    pub env: Option<String>,
    pub buffer_threshold_bytes: Option<i64>,
    pub buffer_threshold_seconds: Option<i64>,
    pub chaos_mode: Option<String>,
    pub data_dir: Option<String>,
    pub transform: Option<Transform>,
    pub input: Option<String>,
    pub output: Option<String>,
    pub schema: Option<String>,
    pub deadletter: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub skippr: Skippr,
    pub pipelines: HashMap<String, Pipeline>,
    pub data_inputs: Option<HashMap<String, PluginConfig>>,
    pub data_outputs: Option<HashMap<String, PluginConfig>>,
    pub data_deadletters: Option<HashMap<String, PluginConfig>>,
    pub schema_outputs: Option<HashMap<String, PluginConfig>>,
}

pub static APP_CONFIG: Lazy<Arc<TimedRwLock<Option<Config>>>> = Lazy::new(|| Arc::new(TimedRwLock::new("config".to_string(),None)));
pub static PIPELINE_NAME: Lazy<Arc<TimedRwLock<String>>> = Lazy::new(|| Arc::new(TimedRwLock::new("config".to_string(),"default".to_string())));

impl Config {

    pub fn find_config_file() -> String {

        let config_file = Config::getenv("SKIPPR_CONFIG_FILE", "");

        if config_file != "" {
            if Path::new(&config_file).exists() {
                return config_file.to_string()
            }
        }

        let mut valid_locations = vec![
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

    pub fn build_config() {

        let file_path = Config::find_config_file();

        let mut file = File::open(&file_path)
            .expect("File not found");

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
        let config = Config::merge_config_with_env(config);

        let string_val = serde_json::to_string(&config).unwrap();
        // Deserialize the String back into Config
        let mut config: Config = serde_json::from_str(&string_val).unwrap();

        // println!("config: {:?}", config);

        {
            let mut app_config = APP_CONFIG.write().unwrap();
            *app_config = Some(config);
        }

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
        let config = Config::get();

        let pipline = config.pipelines.get(PIPELINE_NAME.read().unwrap().as_str()).unwrap();

        if pipline.input.is_some() {
            // split dot string
            let input_plugin_name = pipline.input.as_ref().unwrap().split('.').collect::<Vec<&str>>()[1].to_string();

            match config.data_inputs.as_ref() {
                Some(data_inputs) => {
                    match data_inputs.get(&input_plugin_name) {
                        Some(plugin_config) => {
                            plugin_config.plugin_name().clone().or(Some("".to_string())).unwrap()
                        },
                        None => {
                            "".to_string()
                        }
                    }
                }
                None => {
                    "".to_string()
                }
            }
        } else {
            "".to_string()
        }
    }

    pub fn get_pipeline_output_plugin_name() -> String {
        let config = Config::get();

        let pipline = config.pipelines.get(PIPELINE_NAME.read().unwrap().as_str()).unwrap();

        if pipline.output.is_some() {
            // split dot string
            let input_plugin_name = pipline.output.as_ref().unwrap().split('.').collect::<Vec<&str>>()[1].to_string();

            match config.data_outputs.as_ref() {
                Some(data_outputs) => {
                    match data_outputs.get(&input_plugin_name) {
                        Some(plugin_config) => {
                            plugin_config.plugin_name().clone().or(Some("".to_string())).unwrap()
                        },
                        None => {
                            "".to_string()
                        }
                    }
                }
                None => {
                    "".to_string()
                }
            }
        } else {
            "".to_string()
        }
    }

    pub fn get_pipeline_schema_plugin_name() -> String {
        let config = Config::get();

        let pipline = config.pipelines.get(PIPELINE_NAME.read().unwrap().as_str()).unwrap();

        if pipline.schema.is_some() {
            // split dot string
            let input_plugin_name = pipline.schema.as_ref().unwrap().split('.').collect::<Vec<&str>>()[1].to_string();

            match config.schema_outputs.as_ref() {
                Some(schema_outputs) => {
                    match schema_outputs.get(&input_plugin_name) {
                        Some(plugin_config) => {
                            plugin_config.plugin_name().clone().or(Some("".to_string())).unwrap()
                        },
                        None => {
                            "".to_string()
                        }
                    }
                }
                None => {
                    "".to_string()
                }
            }
        } else {
            "".to_string()
        }

    }

    pub fn get_pipeline_deadletter_plugin_name() -> String {
        let config = Config::get();

        let pipline = config.pipelines.get(PIPELINE_NAME.read().unwrap().as_str()).unwrap();

        if pipline.deadletter.is_some() {
            // split dot string
            let input_plugin_name = pipline.deadletter.as_ref().unwrap().split('.').collect::<Vec<&str>>()[1].to_string();

            match config.data_deadletters.as_ref() {
                Some(data_deadletters) => {
                    match data_deadletters.get(&input_plugin_name) {
                        Some(plugin_config) => {
                            plugin_config.plugin_name().clone().or(Some("".to_string())).unwrap()
                        },
                        None => {
                            "".to_string()
                        }
                    }
                }
                None => {
                    "".to_string()
                }
            }
        } else {
            "".to_string()
        }

    }

    pub fn get_skippr_api_token() -> String {
        let config = Config::get();

        let token = Config::getenv("SKIPPR_API_TOKEN", "");
        if token != "" {
            return token
        }

        config.skippr.api_token.as_ref().or(Some(&"".to_string())).unwrap().to_string()
    }

    pub fn get_pipelines() -> Vec<String> {
        let config = Config::get();

        let mut pipelines = vec![];

        for (key, _value) in config.pipelines.iter() {
            pipelines.push(key.to_string());
        }

        pipelines
    }

    pub fn get_pipeline_config() -> Pipeline {
        let config = Config::get();

        let pipline = config.pipelines.get(PIPELINE_NAME.read().unwrap().as_str()).unwrap();

        pipline.clone()
    }

    pub fn get_transform_config() -> Transform {
        let config = Config::get();

        let pipline = config.pipelines.get(PIPELINE_NAME.read().unwrap().as_str()).unwrap();

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
                    namespace_fields: None,
                }
            }
        }
    }

    pub fn get_transform_batch_partition_fields() -> String {
        let config = Config::get();

        let pipline = config.pipelines.get(PIPELINE_NAME.read().unwrap().as_str()).unwrap();

        let default_batch_partition_fields = &"".to_string();
        let batch_partition_fields = match pipline.transform.as_ref() {
            Some(transform) => {
                transform.batch_partition_fields.as_ref().unwrap_or(default_batch_partition_fields)
            }
            None => {
                default_batch_partition_fields
            }
        };

        batch_partition_fields.to_string()
    }

    pub fn get_transform_namespace_fields() -> String {
        let config = Config::get();

        let pipline = config.pipelines.get(PIPELINE_NAME.read().unwrap().as_str()).unwrap();

        let default_namespace_fields = &"".to_string();
        let namespace_fields = match pipline.transform.as_ref() {
            Some(transform) => {
                transform.namespace_fields.as_ref().unwrap_or(default_namespace_fields)
            }
            None => {
                default_namespace_fields
            }
        };

        namespace_fields.to_string()
    }

    pub fn get_transform_flatten_events() -> bool {
        let config = Config::get();

        let pipline = config.pipelines.get(PIPELINE_NAME.read().unwrap().as_str()).unwrap();

        let default_flatten_events = &"no".to_string();

        let flatten_events = match pipline.transform.as_ref() {
            Some(transform) => {
                transform.flatten_events.as_ref().unwrap_or(default_flatten_events)
            }
            None => {
                default_flatten_events
            }
        };

        Config::truth_value(flatten_events)
    }

    pub fn get_transform_record_field_path() -> String {
        let config = Config::get();

        let pipline = config.pipelines.get(PIPELINE_NAME.read().unwrap().as_str()).unwrap();

        let default_record_field_path = &"".to_string();

        let record_field_path = match pipline.transform.as_ref() {
            Some(transform) => {
                transform.record_field_path.as_ref().unwrap_or(default_record_field_path)
            }
            None => {
                default_record_field_path
            }
        };

        record_field_path.to_string()
    }

    pub fn get_transform_batch_time_fields() -> String {
        let config = Config::get();

        let pipline = config.pipelines.get(PIPELINE_NAME.read().unwrap().as_str()).unwrap();

        let default_batch_time_fields = &"".to_string();

        let batch_time_fields = match pipline.transform.as_ref() {
            Some(transform) => {
                transform.batch_time_fields.as_ref().unwrap_or(default_batch_time_fields)
            }
            None => {
                default_batch_time_fields
            }
        };

        batch_time_fields.to_string()
    }

    pub fn get_transform_batch_time_unit() -> String {
        let config = Config::get();

        let pipline = config.pipelines.get(PIPELINE_NAME.read().unwrap().as_str()).unwrap();

        let default_batch_time_unit = &"".to_string();

        let batch_time_unit = match pipline.transform.as_ref() {
            Some(transform) => {
                transform.batch_time_unit.as_ref().unwrap_or(default_batch_time_unit)
            }
            None => {
                default_batch_time_unit
            }
        };

        batch_time_unit.to_string()
    }

    pub fn get_pipeline_chaos_mode() -> bool {

        let mode = Config::getenv("SKIPPR_CHAOS_MODE", "");
        if mode != "" {
            return Config::truth_value(mode.as_str())
        }

        let config = Config::get();

        let pipline = config.pipelines.get(PIPELINE_NAME.read().unwrap().as_str()).unwrap();

        let default_chaos_mode = &"no".to_string();
        let chaos_mode = pipline.chaos_mode.as_ref().unwrap_or(default_chaos_mode);

        Config::truth_value(chaos_mode)
    }

    pub fn get_pipeline_data_dir() -> String {
        let config = Config::get();

        let pipline = config.pipelines.get(PIPELINE_NAME.read().unwrap().as_str()).unwrap();

        let default_data_dir = &"./data".to_string();
        let data_dir = pipline.data_dir.as_ref().unwrap_or(default_data_dir);

        data_dir.to_string()
    }

    pub fn get_pipeline_env() -> String {
        let config = Config::get();

        let pipeline_name = PIPELINE_NAME.read().unwrap().clone();

        let pipline = config.pipelines.get(pipeline_name.as_str()).unwrap();

        pipline.env.as_ref().unwrap_or(&"prod".to_string()).to_string()
    }

    pub fn get_auto_approve() -> bool {
        let config = Config::get();

        let pipline = config.pipelines.get(PIPELINE_NAME.read().unwrap().as_str()).unwrap();

        pipline.auto_approve == Some("yes".to_string())
    }

    pub fn get_pipeline_buffer_threshold_bytes() -> i64 {
        let config = Config::get();

        let pipline = config.pipelines.get(PIPELINE_NAME.read().unwrap().as_str()).unwrap();

        // @todo: default to 10485760
        pipline.buffer_threshold_bytes.or(Some(10485760)).unwrap()

    }

    pub fn get_pipeline_buffer_threshold_seconds() -> i64 {
        let config = Config::get();

        let pipline = config.pipelines.get(PIPELINE_NAME.read().unwrap().as_str()).unwrap();

        // @todo default to 60
        pipline.buffer_threshold_seconds.or(Some(60)).unwrap()
    }

    // pub fn get_pipline_plugin_config(plugin_type: &str) -> Result<PluginConfig, String> {
    //     let config = Config::get();
    //
    //     let pipline = config.pipelines.get(PIPELINE_NAME.read().unwrap().as_str()).unwrap();
    //
    //     match plugin_type {
    //         "input" => config.data_inputs.as_ref().unwrap().get(&pipline.input.as_ref().unwrap().to_string()).cloned().ok_or("Input not found".to_string()),
    //         "output" => config.data_outputs.as_ref().unwrap().get(&pipline.output.as_ref().unwrap().to_string()).cloned().ok_or("Output not found".to_string()),
    //         "deadletter" => config.data_deadletters.as_ref().unwrap().get(&pipline.deadletter.as_ref().unwrap().to_string()).cloned().ok_or("Deadletter not found".to_string()),
    //         "schema" => config.schema_outputs.as_ref().unwrap().get(&pipline.schema.as_ref().unwrap().to_string()).cloned().ok_or("Schema not found".to_string()),
    //         _ => Err("Invalid plugin type".to_string()),
    //     }
    // }

    pub fn get_pipline_plugin_config(plugin_type: &str) -> Result<PluginConfig, String> {

        let pipeline_name = PIPELINE_NAME.read().unwrap().as_str();

        let pipeline_config = Config::get_pipeline_config();

        let config = Config::get();

        match plugin_type {
            "input" => {
                if let Some(data_inputs) = config.data_inputs {

                    let plugin_name = match pipeline_config.input.as_ref() {
                        Some(input) => {
                            input.split('.').collect::<Vec<&str>>()[1].to_string()
                        }
                        None => {
                            "".to_string()
                        }
                    };

                    if let Some(config) = data_inputs.get(&plugin_name) {
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

                    let plugin_name = match pipeline_config.output.as_ref() {
                        Some(output) => {
                            output.split('.').collect::<Vec<&str>>()[1].to_string()
                        }
                        None => {
                            "".to_string()
                        }
                    };

                    if let Some(config) = data_outputs.get(&plugin_name) {
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

                    let plugin_name = match pipeline_config.deadletter.as_ref() {
                        Some(deadletter) => {
                            deadletter.split('.').collect::<Vec<&str>>()[1].to_string()
                        }
                        None => {
                            "".to_string()
                        }
                    };

                    if let Some(config) = data_deadletters.get(&plugin_name) {
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

                    let plugin_name = match pipeline_config.input.as_ref(){
                        Some(input) => {
                            input.split('.').collect::<Vec<&str>>()[1].to_string()
                        }
                        None => {
                            "".to_string()
                        }
                    };

                    if let Some(config) = schema_outputs.get(&plugin_name) {
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
        APP_CONFIG.read().unwrap().as_ref().unwrap().clone()
    }

    pub fn setenv(name: &str, value: &str) {
        std::env::set_var(name, value);
    }

    pub fn getenv(name: &str, default: &str) -> String {
        match std::env::var(name.to_uppercase()) {
            Ok(val) => match val {
                v if v.is_empty() => default.to_string(),
                _ => val,
            },
            Err(_e) => default.to_string(),
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
        PIPELINE_NAME.read().unwrap().clone()
    }

    pub fn get_workspace_name() -> String {
        let config = Config::get();

        let token = Config::getenv("SKIPPR_WORKSPACE", "");
        if token != "" {
            return token
        }

        config.skippr.workspace.as_ref().unwrap().to_string()
    }

    pub fn get_full_namespace_name() -> String {
        // let mut helpers = Helpers { clean_field_cache: Default::default() };

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

    pub async fn get_config() -> Result<HashMap<String, Metadata>, bool> {

        if !*HAS_LICENSE.read().unwrap() {
            // println!("ERROR: No license found, please set the 'LICENSE' environment variable.");
            return Err(false);
        }


        if Config::get_transform_batch_time_unit() != ""
            && Config::get_transform_batch_time_fields() == ""
        {
            println!("ERROR: Config: 'TRANSFORM_BATCH_TIME_FIELDS' must be since you've set: 'TRANSFORM_BATCH_TIME_UNIT'.");
        }

        let data_dir = Config::get_data_dir();


        let workspace = Self::get_workspace_name();
        let pipeline = Self::get_pipeline_name();

        let env = Config::get_pipeline_env();
        let uri = if env != "prod" {
            format!("https://metadata.{}.api.skippr.io", env)
        } else {
            String::from("https://metadata.api.skippr.io")
        };
        let token = Config::get_skippr_api_token();

        let mut headers = HeaderMap::new();
        let auth_header = HeaderName::from_static("x-api-key");
        headers.insert(auth_header, HeaderValue::from_str(&token).unwrap());

        let client = Client::builder().default_headers(headers).build().unwrap();

        let path = format!(
            "workspace/{}/pipeline/{}/status/{}",
            workspace, pipeline, "approved"
        );

        let response = client
            .get(&format!("{}/{}", uri, path))
            .timeout(Duration::from_secs(15))
            .send()
            .await;

        let metadata: Result<HashMap<String, Metadata>, bool> = match response {
            Ok(resp) => match resp.status() {
                StatusCode::OK => {
                    let metadata = resp.json::<HashMap<String, Metadata>>().await.unwrap();
                    Ok(metadata)
                }
                StatusCode::NOT_FOUND => {
                    // println!("Metadata HTTP Error: {:?}", err);
                    Err(false)
                }
                err => unsafe {
                    println!(
                        "Metadata HTTP Error: {} - {:?}",
                        err,
                        resp.error_for_status()
                    );
                    // RUNNING.write().unwrap().store(false, Ordering::SeqCst);
                    // Err(false)
                    exit(1);
                },
            },
            Err(_err) => {
                // println!("Metadata HTTP Error: {:?}", err);
                Err(false)
            }
        };

        metadata
    }

    pub async fn set_config(metadata: &HashMap<String, Metadata>, evolved: bool) {
        if evolved {

            // let data_dir = Config::get_data_dir();
            // let metadata_file = format!("{}/metadata-{}.json", data_dir, Helpers::random_str(10));
            //
            // let file = OpenOptions::new()
            //     .create(true)
            //     .write(true)
            //     .truncate(true)
            //     .open(metadata_file)
            //     .unwrap();
            //
            // let writer = BufWriter::new(file);
            //
            // serde_json::to_writer(writer, &metadata).unwrap();
            //
            // println!("saved metadata file");

            ///////////

            if !*HAS_LICENSE.read().unwrap() {
                // println!("ERROR: No license found, please set the 'LICENSE' environment variable.");
                return;
            }

            let workspace = Self::get_workspace_name();
            let pipeline = Self::get_pipeline_name();

            // let uri = Config::getenv("SKIPPR_API_ENDPOINT", "");
            let env = Config::get_pipeline_env();
            let uri = if env != "prod" {
                format!("https://metadata.{}.api.skippr.io", env)
            } else {
                String::from("https://metadata.api.skippr.io")
            };
            let token = Config::get_skippr_api_token();

            let mut headers = HeaderMap::new();
            let auth_header = HeaderName::from_static("x-api-key");

            headers.insert(auth_header, HeaderValue::from_str(&token).unwrap());

            let client = Client::builder().default_headers(headers).build().unwrap();

            let path = "";

            let auto_approve_evolution = Config::get_auto_approve();

            let schema_status = if !evolved {
                "approved"
            } else if !auto_approve_evolution && evolved {
                "pending"
            } else {
                "approved"
            };

            let data = json!({
                "workspace": workspace,
                "pipeline": pipeline,
                "metadata": metadata,
                "status": schema_status
            });

            // println!("Posting data: {:?}", data);

            let response = client
                .put(&format!("{}/{}", uri, path))
                .json(&data)
                .send()
                .await;

            match response {
                Ok(resp) => {
                    match resp.status() {
                        StatusCode::OK => {
                            // println!("Metadata HTTP resp: {:?}", resp);
                            println!("Updated pipeline metadata in Skippr SaaS");
                        }
                        err => println!("Metadata HTTP Error: {:?}", err),
                    };
                }
                Err(err) => {
                    println!("Metadata HTTP Error: {:?}", err);
                } // Ok(resp) => {
                  //     println!("Metadata HTTP Success: {:?}", resp);
                  // }
                  // Err(err) => {
                  //     println!("Metadata HTTP Error: {:?}", err);
                  // }
            }

            Config::sync_schema(metadata).await;
        }
    }

    pub async fn sync_schema(metadata: &HashMap<String, Metadata>) {
        if Config::get_pipeline_output_plugin_name() != ""
            && Config::get_pipeline_output_plugin_name() == "athena"
        {
            if *HAS_LICENSE.read().unwrap() {
                let flatten = Config::get_transform_flatten_events();

                for (namespace, schema) in metadata.into_iter() {
                    println!("Updating Hive '{}' schema", namespace);

                    if flatten {
                        let mut out_meta: HashMap<String, Metadata> = HashMap::new();
                        flatten_metadata(metadata.get(namespace).unwrap(), &mut out_meta);

                        let mut output_metadata: HashMap<String, Metadata> = HashMap::new();
                        let mut flat: Metadata = Metadata::new().unwrap();
                        flat.fields = Box::new(out_meta);
                        output_metadata.insert(namespace.clone(), flat);

                        AwsAthena::create_or_update_schema(
                            &namespace,
                            &output_metadata.get(namespace).unwrap(),
                        )
                            .await;
                    } else {
                        AwsAthena::create_or_update_schema(&namespace, &schema).await;
                    }
                }
            } else {
                println!("No license found for AWS Glue schema plugin. Visit https://skippr.io to get a license.");
            }
        }
    }

    pub async fn init() {

        let license = LicenseChecker::new();
        license.unwrap().get_license().await.unwrap();

        Config::get_data_dir();


    }
}



#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_getenv() {
        assert_eq!(Config::getenv("TEST", "default"), "default");
    }

    #[test]

    fn test_getenv_empty_string() {
        Config::setenv("TEST", "");
        assert_eq!(Config::getenv("TEST", "default"), "default");
    }

    #[test]
    fn test_get_pipeline_name() {
        assert_eq!(Config::get_pipeline_name(), "default");
    }
}
