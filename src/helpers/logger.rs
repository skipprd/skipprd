use crate::helpers::configuration::Config;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::json;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use crate::helpers::license::TENANT_ID;
use serde_derive::Serialize;
use std::fmt;
use std::sync::Arc;
use tokio::sync::Mutex;

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
    level: LogLevel,
    message: String,
}

pub struct Logger {
    logs: HashMap<Log, usize>,
    buffer_limit: usize,
}

impl Logger {
    pub fn new(buffer_limit: usize) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            logs: HashMap::new(),
            buffer_limit,
        }))
    }

    pub async fn log(&mut self, level: LogLevel, message: String) {
        let log = Log { level, message };

        let count = self.logs.entry(log).or_insert(0);
        *count += 1;

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
        logs: HashMap<Log, usize>,
        exit_code: Option<i8>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let workspace = Config::get_workspace_name();
        let pipeline = Config::get_pipeline_name();

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

        let tenant_id = TENANT_ID.read().unwrap().clone();

        let data = json!({
            "logs": logs.iter().map(|(log, count)| {
                json!({
                    "level": log.level.to_string(),
                    "message": log.message,
                    "count": count
                })
            }).collect::<Vec<_>>(),
            "type": "log",
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
                println!("Notified Metrics API");
            }
            Err(err) => {
                println!("Metrics HTTP Error: {:?}", err);
            }
        }

        Ok(())
    }
}
