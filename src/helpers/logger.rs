use std::collections::btree_map::BTreeMap;
use crate::helpers::configuration::Config;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::json;

use std::hash::{Hash};

use crate::helpers::license::{HAS_LICENSE, TENANT_ID};
use serde_derive::Serialize;
use std::fmt;
use std::fmt::Debug;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::{RwLock};
use crate::METRICS;

#[derive(Debug, Clone, Serialize, Hash, PartialEq, Eq)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(Debug, Clone, Serialize, Hash, PartialEq, Eq)]
pub struct Log {
    time: SystemTime,
    level: LogLevel,
    message: String,
}

pub struct Logger {
    pub(crate) logs: BTreeMap<SystemTime, Log>,
    buffer_limit: usize,
}

impl Logger {
    pub fn new(buffer_limit: usize) -> Arc<RwLock<Self>> {
        Arc::new(RwLock::new(Self {
            logs: BTreeMap::new(),
            buffer_limit,
        }))
    }

    pub async fn log(&mut self, level: LogLevel, message: String) {
        let log = Log {
            time: SystemTime::now(),
            level,
            message
        };

        self.logs.insert(log.clone().time, log.clone());

        if self.logs.len() >= self.buffer_limit {
            self.flush().await.unwrap();
        }
    }

    pub async fn flush(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if !self.logs.is_empty() {
            match self.log_api(self.logs.clone(), None).await {
                Ok(_) => {
                    println!("Successfully sent logs to API");

                    self.logs.clear();
                    Ok(())
                }
                Err(err) => {
                    println!("Error sending logs to API: {:?}", err);
                    Err(err)
                }
            }
        } else {
            Ok(())
        }
    }

    pub(crate) async fn log_api<'a>(
        &mut self,
        logs: BTreeMap<SystemTime, Log>,
        exit_code: Option<i8>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let workspace = Config::get_workspace_name();
        let pipeline = Config::get_pipeline_name();

        let env = Config::get_pipeline_env();
        let uri = if env != "prod" {
            format!("https://metrics.{}.api.skippr.io", env)
        } else {
            String::from("https://metrics.api.skippr.io")
        };

        let mut default_api_key = "";

        {
            if !*HAS_LICENSE.read() {
                default_api_key = "XxIVftJXN4LF6ARrRqJvKAsv30vhIZHR"
            }
        }

        let mut token = Config::get_skippr_api_token();
        if token.is_empty() {
           token = default_api_key.to_string();
        }

        let mut headers = HeaderMap::new();
        let auth_header = HeaderName::from_static("x-api-key");
        headers.insert(auth_header, HeaderValue::from_str(&token).unwrap());

        let client = reqwest::Client::builder()
            .default_headers(headers)
            // .timeout(Duration::from_secs(10))
            .build()?;

        let path = "";

        let mut _tenant_id = "".to_string();
        {
            _tenant_id = TENANT_ID.read().clone();
        }

        let mut _run_id = "".to_string();
        {
            _run_id = METRICS.read().run_id.clone();
        }
        
        
        let data = json!({
            "logs": logs.iter().map(|(_time, log)| {
                json!({
                    "level": log.level.to_string(),
                    "message": log.message,
                    "time": log.time
                })
            }).collect::<Vec<_>>(),
            "type": "log",
            "tenant_id": _tenant_id,
            "workspace_name": workspace,
            "pipeline_name": pipeline,
            "run_id": _run_id,
            "datetime": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "exit_code": exit_code
        });

        // println!("Posting data: {:?}", data);

        let response = client
            .put(format!("{}/{}", uri, path))
            .json(&data)
            .send()
            .await;

        match response {
            Ok(resp) => {
                if resp.status().is_success() {
                    // println!("Logs API Response: {:?}", resp);
                } else {
                    println!("Logs API Error: {:?}", resp);
                }
            }
            Err(err) => {
                println!("Logs API Error: {:?}", err);
            }
        }
        
        Ok(())
    }
}
