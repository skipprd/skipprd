use serde::Deserialize;
use serde_derive::Serialize;
use std::path::{Path, PathBuf};
use uuid::Uuid;

use crate::brightdata::BrightDataClient;
use crate::config::DataSourceGoogleSerpRanksPluginConfig;

pub const FIXTURE_ENV: &str = "SKIPPR_GOOGLE_SERP_RANKS_FIXTURE_DIR";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkerJobRequest {
    pub job_id: String,
    pub keyword: String,
    pub country: String,
    pub language: String,
    pub device: String,
    pub max_depth: u32,
    pub targets: Vec<String>,
    pub stop_after_first_target_match: bool,
    pub capture_results: bool,
    pub navigation_timeout_ms: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint_last_position: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint_last_page_start: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OrganicResultRow {
    pub position: u32,
    pub title: Option<String>,
    pub url: String,
    pub domain: String,
    pub snippet: Option<String>,
    pub page_start: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TargetMatchRow {
    pub target_site: String,
    pub matched_url: Option<String>,
    pub matched_domain: Option<String>,
    pub position: Option<u32>,
    pub page_start: Option<u32>,
    pub found: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkerJobResult {
    pub job_id: String,
    pub ok: bool,
    pub status: String,
    #[serde(default)]
    pub blocked_reason: Option<String>,
    #[serde(default)]
    pub organic_results: Vec<OrganicResultRow>,
    #[serde(default)]
    pub target_matches: Vec<TargetMatchRow>,
    #[serde(default)]
    pub results_inspected: u32,
    #[serde(default)]
    pub pages_fetched: u32,
    #[serde(default)]
    pub search_url_hash: Option<String>,
    #[serde(default)]
    pub error: Option<serde_json::Value>,
}

pub fn build_job_request(
    config: &DataSourceGoogleSerpRanksPluginConfig,
    keyword: &str,
    max_depth: u32,
    targets: Vec<String>,
    prior: Option<&crate::checkpoint::QueryCheckpoint>,
) -> WorkerJobRequest {
    WorkerJobRequest {
        job_id: Uuid::new_v4().to_string(),
        keyword: keyword.to_string(),
        country: config.country.clone(),
        language: config.language.clone(),
        device: config.device.as_str().to_string(),
        max_depth,
        targets,
        stop_after_first_target_match: config.stop_after_first_target_match,
        capture_results: config.capture_results,
        navigation_timeout_ms: config.navigation_timeout_ms,
        user_agent: config.user_agent.clone(),
        hint_last_position: prior.and_then(|cp| cp.last_position),
        hint_last_page_start: prior.and_then(|cp| cp.last_page_start),
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
    config: DataSourceGoogleSerpRanksPluginConfig,
    fixture_dir: Option<PathBuf>,
    brightdata: Option<BrightDataClient>,
}

impl WorkerClient {
    pub fn new(config: DataSourceGoogleSerpRanksPluginConfig) -> Result<Self, std::io::Error> {
        let fixture_dir = std::env::var(FIXTURE_ENV)
            .ok()
            .filter(|d| !d.trim().is_empty())
            .map(PathBuf::from);
        let brightdata = if fixture_dir.is_none() {
            Some(BrightDataClient::new(config.clone())?)
        } else {
            None
        };
        Ok(Self {
            config,
            fixture_dir,
            brightdata,
        })
    }

    pub async fn run_job(&self, job: &WorkerJobRequest) -> Result<WorkerJobResult, std::io::Error> {
        if let Some(dir) = &self.fixture_dir {
            return self.run_fixture_job(dir, job).await;
        }
        let client = self.brightdata.as_ref().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Bright Data client not initialized",
            )
        })?;
        client.run_job(job).await
    }

    async fn run_fixture_job(
        &self,
        dir: &Path,
        job: &WorkerJobRequest,
    ) -> Result<WorkerJobResult, std::io::Error> {
        let slug = fixture_slug(&job.keyword);
        let candidates = [
            dir.join(format!("{slug}.json")),
            dir.join("worker_result_ok.json"),
            dir.join("worker_result_blocked.json"),
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
                return Ok(result);
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "no fixture for keyword {:?} in {}",
                job.keyword,
                dir.display()
            ),
        ))
    }

    pub async fn throttle_delay(&self) {
        if let Some(client) = &self.brightdata {
            client.throttle_delay().await;
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(
                self.config.min_query_interval_ms,
            ))
            .await;
        }
    }
}

fn fixture_slug(keyword: &str) -> String {
    keyword
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SerpDevice;

    #[test]
    fn worker_protocol_roundtrip() {
        let raw = include_str!("../fixtures/worker_result_ok.json");
        let result = parse_result_line(raw).expect("parse fixture");
        assert!(result.ok);
        assert_eq!(result.status, "ok");
        assert!(!result.target_matches.is_empty());
    }

    #[test]
    fn parse_rejects_empty_line() {
        assert!(parse_result_line("  \n").is_err());
    }

    #[test]
    fn parse_rejects_invalid_json() {
        assert!(parse_result_line("{not json}").is_err());
    }

    #[test]
    fn fixture_slug_sanitizes_keyword() {
        assert_eq!(fixture_slug("not found query"), "not_found_query");
    }

    #[tokio::test]
    async fn unhappy_fixture_missing_returns_not_found() {
        let temp = tempfile::tempdir().unwrap();
        std::env::set_var(FIXTURE_ENV, temp.path());

        let cfg = DataSourceGoogleSerpRanksPluginConfig {
            targets: vec![crate::config::TargetEntry {
                site: "x.com".into(),
                aliases: vec![],
            }],
            keywords: vec!["missing".into()],
            country: "uk".into(),
            language: "en".into(),
            device: SerpDevice::Desktop,
            max_depth: 10,
            min_query_interval_ms: 5_000,
            max_queries_per_run: 1,
            stop_after_first_target_match: true,
            capture_results: false,
            force_refresh_today: false,
            navigation_timeout_ms: 45_000,
            worker_node_path: "node".into(),
            playwright_executable_path: None,
            user_agent: None,
            brightdata_zone: Some("serp_api1".into()),
            brightdata_api_base: None,
        };
        let client = WorkerClient::new(cfg).unwrap();
        let job = build_job_request(
            &DataSourceGoogleSerpRanksPluginConfig {
                targets: vec![crate::config::TargetEntry {
                    site: "x.com".into(),
                    aliases: vec![],
                }],
                keywords: vec!["missing".into()],
                country: "uk".into(),
                language: "en".into(),
                device: SerpDevice::Desktop,
                max_depth: 10,
                min_query_interval_ms: 5_000,
                max_queries_per_run: 1,
                stop_after_first_target_match: true,
                capture_results: false,
                force_refresh_today: false,
                navigation_timeout_ms: 45_000,
                worker_node_path: "node".into(),
                playwright_executable_path: None,
                user_agent: None,
                brightdata_zone: Some("serp_api1".into()),
                brightdata_api_base: None,
            },
            "no-such-fixture-keyword-xyz",
            10,
            vec!["x.com".into()],
            None,
        );
        let err = client.run_job(&job).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        std::env::remove_var(FIXTURE_ENV);
    }

    #[test]
    fn build_job_uses_desktop_device_string() {
        let mut cfg = DataSourceGoogleSerpRanksPluginConfig {
            targets: vec![crate::config::TargetEntry {
                site: "example.com".into(),
                aliases: vec![],
            }],
            keywords: vec!["kw".into()],
            country: "uk".into(),
            language: "en".into(),
            device: SerpDevice::Desktop,
            max_depth: 10,
            min_query_interval_ms: 30_000,
            max_queries_per_run: 5,
            stop_after_first_target_match: true,
            capture_results: false,
            force_refresh_today: false,
            navigation_timeout_ms: 45_000,
            worker_node_path: "node".into(),
            playwright_executable_path: None,
            user_agent: None,
            brightdata_zone: None,
            brightdata_api_base: None,
        };
        cfg.device = SerpDevice::Mobile;
        let job = build_job_request(&cfg, "kw", 10, vec!["example.com".into()], None);
        assert_eq!(job.device, "mobile");
    }
}
