use std::collections::btree_map::BTreeMap;
use crate::helpers::configuration::Config;
use serde_json::json;

use std::hash::{Hash};

use crate::helpers::s3;
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

        let _tenant_id = Config::get_tenant_id();

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

        // Upload logs to S3
        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
        let s3_key = format!("skippr/{}/{}/logs/{}_{}.json", workspace, pipeline, timestamp, _run_id);

        match s3::put_json(&s3_key, &data).await {
            Ok(_) => {
                println!("Uploaded logs to S3: {}", s3_key);
            }
            Err(err) => {
                println!("Failed to upload logs to S3: {:?}", err);
            }
        }
        
        Ok(())
    }
}
