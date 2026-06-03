use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use serde_derive::Serialize;
use skippr_plugin_data_source_site_quality::config::{DeviceProfile, Viewport};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::sleep;
use tracing::info;

use crate::config::DataSourceSiteSecurityPluginConfig;

pub const FIXTURE_ENV: &str = "SKIPPR_SITE_SECURITY_FIXTURE_DIR";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkerJobRequest {
    pub job_id: String,
    pub url: String,
    pub device_profile: String,
    pub viewport: Viewport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    pub wait_until: String,
    pub navigation_timeout_ms: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SecurityHeaders {
    pub has_csp: bool,
    pub has_hsts: bool,
    pub has_x_frame_options: bool,
    pub has_x_content_type_options: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StorageEntryWire {
    pub storage_kind: String,
    pub entry_name: String,
    pub value_length: u32,
    #[serde(default)]
    pub pii_hints: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScriptEntryWire {
    pub script_url: String,
    pub script_host: String,
    pub is_third_party: bool,
    #[serde(rename = "async")]
    pub async_attr: bool,
    pub defer: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkerJobResult {
    pub job_id: String,
    pub ok: bool,
    #[serde(default)]
    pub final_url: Option<String>,
    #[serde(default)]
    pub status: Option<u32>,
    #[serde(default)]
    pub headers: Option<SecurityHeaders>,
    #[serde(default)]
    pub cookies: Vec<StorageEntryWire>,
    #[serde(default)]
    pub local_storage: Vec<StorageEntryWire>,
    #[serde(default)]
    pub session_storage: Vec<StorageEntryWire>,
    #[serde(default)]
    pub scripts: Vec<ScriptEntryWire>,
    #[serde(default)]
    pub cookie_count: Option<u32>,
    #[serde(default)]
    pub local_storage_key_count: Option<u32>,
    #[serde(default)]
    pub session_storage_key_count: Option<u32>,
    #[serde(default)]
    pub third_party_script_count: Option<u32>,
    #[serde(default)]
    pub error: Option<serde_json::Value>,
}

pub fn build_job_request(
    config: &DataSourceSiteSecurityPluginConfig,
    url: &str,
    device: &DeviceProfile,
) -> WorkerJobRequest {
    WorkerJobRequest {
        job_id: uuid::Uuid::new_v4().to_string(),
        url: url.to_string(),
        device_profile: device.profile.clone(),
        viewport: device.viewport.clone(),
        user_agent: None,
        wait_until: config.wait_until.clone(),
        navigation_timeout_ms: config.navigation_timeout_ms,
    }
}

pub struct WorkerClient {
    node_path: String,
    script_path: PathBuf,
    throttle_ms: u64,
}

impl WorkerClient {
    pub fn new(config: &DataSourceSiteSecurityPluginConfig) -> Result<Self, std::io::Error> {
        let script_path =
            skippr_runtime_sdk::site_security_worker::resolve_site_security_worker_script(Some(
                Path::new(env!("CARGO_MANIFEST_DIR")),
            ))?;
        let ppm = config.pages_per_minute.max(1);
        Ok(Self {
            node_path: config.worker_node_path.clone(),
            script_path,
            throttle_ms: 60_000 / ppm as u64,
        })
    }

    pub async fn run_job(&self, job: &WorkerJobRequest) -> Result<WorkerJobResult, std::io::Error> {
        if let Ok(dir) = std::env::var(FIXTURE_ENV) {
            let trimmed = dir.trim();
            if !trimmed.is_empty() {
                return load_fixture(trimmed, &job.job_id);
            }
        }

        let mut child = Command::new(&self.node_path)
            .arg(&self.script_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .env(
                "PLAYWRIGHT_EXECUTABLE_PATH",
                std::env::var("PLAYWRIGHT_EXECUTABLE_PATH").unwrap_or_default(),
            )
            .spawn()?;

        let mut stdin = child.stdin.take().expect("stdin");
        let line = serde_json::to_string(job).map_err(std::io::Error::other)?;
        stdin.write_all(line.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        drop(stdin);

        let stdout = child.stdout.take().expect("stdout");
        let mut reader = BufReader::new(stdout).lines();
        let response_line = reader
            .next_line()
            .await?
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "no worker output"))?;
        let _ = child.wait().await;

        serde_json::from_str(&response_line).map_err(std::io::Error::other)
    }

    pub async fn throttle_delay(&self) {
        if self.throttle_ms > 0 {
            sleep(Duration::from_millis(self.throttle_ms)).await;
        }
    }
}

fn load_fixture(dir: &str, job_id: &str) -> Result<WorkerJobResult, std::io::Error> {
    let path = Path::new(dir).join("scan_ok.json");
    let raw = std::fs::read_to_string(&path)?;
    let mut result: WorkerJobResult = serde_json::from_str(&raw).map_err(std::io::Error::other)?;
    result.job_id = job_id.to_string();
    result.ok = true;
    info!(fixture = %path.display(), "Site Security: using fixture scan");
    Ok(result)
}
