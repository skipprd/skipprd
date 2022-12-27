
use std::collections::HashMap;
use std::fs::create_dir;
use serde_derive::{Deserialize};

use crate::helpers::Helpers;

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
    pub event_time_bucket_duration_seconds: i32,
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
    pub config_updated_time: i32,
}

impl Config {
    pub fn new() -> Config {
        Config {
            anonymous_metrics: true,
            log_level: String::from("INFO"),
            data_dir: String::from("/data"),
            container_mem: 0,
            pipeline_name: String::from(""),
            pipeline_id: String::from(""),
            tenant_id: String::from(""),
            task_id: 0,
            task_logs: Vec::new(),
            exit_code: 0,
            sync_mode: String::from("sync"),
            mutable_mode_strict: String::from("strict"),
            mutable_mode_resolve: String::from("resolve"),
            mutable_mode_evolve: String::from("evolve"),
            mutable_mode: MutableModes::MUTABLE_MODE_STRICT.to_string(),
            run_mode: RunModes::RUN_MODE_SYNC.to_string(),
            offsets: Vec::new(),
            source_format: None,
            output_format: None,
            analysing: true,
            min_discovery_records: 10000,
            max_discovery_seconds: 300,
            id_fields: Vec::new(),
            date_field_candidates: vec![],
            discovered_field_occurrence: vec![],
            schema: vec![],
            filters: false,
            flush_mem_buffer_bytes: 200000000,
            flush_buffer_bytes: 200000000,
            flush_mem_buffer_seconds: 300,
            flush_buffer_seconds: 300,
            flush_mem_buffer_records: 5000000,
            flush_buffer_records: 5000000,
            event_time_bucket_duration_seconds: 0,
            poll_interval_seconds: 0,
            avro_schemas: Vec::new(),
            output_schemas: Vec::new(),
            partition_by_fields: false,
            event_type_fields: Vec::new(),
            event_path: false,
            flatten_events: false,
            time_fields: false,
            system_user_api_token: String::new(),
            enable_dead_letters: true,
            config_updated_time: 0,
        }
    }

    pub fn getenv(name: &str, default: &str) -> String {
        match std::env::var(name) {
            Ok(val) => val,
            Err(_e) => default.to_string(),
        }
    }

    pub fn get_pipeline_name() -> String {
        let mut helpers = Helpers { clean_field_cache: Default::default() };

        let input_plugin_name = helpers.clean_field_name(Config::getenv("DATA_SOURCE_PLUGIN_NAME", ""));
        let output_plugin_name = helpers.clean_field_name(Config::getenv("DATA_OUTPUT_PLUGIN_NAME", ""));
        let default_pipeline_name = format!("{}to{}", input_plugin_name, output_plugin_name);
        let pipeline_name = Config::getenv("PIPELINE_NAME", default_pipeline_name.as_str());

        pipeline_name
    }

    pub fn get_config() -> Config {
        let mut config = Config::new();

        config = envy::from_env::<Config>()
            .expect("Please provide env vars");

        println!("{:#?}", config);

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
        // config.event_time_bucket_duration_seconds = Config::getenv("DATA_OUTPUT_TIME_BUCKET", false);
        //
        // config.poll_interval_seconds = Config::getenv("DATA_SOURCE_POLL_INTERVAL_SECONDS", config.poll_interval_seconds);
        //
        // config.mutable_mode = Config::getenv("DATA_SOURCE_MUTABLE_MODE", config.mutable_mode);

        if config.mutable_mode == config.mutable_mode {
            // SkipprLogger::info("Strict mutable mode enabled, will sync an exact copy of records.");
        }

        // config.run_mode = Config::getenv("RUN_MODE", config.run_mode);

        // config.flatten_events = Config::getenv("DATA_SOURCE_FLATTEN_EVENTS", config.flatten_events);
        //
        // config.task_id = Config::getenv("TASK_ID", "") as i64;

        let avro_arr: HashMap<String, String> = HashMap::new();

        config.discovered_field_occurrence = Vec::new();

        // config.anonymous_metrics = Config::getenv("ANONYMOUS_METRICS", "true");

        config.pipeline_name = Config::get_pipeline_name();

        config.tenant_id = Config::getenv("TENANT_ID", Helpers::random_str(16).as_str());

        // let data_dir = Config::getenv("DATA_DIR", "");
        // if data_dir != "" {
        //     config.data_dir = data_dir;
        // }


        let uri = Config::getenv("SKIPPR_API_ENDPOINT", "");

        config
    }

    pub fn init() {
        let config: Config = Config::get_config();
        create_dir(config.data_dir);
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
    fn test_get_pipeline_name() {
        assert_eq!(Config::get_pipeline_name(), "test");
    }
}
