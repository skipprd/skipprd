use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::Deserialize;
use serde_derive::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tracing::info;

use crate::config::DataSourceContentQualityPluginConfig;
use crate::fetch::FetchResponse;

pub const FIXTURE_ENV: &str = "SKIPPR_CONTENT_QUALITY_RENDER_FIXTURE_DIR";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkerJobRequest {
    pub job_id: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    pub wait_until: String,
    pub navigation_timeout_ms: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkerJobResult {
    pub job_id: String,
    pub ok: bool,
    #[serde(default)]
    pub final_url: Option<String>,
    #[serde(default)]
    pub status: Option<u16>,
    #[serde(default)]
    pub html: Option<String>,
    #[serde(default)]
    pub error: Option<serde_json::Value>,
}

pub fn build_job_request(
    config: &DataSourceContentQualityPluginConfig,
    url: &str,
) -> WorkerJobRequest {
    WorkerJobRequest {
        job_id: job_id(),
        url: url.to_string(),
        user_agent: Some(config.user_agent.clone()),
        wait_until: config.render_wait_until.clone(),
        navigation_timeout_ms: config.render_timeout_ms,
    }
}

fn job_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("content-quality-{nanos}")
}

pub fn parse_result_line(line: &str) -> Result<WorkerJobResult, std::io::Error> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "empty content quality worker result line",
        ));
    }
    serde_json::from_str(trimmed).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid content quality worker result JSON: {e}"),
        )
    })
}

pub struct WorkerClient {
    config: DataSourceContentQualityPluginConfig,
    fixture_dir: Option<PathBuf>,
    worker_script: PathBuf,
}

impl WorkerClient {
    pub fn new(config: DataSourceContentQualityPluginConfig) -> Result<Self, std::io::Error> {
        let fixture_dir = std::env::var(FIXTURE_ENV)
            .ok()
            .filter(|d| !d.trim().is_empty())
            .map(PathBuf::from);
        let worker_script = resolve_worker_script()?;
        Ok(Self {
            config,
            fixture_dir,
            worker_script,
        })
    }

    pub async fn render(&self, url: &str) -> Result<FetchResponse, std::io::Error> {
        let job = build_job_request(&self.config, url);
        let result = if let Some(dir) = &self.fixture_dir {
            self.run_fixture_job(dir, &job).await?
        } else {
            self.run_live_worker(&job).await?
        };
        if !result.ok {
            return Err(std::io::Error::other(format!(
                "content quality worker failed: {}",
                result
                    .error
                    .unwrap_or_else(|| serde_json::json!({"message":"unknown"}))
            )));
        }
        Ok(FetchResponse {
            final_url: result.final_url.unwrap_or_else(|| url.to_string()),
            status: result.status.unwrap_or(200),
            headers: std::collections::HashMap::from([(
                "content-type".into(),
                "text/html; charset=utf-8".into(),
            )]),
            body: result.html.unwrap_or_default(),
            redirect_chain: vec![result.status.unwrap_or(200)],
            ttfb_ms: 0,
        })
    }

    async fn run_fixture_job(
        &self,
        dir: &Path,
        job: &WorkerJobRequest,
    ) -> Result<WorkerJobResult, std::io::Error> {
        let slug = fixture_slug(&job.url);
        let candidates = [
            dir.join(format!("{slug}.json")),
            dir.join(format!("{slug}.html")),
            dir.join("rendered.html"),
        ];
        for path in candidates {
            if path.exists() {
                let raw = tokio::fs::read_to_string(&path)
                    .await
                    .map_err(std::io::Error::other)?;
                if path.extension().and_then(|e| e.to_str()) == Some("json") {
                    let mut result: WorkerJobResult = serde_json::from_str(&raw).map_err(|e| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("fixture {}: {e}", path.display()),
                        )
                    })?;
                    result.job_id = job.job_id.clone();
                    return Ok(result);
                }
                return Ok(WorkerJobResult {
                    job_id: job.job_id.clone(),
                    ok: true,
                    final_url: Some(job.url.clone()),
                    status: Some(200),
                    html: Some(raw),
                    error: None,
                });
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no rendered fixture for {} in {}", job.url, dir.display()),
        ))
    }

    async fn run_live_worker(
        &self,
        job: &WorkerJobRequest,
    ) -> Result<WorkerJobResult, std::io::Error> {
        let mut command = Command::new(&self.config.worker_node_path);
        command
            .arg(&self.worker_script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Ok(path) = std::env::var("PLAYWRIGHT_BROWSERS_PATH") {
            if !path.trim().is_empty() {
                command.env("PLAYWRIGHT_BROWSERS_PATH", path);
            }
        }
        if let Some(path) = &self.config.playwright_executable_path {
            command.env("PLAYWRIGHT_EXECUTABLE_PATH", path);
        }
        info!(
            url = %job.url,
            script = %self.worker_script.display(),
            "Content Quality worker: spawning Node render process"
        );
        let mut child = command.spawn().map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "failed to spawn content quality worker ({}): {e}",
                    self.worker_script.display()
                ),
            )
        })?;

        let mut stdin = child.stdin.take().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "worker stdin unavailable")
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "worker stdout unavailable")
        })?;

        let request_line = serde_json::to_string(job).map_err(std::io::Error::other)?;
        stdin
            .write_all(format!("{request_line}\n").as_bytes())
            .await
            .map_err(std::io::Error::other)?;
        stdin.shutdown().await.map_err(std::io::Error::other)?;

        let mut lines = BufReader::new(stdout).lines();
        let response_line = lines
            .next_line()
            .await
            .map_err(std::io::Error::other)?
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "worker produced no output",
                )
            })?;
        let _ = child.start_kill();
        let _ = child.wait().await;
        parse_result_line(&response_line)
    }
}

pub fn resolve_worker_script() -> Result<PathBuf, std::io::Error> {
    skippr_runtime_sdk::content_quality_worker::resolve_content_quality_worker_script(Some(
        Path::new(env!("CARGO_MANIFEST_DIR")),
    ))
}

fn fixture_slug(url: &str) -> String {
    url.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_protocol_roundtrip() {
        let raw = r#"{"job_id":"j1","ok":true,"final_url":"https://example.com/","status":200,"html":"<html></html>"}"#;
        let result = parse_result_line(raw).expect("parse fixture");
        assert!(result.ok);
        assert_eq!(result.html.as_deref(), Some("<html></html>"));
    }
}
