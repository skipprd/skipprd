use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_derive::Serialize;
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

use crate::app_match::{match_targets, TargetMatchRow};
use crate::config::{DataSourceAppleAppStoreSerpPluginConfig, TargetEntry};

pub const FIXTURE_ENV: &str = "SKIPPR_APPLE_APP_STORE_SERP_FIXTURE_DIR";

const ITUNES_SEARCH_BASE: &str = "https://itunes.apple.com/search";
const DEFAULT_USER_AGENT: &str = "SkipprAppleAppStoreSerp/1.0 (+https://skippr.io)";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppResultRow {
    pub position: u32,
    pub app_id: String,
    pub bundle_id: Option<String>,
    pub track_name: Option<String>,
    pub artist_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ItunesSearchJob {
    pub job_id: String,
    pub keyword: String,
    pub storefront: String,
    pub entity: String,
    pub limit: u32,
    pub targets: Vec<TargetEntry>,
    pub stop_after_first_target_match: bool,
    pub capture_results: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ItunesSearchResult {
    pub job_id: String,
    pub ok: bool,
    pub status: String,
    #[serde(default)]
    pub results: Vec<AppResultRow>,
    #[serde(default)]
    pub target_matches: Vec<TargetMatchRow>,
    #[serde(default)]
    pub results_inspected: u32,
    #[serde(default)]
    pub search_url_hash: Option<String>,
    #[serde(default)]
    pub error: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct ItunesApiResponse {
    #[serde(default)]
    results: Vec<ItunesApiResult>,
}

#[derive(Debug, Deserialize)]
struct ItunesApiResult {
    #[serde(default, rename = "trackId")]
    track_id: Option<serde_json::Value>,
    #[serde(default, rename = "bundleId")]
    bundle_id: Option<String>,
    #[serde(default, rename = "trackName")]
    track_name: Option<String>,
    #[serde(default, rename = "artistName")]
    artist_name: Option<String>,
}

pub fn build_search_job(
    config: &DataSourceAppleAppStoreSerpPluginConfig,
    keyword: &str,
    storefront: &str,
    max_depth: u32,
) -> ItunesSearchJob {
    ItunesSearchJob {
        job_id: Uuid::new_v4().to_string(),
        keyword: keyword.to_string(),
        storefront: storefront.to_string(),
        entity: config.entity.as_str().to_string(),
        limit: max_depth.min(200),
        targets: config.targets.clone(),
        stop_after_first_target_match: config.stop_after_first_target_match,
        capture_results: config.capture_results,
    }
}

pub fn build_search_url(job: &ItunesSearchJob) -> Result<String, std::io::Error> {
    let mut url = Url::parse(ITUNES_SEARCH_BASE).map_err(std::io::Error::other)?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("term", &job.keyword);
        pairs.append_pair("country", &job.storefront);
        pairs.append_pair("entity", &job.entity);
        pairs.append_pair("limit", &job.limit.to_string());
    }
    Ok(url.to_string())
}

fn hash_url(url: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(url.as_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

fn track_id_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Number(n) => n.as_u64().map(|v| v.to_string()),
        serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
}

pub fn parse_itunes_response(
    job: &ItunesSearchJob,
    body: &str,
    search_url: &str,
) -> Result<ItunesSearchResult, std::io::Error> {
    let parsed: ItunesApiResponse = serde_json::from_str(body).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid iTunes JSON: {e}"),
        )
    })?;

    let mut results = Vec::new();
    for (idx, item) in parsed.results.iter().enumerate() {
        let app_id = item
            .track_id
            .as_ref()
            .and_then(track_id_string)
            .unwrap_or_default();
        if app_id.is_empty() {
            continue;
        }
        results.push(AppResultRow {
            position: (idx + 1) as u32,
            app_id,
            bundle_id: item.bundle_id.clone(),
            track_name: item.track_name.clone(),
            artist_name: item.artist_name.clone(),
        });
    }

    let target_matches = match_targets(
        &job.targets,
        &results,
        job.stop_after_first_target_match,
    );

    Ok(ItunesSearchResult {
        job_id: job.job_id.clone(),
        ok: true,
        status: "ok".into(),
        results_inspected: results.len() as u32,
        results,
        target_matches,
        search_url_hash: Some(hash_url(search_url)),
        error: None,
    })
}

pub struct ItunesClient {
    config: DataSourceAppleAppStoreSerpPluginConfig,
    fixture_dir: Option<PathBuf>,
    http: Option<reqwest::Client>,
}

impl ItunesClient {
    pub fn new(config: DataSourceAppleAppStoreSerpPluginConfig) -> Result<Self, std::io::Error> {
        let fixture_dir = std::env::var(FIXTURE_ENV)
            .ok()
            .filter(|d| !d.trim().is_empty())
            .map(PathBuf::from);
        let http = if fixture_dir.is_none() {
            Some(
                reqwest::Client::builder()
                    .user_agent(
                        config
                            .user_agent
                            .as_deref()
                            .unwrap_or(DEFAULT_USER_AGENT),
                    )
                    .build()
                    .map_err(std::io::Error::other)?,
            )
        } else {
            None
        };
        Ok(Self {
            config,
            fixture_dir,
            http,
        })
    }

    pub async fn run_search(&self, job: &ItunesSearchJob) -> Result<ItunesSearchResult, std::io::Error> {
        if let Some(dir) = &self.fixture_dir {
            return self.run_fixture_search(dir, job).await;
        }
        self.run_live_search(job).await
    }

    async fn run_live_search(&self, job: &ItunesSearchJob) -> Result<ItunesSearchResult, std::io::Error> {
        let search_url = build_search_url(job)?;
        let client = self.http.as_ref().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "HTTP client not initialized")
        })?;

        let response = client.get(&search_url).send().await.map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("iTunes request failed: {e}"),
            )
        })?;

        if !response.status().is_success() {
            return Ok(ItunesSearchResult {
                job_id: job.job_id.clone(),
                ok: false,
                status: "error".into(),
                results: vec![],
                target_matches: vec![],
                results_inspected: 0,
                search_url_hash: Some(hash_url(&search_url)),
                error: Some(serde_json::json!({
                    "code": "HTTP_ERROR",
                    "status": response.status().as_u16(),
                })),
            });
        }

        let body = response.text().await.map_err(std::io::Error::other)?;
        parse_itunes_response(job, &body, &search_url)
    }

    async fn run_fixture_search(
        &self,
        dir: &Path,
        job: &ItunesSearchJob,
    ) -> Result<ItunesSearchResult, std::io::Error> {
        let slug = fixture_slug(&job.keyword);
        let candidates = [
            dir.join(format!("{slug}.json")),
            dir.join("search_ok.json"),
            dir.join("http_error.json"),
        ];
        for path in candidates {
            if path.exists() {
                let raw = tokio::fs::read_to_string(&path)
                    .await
                    .map_err(std::io::Error::other)?;
                let mut result: ItunesSearchResult = serde_json::from_str(&raw).map_err(|e| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("fixture {}: {e}", path.display()),
                    )
                })?;
                result.job_id = job.job_id.clone();
                if result.target_matches.is_empty() && !result.results.is_empty() {
                    result.target_matches = match_targets(
                        &job.targets,
                        &result.results,
                        job.stop_after_first_target_match,
                    );
                }
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
        tokio::time::sleep(std::time::Duration::from_millis(
            self.config.min_query_interval_ms,
        ))
        .await;
    }
}

fn fixture_slug(keyword: &str) -> String {
    keyword
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TargetEntry;

    #[test]
    fn builds_search_url() {
        let job = ItunesSearchJob {
            job_id: "j".into(),
            keyword: "photo editor".into(),
            storefront: "us".into(),
            entity: "software".into(),
            limit: 50,
            targets: vec![],
            stop_after_first_target_match: true,
            capture_results: false,
        };
        let url = build_search_url(&job).unwrap();
        assert!(url.contains("term=photo"));
        assert!(url.contains("country=us"));
        assert!(url.contains("entity=software"));
        assert!(url.contains("limit=50"));
    }

    #[test]
    fn parses_itunes_json() {
        let job = ItunesSearchJob {
            job_id: "j".into(),
            keyword: "test".into(),
            storefront: "us".into(),
            entity: "software".into(),
            limit: 10,
            targets: vec![TargetEntry {
                app_id: "123456789".into(),
                bundle_id: None,
                aliases: vec![],
            }],
            stop_after_first_target_match: true,
            capture_results: false,
        };
        let body = r#"{
            "resultCount": 1,
            "results": [{
                "trackId": 123456789,
                "bundleId": "com.example.app",
                "trackName": "Example",
                "artistName": "Dev"
            }]
        }"#;
        let result = parse_itunes_response(&job, body, "https://itunes.apple.com/search").unwrap();
        assert_eq!(result.status, "ok");
        assert_eq!(result.results.len(), 1);
        assert_eq!(result.results[0].position, 1);
        assert!(result.target_matches[0].found);
    }
}
