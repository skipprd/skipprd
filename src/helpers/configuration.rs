use std::collections::HashMap;
use std::fmt::Debug;
use std::fs;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read};
use std::path::Path;
use std::sync::atomic::Ordering;
use yaml_rust::YamlLoader;

use nix::libc::exit;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

// use aws_config::profile::profile_file::ProfileFileKind::Config;
use serde_derive::{Deserialize, Serialize};

use serde_json::json;

use reqwest::header::HeaderValue;

use once_cell::sync::Lazy;
use reqwest::header::{HeaderMap, HeaderName};
use reqwest::{Client, StatusCode};

use crate::discover::Metadata;
use crate::{flatten_metadata, METRICS, RUNNING};

use crate::helpers::license::{LicenseChecker, TENANT_ID};
use crate::helpers::Helpers;
use crate::plugins::athena::AwsAthena;

#[non_exhaustive]
struct RunModes;

impl RunModes {
    pub const RUN_MODE_SYNC: &'static str = "sync";
    pub const RUN_MODE_VALIDATE_CONNECTION: &'static str = "validate_connection";
    pub const RUN_MODE_VALIDATE_CONFIG: &'static str = "validate_config";
    pub const RUN_MODE_SAVE: &'static str = "save";
    pub const RUN_MODE_RESET_SOURCE_OFFSETS: &'static str = "reset_source_offsets";
    pub const RUN_MODE_DELETE_PLUGIN: &'static str = "delete_plugin";
    // output plugins only
    pub const RUN_MODE_CREATE_UPDATE_DEST_SCHEMA: &'static str = "sync_schema";
    pub const RUN_MODE_DELETE_DEST_SCHEMA: &'static str = "delete_schema";

    pub const RUN_MODE_VALIDATE_SCHEMA: &'static str = "validate_schema";
}

#[non_exhaustive]
struct MutableModes;

impl MutableModes {
    pub const MUTABLE_MODE_STRICT: &'static str = "strict";
    pub const MUTABLE_MODE_RESOLVE: &'static str = "resolve";
    pub const MUTABLE_MODE_EVOLVE: &'static str = "evolve";
}

#[derive(Deserialize, Debug)]
pub struct Config {
    pub run_mode: String,
    pub anonymous_metrics: bool,
    pub log_level: String,
    pub data_dir: String,
    pub container_mem: u64,
    pub pipeline_name: String,
    pub pipeline_id: String,
    pub tenant_id: String,
    pub task_id: u64,
    pub task_logs: Vec<String>,
    pub exit_code: i32,
    pub sync_mode: String,
    pub mutable_mode_strict: String,
    pub mutable_mode_resolve: String,
    pub mutable_mode_evolve: String,
    pub mutable_mode: String,
    pub offsets: Vec<u8>,
    pub source_format: Option<String>,
    pub output_format: Option<String>,
    pub analysing: bool,
    pub min_discovery_records: u32,
    pub max_discovery_seconds: u32,
    pub id_fields: Vec<String>,
    pub date_field_candidates: Vec<String>,
    pub discovered_field_occurrence: Vec<String>,
    pub schema: Vec<String>,
    pub filters: bool,
    pub flush_mem_buffer_bytes: i32,
    pub flush_buffer_bytes: i32,
    pub flush_mem_buffer_seconds: i32,
    pub flush_buffer_seconds: i32,
    pub flush_mem_buffer_records: i32,
    pub flush_buffer_records: i32,
    pub event_time_bucket_duration: i32,
    pub poll_interval_seconds: i32,
    pub avro_schemas: Vec<String>,
    pub output_schemas: Vec<String>,
    pub partition_by_fields: bool,
    pub event_type_fields: Vec<String>,
    pub event_path: bool,
    pub flatten_events: bool,
    pub time_fields: bool,
    pub system_user_api_token: String,
    pub enable_dead_letters: bool,
}

impl Config {
    pub fn new() -> Config {
        Config {
            anonymous_metrics: true,
            log_level: String::from("INFO"),
            data_dir: String::from(""),
            container_mem: 0,
            pipeline_name: String::from(""),
            pipeline_id: String::from(""),
            tenant_id: String::from(""),
            task_id: 0,
            system_user_api_token: String::new(),
            enable_dead_letters: true,
            poll_interval_seconds: 0,

            source_format: None,
            filters: false,
            partition_by_fields: false,
            event_path: false,

            output_format: None,
            flatten_events: false,

            analysing: true,
            sync_mode: String::from("sync"),
            mutable_mode_strict: String::from("strict"),
            mutable_mode_resolve: String::from("resolve"),
            mutable_mode_evolve: String::from("evolve"),
            mutable_mode: MutableModes::MUTABLE_MODE_STRICT.to_string(),
            run_mode: RunModes::RUN_MODE_SYNC.to_string(),

            offsets: Vec::new(),
            task_logs: Vec::new(),
            exit_code: 0,
            id_fields: Vec::new(),
            event_type_fields: Vec::new(),
            time_fields: false,
            date_field_candidates: vec![],
            discovered_field_occurrence: vec![],
            schema: vec![],
            avro_schemas: Vec::new(),
            output_schemas: Vec::new(),

            min_discovery_records: 10000,
            max_discovery_seconds: 300,
            flush_mem_buffer_bytes: 200000000,
            flush_buffer_bytes: 200000000,
            flush_mem_buffer_seconds: 300,
            flush_buffer_seconds: 300,
            flush_mem_buffer_records: 5000000,
            flush_buffer_records: 5000000,
            event_time_bucket_duration: 0,
        }
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
        let mut data_dir = Config::getenv("DATA_DIR", "./data");
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
        Config::getenv("PIPELINE_NAME", "default").to_lowercase()
    }

    pub fn get_workspace_name() -> String {
        Config::getenv("WORKSPACE_NAME", "default").to_lowercase()
    }

    pub fn get_full_namespace_name() -> String {
        // let mut helpers = Helpers { clean_field_cache: Default::default() };

        // let input_plugin_name = Helpers::clean_field_name(Config::getenv("DATA_SOURCE_PLUGIN_NAME", "unknown"));
        // let output_plugin_name = Helpers::clean_field_name(Config::getenv("DATA_OUTPUT_PLUGIN_NAME", "unknown"));
        // let default_pipeline_name = format!("{} to {}", input_plugin_name, output_plugin_name);

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
        let mut config = Config::new();

        config = match envy::from_env::<Config>() {
            Err(_) => config,
            Ok(config) => config,
        };
        // .expect("Please provide env vars");

        // println!("{:#?}", config);

        // let file_config = Self::load_file();

        // config.
        // println!("{:#?}", config);
        // let config_file = File::open("config/connections.yml").unwrap();
        //
        // let yaml_str: String = serde_yaml::from_reader(config_file).unwrap();
        //
        // let configuration: Value = serde_yaml::from_str(&yaml_str).unwrap();
        // println!("{:#?}", configuration);

        // config.pipeline_id = Config::getenv("PIPELINE_ID", "");
        //
        // config.log_level = Config::getenv("LOG_LEVEL", "INFO");
        //
        // config.container_mem = Config::getenv("MEM", "1024");
        // config.container_mem = config.container_mem * 0.8; // allow some overhead
        // ini_set("memory_limit", config.container_mem "M");

        // config.flush_buffer_bytes = Config::getenv("DATA_OUTPUT_FLUSH_BYTES", config.flush_buffer_bytes);
        // config.flush_buffer_seconds = Config::getenv("DATA_OUTPUT_FLUSH_SECONDS", config.flush_buffer_seconds);
        // config.flush_buffer_records = Config::getenv("DATA_OUTPUT_FLUSH_RECORDS", config.flush_buffer_records);
        //
        // config.event_time_bucket_duration = Config::getenv("TRANSFORM_BATCH_TIME_UNIT", false);
        //
        // config.poll_interval_seconds = Config::getenv("DATA_SOURCE_POLL_INTERVAL_SECONDS", config.poll_interval_seconds);
        //
        // config.mutable_mode = Config::getenv("DATA_SOURCE_MUTABLE_MODE", config.mutable_mode);

        if config.mutable_mode == config.mutable_mode {
            // SkipprLogger::info("Strict mutable mode enabled, will sync an exact copy of records.");
        }

        if !Config::getenv("TRANSFORM_BATCH_TIME_UNIT", "").is_empty()
            && Config::getenv("TRANSFORM_BATCH_TIME_FIELDS", "").is_empty()
        {
            println!("ERROR: Environment variable: 'TRANSFORM_BATCH_TIME_FIELDS' must be since you've set: 'TRANSFORM_BATCH_TIME_UNIT'.");
        }

        // config.run_mode = Config::getenv("RUN_MODE", config.run_mode);

        // config.flatten_events = Config::getenv("TRANSFORM_FLATTEN_EVENTS", config.flatten_events);
        //
        // config.task_id = Config::getenv("TASK_ID", "") as i64;

        let _avro_arr: HashMap<String, String> = HashMap::new();

        config.discovered_field_occurrence = Vec::new();

        // config.anonymous_metrics = Config::getenv("ANONYMOUS_METRICS", "true");

        config.pipeline_name = Config::get_pipeline_name();

        // config.tenant_id = Config::getenv("TENANT_ID", Helpers::random_str(16).as_str());

        let data_dir = Config::get_data_dir();
        if !data_dir.is_empty() {
            config.data_dir = data_dir;
        }

        let workspace = Self::get_workspace_name();
        let pipeline = Self::get_pipeline_name();

        let env = Config::getenv("APP_ENV", "prod");
        let uri = if env != "prod" {
            format!("https://metadata.{}.api.skippr.io", env)
        } else {
            String::from("https://metadata.api.skippr.io")
        };
        let token = Config::getenv("SKIPPR_API_TOKEN", "");

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
                    RUNNING.lock().unwrap().store(false, Ordering::SeqCst);
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

            let workspace = Self::get_workspace_name();
            let pipeline = Self::get_pipeline_name();

            // let uri = Config::getenv("SKIPPR_API_ENDPOINT", "");
            let env = Config::getenv("APP_ENV", "prod");
            let uri = if env != "prod" {
                format!("https://metadata.{}.api.skippr.io", env)
            } else {
                String::from("https://metadata.api.skippr.io")
            };
            let token = Config::getenv("SKIPPR_API_TOKEN", "");

            let mut headers = HeaderMap::new();
            let auth_header = HeaderName::from_static("x-api-key");

            headers.insert(auth_header, HeaderValue::from_str(&token).unwrap());

            let client = Client::builder().default_headers(headers).build().unwrap();

            let path = "";

            let data = json!({
                "workspace": workspace,
                "pipeline": pipeline,
                "metadata": metadata,
                "status": "approved",
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
        if !Config::getenv("DATA_OUTPUT_PLUGIN_NAME", "").is_empty() {
            let flatten = Config::truth_value(&Config::getenv("TRANSFORM_FLATTEN_EVENTS", "no"));

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
        }
    }

    pub(crate) async fn set_status<'a>(
        exit_code: Option<i8>,
    ) -> Result<(), Box<dyn std::error::Error>> {

        let metrics = METRICS.lock().unwrap();

        let workspace = Self::get_workspace_name();
        let pipeline = Self::get_pipeline_name();

        let env = Config::getenv("APP_ENV", "prod");
        let uri = if env != "prod" {
            format!("https://metrics.{}.api.skippr.io", env)
        } else {
            String::from("https://metrics.api.skippr.io")
        };
        let token = Config::getenv("SKIPPR_API_TOKEN", "");

        let mut headers = HeaderMap::new();
        let auth_header = HeaderName::from_static("x-api-key");
        headers.insert(auth_header, HeaderValue::from_str(&token).unwrap());

        let client = reqwest::Client::builder()
            .default_headers(headers)
            // .timeout(Duration::from_secs(10))
            .build()?;

        let path = "";

        let tenant_id = TENANT_ID.lock().unwrap().clone();

        let data = json!({
            "metrics": {
                "ingeted_total": metrics.messages_total,
                "deadletters_total": metrics.deadletters_total,
                "ingeted_current": metrics.ingeted_current,
                "run_time_seconds": metrics.run_time_seconds,
                "bytes_current": metrics.bytes_current,
                "bytes_total": metrics.bytes_total,
            },
            "type": "metric",
            "tenant_id": tenant_id,
            "workspace_name": workspace,
            "pipeline_name": pipeline,
            "datetime": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "exit_code": exit_code
        });

        // println!("Posting data: {:?}", data);

        let response = client
            .put(format!("{}/{}", uri, path))
            .json(&data)
            .send()
            .await?;

        match response.error_for_status() {
            Ok(_resp) => {
                // println!("Status HTTP Success: {:?}", resp);
            }
            Err(err) => {
                println!("Metrics HTTP Error: {:?}", err);
            }
        }

        println!("Notified Metrics API");
        Ok(())
    }

    pub async fn init() {
        let license = LicenseChecker::new();
        license.unwrap().get_license().await.unwrap();

        // let config: Config = Config::get_config().await;
        Config::get_data_dir();
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Metrics {
    // pub msgs_total: u64,
    pub messages_total: u64,
    pub deadletters_total: u64,
    pub ingeted_current: u64,
    pub run_time_seconds: u64,
    pub bytes_current: u64,
    pub bytes_total: u64,
}
impl Metrics {
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self {
            // msgs_total: 0,
            messages_total: 0,
            deadletters_total: 0,
            ingeted_current: 0,
            run_time_seconds: 0,
            bytes_current: 0,
            bytes_total: 0,
        }
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
