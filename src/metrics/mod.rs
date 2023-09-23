use std::fmt::Debug;
use std::ops::Sub;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime};
use chrono::DateTime;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_derive::{Deserialize, Serialize};
use serde_json::json;
use tar::Header;
use crate::helpers::configuration::Config;
use crate::helpers::Helpers;
use crate::helpers::license::{HAS_LICENSE, TENANT_ID};
use crate::METRICS;

pub static LAST_MESSAGES_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static LAST_FIXED_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static LAST_DEADLETTERS_TOTAL: AtomicU64 = AtomicU64::new(0);


const VERSION: Option<&str> = option_env!("CARGO_PKG_VERSION");

#[derive(Debug, Deserialize, Serialize)]
struct MetricsEnvConfig {
    data_source_plugin_name: String,
    data_output_plugin_name: String,
    schema_output_plugin_name: String,
    data_deadletter_plugin_name: String,
    data_source_batch_size_bytes: u64,
    data_source_batch_size_seconds: u64,
    buffer_threshold_bytes: u64,
    buffer_threshold_seconds: u64,
    transform_namespace_fields: String,
    transform_batch_partition_fields: String,
    transform_flatten_events: String,
    transform_batch_time_fields: String,
    transform_batch_time_units: String,
    data_dir: String,
    chaos_mode: String,
    input_format: String,
    output_format: String,

    schema_output_glue_database_name: String,
}

impl MetricsEnvConfig {
    // default()
    fn new() -> Self {
        Self {
            data_source_plugin_name: Config::getenv("DATA_SOURCE_PLUGIN_NAME", ""),
            data_output_plugin_name: Config::getenv("DATA_OUTPUT_PLUGIN_NAME", ""),
            schema_output_plugin_name: Config::getenv("SCHEMA_OUTPUT_PLUGIN_NAME", ""),
            data_deadletter_plugin_name: Config::getenv("DATA_DEADLETTER_PLUGIN_NAME", ""),
            data_source_batch_size_bytes: Config::getenv("DATA_SOURCE_BATCH_SIZE_BYTES", "0").parse::<u64>().unwrap(),
            data_source_batch_size_seconds: Config::getenv("DATA_SOURCE_BATCH_SIZE_SECONDS", "0").parse::<u64>().unwrap(),
            buffer_threshold_bytes: Config::getenv("BUFFER_THRESHOLD_BYTES", "0").parse::<u64>().unwrap(),
            buffer_threshold_seconds: Config::getenv("BUFFER_THRESHOLD_SECONDS", "0").parse::<u64>().unwrap(),
            transform_namespace_fields: Config::getenv("TRANSFORM_NAMESPACE_FIELDS", ""),
            transform_batch_partition_fields: Config::getenv("TRANSFORM_BATCH_PARTITION_FIELDS", ""),
            transform_flatten_events: Config::getenv("TRANSFORM_FLATTEN_EVENTS", "false"),
            transform_batch_time_fields: Config::getenv("TRANSFORM_BATCH_TIME_FIELDS", ""),
            transform_batch_time_units: Config::getenv("TRANSFORM_BATCH_TIME_UNITS", ""),
            data_dir: Config::getenv("DATA_DIR", ""),
            chaos_mode: Config::getenv("CHAOS_MODE", "false"),
            input_format: Config::getenv("INPUT_FORMAT", ""),
            output_format: Config::getenv("OUTPUT_FORMAT", ""),

            schema_output_glue_database_name: Config::getenv("SCHEMA_OUTPUT_GLUE_DATABASE_NAME", ""),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub enum MetricsStatus {
    Running,
    Stopped,
    Finishing,
    Completed,
    Error,
    Unknown
}

impl MetricsStatus {

    pub fn name(&self) -> &'static str {
        match self {
            MetricsStatus::Running => "Running",
            MetricsStatus::Stopped => "Stopped",
            MetricsStatus::Finishing => "Finishing",
            MetricsStatus::Completed => "Completed",
            MetricsStatus::Error => "Error",
            MetricsStatus::Unknown => "Unknown",
        }
    }
}

#[derive(Debug)]
pub struct Metrics {
    pub messages_total: u64,
    pub deadletters_total: u64,
    pub ingeted_slow_total: u64,
    pub start_time: DateTime<chrono::Utc>,
    pub bytes_current: u64,
    pub bytes_total: u64,
    pub status: MetricsStatus,
    pub run_id: String,
}

impl Metrics {


    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self {
            // msgs_total: 0,
            messages_total: 0,
            deadletters_total: 0,
            ingeted_slow_total: 0,
            start_time: DateTime::<chrono::Utc>::from(SystemTime::now()),
            bytes_current: 0,
            bytes_total: 0,
            status: MetricsStatus::Unknown,
            run_id: Helpers::random_password(32),
        }
    }

    pub(crate) async fn send_metrics<'a>(
        exit_code: Option<i8>,
    ) -> Result<(), Box<dyn std::error::Error>> {

        let metrics = METRICS.read().unwrap();

        let workspace = Config::get_workspace_name();
        let pipeline = Config::get_pipeline_name();

        let env = Config::getenv("APP_ENV", "prod");
        let uri = if env != "prod" {
            format!("https://metrics.{}.api.skippr.io", env)
        } else {
            String::from("https://metrics.api.skippr.io")
        };

        let mut default_api_key = "";

        if !*HAS_LICENSE.read().unwrap() {
            default_api_key = "XxIVftJXN4LF6ARrRqJvKAsv30vhIZHR"
        }

        let token = Config::getenv("SKIPPR_API_TOKEN", default_api_key);

        let mut headers = HeaderMap::new();
        let auth_header = HeaderName::from_static("x-api-key");
        headers.insert(auth_header, HeaderValue::from_str(&token).unwrap());

        let client = reqwest::Client::builder()
            .default_headers(headers)
            // .timeout(Duration::from_secs(10))
            .build()?;

        let path = "";

        let tenant_id = TENANT_ID.read().unwrap().clone();

        let last_messages_total = LAST_MESSAGES_TOTAL.load(Ordering::Relaxed);
        let ingested_current = metrics.messages_total - last_messages_total;
        LAST_MESSAGES_TOTAL.store(metrics.messages_total, Ordering::Relaxed);

        let last_fixed_total = LAST_FIXED_TOTAL.load(Ordering::Relaxed);
        let fixed_current = metrics.ingeted_slow_total - last_fixed_total;
        LAST_FIXED_TOTAL.store(metrics.ingeted_slow_total, Ordering::Relaxed);

        let last_deadletters_total = LAST_DEADLETTERS_TOTAL.load(Ordering::Relaxed);
        let deadletters_current = metrics.deadletters_total - last_deadletters_total;
        LAST_DEADLETTERS_TOTAL.store(metrics.deadletters_total, Ordering::Relaxed);

        let start_time_utc_str = metrics.start_time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        // get runtime in seocds from metrics.start_time
        let current_time = chrono::Utc::now();
        let run_time_seconds = (current_time - metrics.start_time).num_seconds();


        let data = json!({
            "metrics": {
                "ingeted_total": metrics.messages_total,
                "fixed_total": metrics.ingeted_slow_total,
                "deadletters_total": metrics.deadletters_total,
                "ingeted_current": ingested_current,
                "fixed_current": fixed_current,
                "deadletters_current": deadletters_current,
                "run_time_seconds": run_time_seconds,
                "bytes_current": metrics.bytes_current,
                "bytes_total": metrics.bytes_total,
            },
            "type": "metric",
            "run_id": metrics.run_id,
            "tenant_id": tenant_id,
            "workspace_name": workspace,
            "pipeline_name": pipeline,
            "status": metrics.status.name(),
            "start_time": start_time_utc_str,
            "datetime": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "version": VERSION.unwrap_or("unknown"),
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
                // println!("Notified Metrics API");
            }
            Err(err) => {
                println!("Metrics HTTP Error: {:?}", err);
            }
        }


        Ok(())
    }

    pub(crate) async fn send_config() -> Result<(), Box<dyn std::error::Error>> {

        let metrics = METRICS.read().unwrap();

        let workspace = Config::get_workspace_name();
        let pipeline = Config::get_pipeline_name();

        let env = Config::getenv("APP_ENV", "prod");
        let uri = if env != "prod" {
            format!("https://metrics.{}.api.skippr.io", env)
        } else {
            String::from("https://metrics.api.skippr.io")
        };

        let mut default_api_key = "";

        if !*HAS_LICENSE.read().unwrap() {
            default_api_key = "XxIVftJXN4LF6ARrRqJvKAsv30vhIZHR"
        }

        let token = Config::getenv("SKIPPR_API_TOKEN", default_api_key);

        let mut headers = HeaderMap::new();
        let auth_header = HeaderName::from_static("x-api-key");
        headers.insert(auth_header, HeaderValue::from_str(&token).unwrap());

        let client = reqwest::Client::builder()
            .default_headers(headers)
            // .timeout(Duration::from_secs(10))
            .build()?;

        let path = "";

        let tenant_id = TENANT_ID.read().unwrap().clone();

        let current_time = chrono::Utc::now();
        let run_time_seconds = (current_time - metrics.start_time).num_seconds();

        let start_time_utc_str = DateTime::<chrono::Utc>::from(SystemTime::now())
            .sub(chrono::Duration::seconds(run_time_seconds as i64))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        let metrics_env_config = MetricsEnvConfig::new();

        let data = json!({
            "config": metrics_env_config,
            "type": "config",
            "run_id": metrics.run_id,
            "tenant_id": tenant_id,
            "workspace_name": workspace,
            "pipeline_name": pipeline,
            "status": metrics.status.name(),
            "start_time": start_time_utc_str,
            "datetime": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "version": VERSION.unwrap_or("unknown"),
        });

        // println!("Posting data: {:?}", data);

        let response = client
            .put(format!("{}/{}", uri, path))
            .json(&data)
            .send()
            .await?;

        match response.error_for_status() {
            Ok(_resp) => {
                // println!("Status HTTP Success: {:?}", _resp);
                // println!("Notified Metrics API");
            }
            Err(err) => {
                println!("Metrics HTTP Error: {:?}", err);
            }
        }


        Ok(())
    }
}