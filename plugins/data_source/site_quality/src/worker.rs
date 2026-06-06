use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use serde_derive::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::sleep;
use tracing::info;
use uuid::Uuid;

use crate::checkpoint::PageCheckpoint;
use crate::config::{
    default_user_agent_for_profile, lighthouse_form_factor_for_profile,
    DataSourceSiteQualityPluginConfig, DeviceProfile, ThrottleConfig, Viewport,
};

pub const FIXTURE_ENV: &str = "SKIPPR_SITE_QUALITY_FIXTURE_DIR";

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
    pub throttle: ThrottleConfig,
    pub lighthouse_enabled: bool,
    pub lighthouse_categories: Vec<String>,
    pub axe_enabled: bool,
    pub axe_tags: Vec<String>,
    #[serde(default)]
    pub skip_heavy_audits: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prior_checkpoint: Option<PageCheckpoint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lighthouse_form_factor: Option<String>,
    #[serde(default = "default_web_vitals_settle_ms")]
    pub web_vitals_settle_ms: u32,
    #[serde(default = "default_collect_inp")]
    pub collect_inp: bool,
}

fn default_web_vitals_settle_ms() -> u32 {
    2500
}

fn default_collect_inp() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TimingsMs {
    pub dom_content_loaded: Option<f64>,
    pub load: Option<f64>,
    pub fully_loaded: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WebVitals {
    pub lcp: Option<f64>,
    pub inp: Option<f64>,
    pub cls: Option<f64>,
    pub fcp: Option<f64>,
    pub ttfb: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MobileHeuristics {
    pub viewport_meta_ok: Option<bool>,
    pub horizontal_scroll: Option<bool>,
    pub text_too_small_count: Option<u32>,
    pub tap_target_issues: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SocialPreview {
    pub title: Option<String>,
    pub description: Option<String>,
    pub image: Option<String>,
    pub url: Option<String>,
    pub card: Option<String>,
    pub card_title: Option<String>,
    pub card_description: Option<String>,
    pub card_image: Option<String>,
    pub title_present: Option<bool>,
    pub description_present: Option<bool>,
    pub image_present: Option<bool>,
    pub url_present: Option<bool>,
    pub card_present: Option<bool>,
    pub card_title_present: Option<bool>,
    pub card_description_present: Option<bool>,
    pub card_image_present: Option<bool>,
    #[serde(default)]
    pub missing_fields: Vec<String>,
    #[serde(default)]
    pub card_missing_fields: Vec<String>,
    pub complete: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LighthouseScores {
    pub performance: Option<f64>,
    pub accessibility: Option<f64>,
    #[serde(rename = "best_practices")]
    pub best_practices: Option<f64>,
    pub seo: Option<f64>,
    #[serde(default)]
    pub top_failing_audits: Vec<LighthouseAudit>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LighthouseAudit {
    pub id: String,
    pub score: Option<f64>,
    pub display_value: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AxeViolation {
    pub id: String,
    pub impact: Option<String>,
    pub help: Option<String>,
    pub nodes: Option<u32>,
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
    pub redirect_count: Option<u32>,
    #[serde(default)]
    pub timings_ms: Option<TimingsMs>,
    #[serde(default)]
    pub web_vitals: Option<WebVitals>,
    #[serde(default)]
    pub render_hash: Option<String>,
    #[serde(default)]
    pub mobile_heuristics: Option<MobileHeuristics>,
    #[serde(default)]
    pub social_preview: Option<SocialPreview>,
    #[serde(default)]
    pub lighthouse: Option<LighthouseScores>,
    #[serde(default)]
    pub axe_violations: Option<Vec<AxeViolation>>,
    #[serde(default)]
    pub error: Option<serde_json::Value>,
    #[serde(default)]
    pub skip_heavy_audits: Option<bool>,
}

pub fn build_job_request(
    config: &DataSourceSiteQualityPluginConfig,
    url: &str,
    device: &DeviceProfile,
    skip_heavy: bool,
    prior: Option<PageCheckpoint>,
) -> WorkerJobRequest {
    WorkerJobRequest {
        job_id: Uuid::new_v4().to_string(),
        url: url.to_string(),
        device_profile: device.profile.clone(),
        viewport: device.viewport.clone(),
        user_agent: device
            .user_agent
            .clone()
            .or_else(|| default_user_agent_for_profile(&device.profile)),
        wait_until: config.wait_until.clone(),
        navigation_timeout_ms: config.navigation_timeout_ms,
        throttle: config.throttle.clone(),
        lighthouse_enabled: config.lighthouse_enabled,
        lighthouse_categories: config.lighthouse_categories.clone(),
        axe_enabled: config.axe_enabled,
        axe_tags: config.axe_tags.clone(),
        skip_heavy_audits: skip_heavy,
        prior_checkpoint: prior,
        lighthouse_form_factor: Some(lighthouse_form_factor_for_profile(&device.profile)),
        web_vitals_settle_ms: default_web_vitals_settle_ms(),
        collect_inp: default_collect_inp(),
    }
}

pub fn parse_result_line(line: &str) -> Result<WorkerJobResult, std::io::Error> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "empty worker result line",
        ));
    }
    serde_json::from_str(trimmed).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid worker result JSON: {e}"),
        )
    })
}

pub struct WorkerClient {
    config: DataSourceSiteQualityPluginConfig,
    fixture_dir: Option<PathBuf>,
    worker_script: PathBuf,
    last_skip_heavy: std::sync::Mutex<Option<bool>>,
}

impl WorkerClient {
    pub fn new(config: DataSourceSiteQualityPluginConfig) -> Result<Self, std::io::Error> {
        let fixture_dir = std::env::var(FIXTURE_ENV)
            .ok()
            .filter(|d| !d.trim().is_empty())
            .map(PathBuf::from);
        let worker_script = resolve_worker_script()?;
        Ok(Self {
            config,
            fixture_dir,
            worker_script,
            last_skip_heavy: std::sync::Mutex::new(None),
        })
    }

    pub fn worker_script_path(&self) -> &Path {
        &self.worker_script
    }

    pub fn last_skip_heavy_audits(&self) -> Option<bool> {
        *self.last_skip_heavy.lock().unwrap()
    }

    pub async fn run_job(&self, job: &WorkerJobRequest) -> Result<WorkerJobResult, std::io::Error> {
        *self.last_skip_heavy.lock().unwrap() = Some(job.skip_heavy_audits);
        if let Some(dir) = &self.fixture_dir {
            return self.run_fixture_job(dir, job).await;
        }
        self.run_live_worker(job).await
    }

    async fn run_fixture_job(
        &self,
        dir: &Path,
        job: &WorkerJobRequest,
    ) -> Result<WorkerJobResult, std::io::Error> {
        let slug = fixture_slug(&job.url, &job.device_profile);
        let candidates = [
            dir.join(format!("{slug}.json")),
            dir.join("worker_result.json"),
            dir.join("sample_result_mobile.json"),
        ];
        for path in candidates {
            if path.exists() {
                let raw = tokio::fs::read_to_string(&path)
                    .await
                    .map_err(std::io::Error::other)?;
                let mut result: WorkerJobResult = serde_json::from_str(&raw).map_err(|e| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("fixture {}: {e}", path.display()),
                    )
                })?;
                result.job_id = job.job_id.clone();
                let unchanged = job
                    .prior_checkpoint
                    .as_ref()
                    .zip(result.render_hash.as_ref())
                    .is_some_and(|(prior, hash)| prior.render_hash == *hash);
                if job.skip_heavy_audits || unchanged {
                    if let Some(cp) = job.prior_checkpoint.as_ref() {
                        result.lighthouse = Some(LighthouseScores {
                            performance: cp.lh_performance,
                            accessibility: cp.lh_accessibility,
                            best_practices: cp.lh_best_practices,
                            seo: cp.lh_seo,
                            top_failing_audits: Vec::new(),
                        });
                    }
                    result.axe_violations = Some(Vec::new());
                    result.skip_heavy_audits = Some(true);
                }
                return Ok(result);
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no fixture for {} in {}", job.url, dir.display()),
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
            // Playwright/Chromium can be verbose on stderr; if we only read stderr after
            // wait(), a full pipe will block the child and hang sync indefinitely.
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
            device = %job.device_profile,
            script = %self.worker_script.display(),
            "Site Quality worker: spawning Node process"
        );
        let mut child = command.spawn().map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "failed to spawn site quality worker ({}): {e}",
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

        // One job per process; after the result line we do not need to wait for Playwright
        // browser teardown (can hang for minutes on some platforms).
        let _ = child.start_kill();
        let _ = child.wait().await;

        parse_result_line(&response_line)
    }

    pub async fn throttle_delay(&self) {
        let ppm = self.config.pages_per_minute.max(1);
        let delay_ms = 60_000 / ppm as u64;
        sleep(Duration::from_millis(delay_ms)).await;
    }
}

pub fn resolve_worker_script() -> Result<PathBuf, std::io::Error> {
    skippr_runtime_sdk::site_quality_worker::resolve_site_quality_worker_script(Some(Path::new(
        env!("CARGO_MANIFEST_DIR"),
    )))
}

fn fixture_slug(url: &str, device: &str) -> String {
    let sanitized: String = url
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("{sanitized}_{device}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_protocol_roundtrip() {
        let raw = include_str!("../fixtures/sample_result_mobile.json");
        let result = parse_result_line(raw).expect("parse fixture");
        assert!(result.ok);
        assert_eq!(result.job_id, "fixture-mobile");
        assert!(result.web_vitals.as_ref().unwrap().lcp.is_some());
    }
}
