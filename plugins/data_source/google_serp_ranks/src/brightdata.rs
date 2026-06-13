use std::time::Duration;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tracing::info;
use url::Url;

use crate::config::{DataSourceGoogleSerpRanksPluginConfig, SerpDevice};
use crate::domain::{domain_matches_target, normalize_domain};
use crate::worker::{OrganicResultRow, TargetMatchRow, WorkerJobRequest, WorkerJobResult};

pub const API_KEY_ENV: &str = "BRIGHTDATA_API_KEY";
pub const ZONE_ENV: &str = "BRIGHTDATA_ZONE";
const DEFAULT_API_BASE: &str = "https://api.brightdata.com";
const DEFAULT_ZONE: &str = "serp_api1";

#[derive(Debug, Clone)]
pub struct BrightDataClient {
    http: Client,
    api_key: String,
    zone: String,
    api_base: String,
    config: DataSourceGoogleSerpRanksPluginConfig,
}

impl BrightDataClient {
    pub fn new(config: DataSourceGoogleSerpRanksPluginConfig) -> Result<Self, std::io::Error> {
        config.validate()?;
        let api_key = std::env::var(API_KEY_ENV)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("{API_KEY_ENV} is required for Google SERP ranks (Bright Data)"),
                )
            })?;
        let zone = config
            .brightdata_zone
            .clone()
            .filter(|z| !z.trim().is_empty())
            .or_else(|| {
                std::env::var(ZONE_ENV)
                    .ok()
                    .filter(|z| !z.trim().is_empty())
            })
            .unwrap_or_else(|| DEFAULT_ZONE.to_string());
        let api_base = config
            .brightdata_api_base
            .clone()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_API_BASE.to_string());
        // Bright Data can reject HTTP/2 from some egress paths (e.g. Lambda); stick to HTTP/1.1.
        let http = Client::builder()
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(30))
            .http1_only()
            .build()
            .map_err(std::io::Error::other)?;
        Ok(Self {
            http,
            api_key,
            zone,
            api_base,
            config,
        })
    }

    pub async fn run_job(&self, job: &WorkerJobRequest) -> Result<WorkerJobResult, std::io::Error> {
        let device = if job.device.eq_ignore_ascii_case("mobile") {
            SerpDevice::Mobile
        } else {
            SerpDevice::Desktop
        };
        let page_starts = page_starts_for_rescan(job.hint_last_position, job.max_depth);
        let page_count = page_starts.len();
        let mut merged: Option<WorkerJobResult> = None;
        let mut first_hash: Option<String> = None;
        let mut pages_fetched = 0u32;

        for page_start in page_starts {
            let search_url =
                build_search_url(&job.keyword, &job.country, &job.language, device, page_start);
            let search_url_hash = hash_search_url(&search_url);
            if first_hash.is_none() {
                first_hash = Some(search_url_hash.clone());
            }

            info!(
                keyword = %job.keyword,
                zone = %self.zone,
                page_start,
                search_url = %search_url,
                "Bright Data SERP: requesting parsed results"
            );

            let body = serde_json::json!({
                "zone": self.zone,
                "url": search_url,
                "format": "raw",
                "data_format": "parsed_light"
            });

            let endpoint = format!("{}/request", self.api_base.trim_end_matches('/'));
            let response = self
                .http
                .post(&endpoint)
                .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
                .header(CONTENT_TYPE, "application/json")
                .json(&body)
                .send()
                .await
                .map_err(std::io::Error::other)?;

            let status = response.status();
            let raw_text = response.text().await.map_err(std::io::Error::other)?;

            if !status.is_success() {
                return Ok(error_result(
                    job,
                    first_hash.unwrap_or(search_url_hash),
                    "BRIGHTDATA_HTTP_ERROR",
                    format!("Bright Data HTTP {}: {}", status, truncate(&raw_text, 500)),
                ));
            }

            let parsed = parse_response_payload(&raw_text)?;
            if let Some(reason) = detect_blocked(&parsed, &raw_text) {
                return Ok(blocked_result(
                    job,
                    first_hash.unwrap_or(search_url_hash),
                    reason,
                    &parsed,
                    pages_fetched + 1,
                ));
            }

            let page_result = build_success_result(
                job,
                search_url_hash,
                &parsed,
                &self.config,
                page_start,
            );
            pages_fetched += 1;
            merged = Some(merge_page_results(merged.take(), page_result, job));

            if merged
                .as_ref()
                .is_some_and(|result| should_stop_pagination(job, result))
            {
                break;
            }

            if organic_entries(&parsed).is_empty() {
                break;
            }

            if pages_fetched < page_count as u32 {
                tokio::time::sleep(Duration::from_millis(self.config.min_query_interval_ms)).await;
            }
        }

        Ok(merged.unwrap_or_else(|| error_result(
            job,
            first_hash.unwrap_or_else(|| "sha256:empty".into()),
            "BRIGHTDATA_EMPTY",
            "No SERP pages fetched".into(),
        )))
    }

    pub async fn throttle_delay(&self) {
        tokio::time::sleep(Duration::from_millis(self.config.min_query_interval_ms)).await;
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

pub fn build_search_url(
    keyword: &str,
    country: &str,
    language: &str,
    device: SerpDevice,
    start: u32,
) -> String {
    let mut url = Url::parse("https://www.google.com/search").expect("valid google search base");
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("q", keyword);
        q.append_pair("hl", language);
        if !country.trim().is_empty() {
            q.append_pair("gl", country.trim());
        }
        q.append_pair("start", &start.to_string());
        q.append_pair("num", "10");
        q.append_pair("brd_json", "1");
    }
    if device == SerpDevice::Mobile {
        url.query_pairs_mut().append_pair("brd_mobile", "1");
    }
    url.to_string()
}

pub fn hash_search_url(url: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(url.as_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

fn parse_response_payload(raw: &str) -> Result<Value, std::io::Error> {
    let value: Value = serde_json::from_str(raw).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Bright Data response is not JSON: {e}"),
        )
    })?;
    if let Some(inner) = value.get("body").and_then(|b| b.as_str()) {
        if let Ok(parsed) = serde_json::from_str::<Value>(inner) {
            return Ok(parsed);
        }
    }
    Ok(value)
}

fn detect_blocked(parsed: &Value, raw: &str) -> Option<String> {
    if parsed
        .get("status")
        .and_then(|s| s.as_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("error"))
    {
        return Some(
            parsed
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("brightdata_error")
                .to_string(),
        );
    }
    let haystack = raw.to_lowercase();
    if haystack.contains("captcha") || haystack.contains("unusual traffic") {
        return Some("captcha".into());
    }
    if haystack.contains("/sorry/") {
        return Some("google_sorry_redirect".into());
    }
    None
}

#[derive(Debug, Deserialize)]
struct BrightOrganicRow {
    link: Option<String>,
    url: Option<String>,
    title: Option<String>,
    description: Option<String>,
    snippet: Option<String>,
    rank: Option<u32>,
    global_rank: Option<u32>,
    position: Option<u32>,
}

fn organic_entries(parsed: &Value) -> Vec<BrightOrganicRow> {
    let arrays = [
        parsed.get("organic"),
        parsed.get("oragnic"),
        parsed.get("results"),
    ];
    for arr in arrays {
        let Some(items) = arr.and_then(|a| a.as_array()) else {
            continue;
        };
        let mut out = Vec::new();
        for item in items {
            if item.get("type").and_then(|t| t.as_str()) == Some("organic")
                || item.get("link").is_some()
                || item.get("url").is_some()
            {
                if let Ok(row) = serde_json::from_value::<BrightOrganicRow>(item.clone()) {
                    if row.link.is_some() || row.url.is_some() {
                        out.push(row);
                    }
                }
            }
        }
        if !out.is_empty() {
            return out;
        }
        if let Ok(rows) =
            serde_json::from_value::<Vec<BrightOrganicRow>>(Value::Array(items.clone()))
        {
            let filtered: Vec<_> = rows
                .into_iter()
                .filter(|r| r.link.is_some() || r.url.is_some())
                .collect();
            if !filtered.is_empty() {
                return filtered;
            }
        }
    }
    vec![]
}

fn row_url(row: &BrightOrganicRow) -> Option<String> {
    row.link.clone().or_else(|| row.url.clone())
}

fn row_snippet(row: &BrightOrganicRow) -> Option<String> {
    row.description.clone().or_else(|| row.snippet.clone())
}

fn row_position(row: &BrightOrganicRow, index: usize, page_start: u32) -> u32 {
    row.global_rank
        .or(row.rank)
        .or(row.position)
        .unwrap_or(page_start + (index as u32) + 1)
}

fn page_starts_for_rescan(last_position: Option<u32>, max_depth: u32) -> Vec<u32> {
    let max_pages = ((max_depth as usize + 9) / 10).clamp(1, 10);
    let linear: Vec<u32> = (0..max_pages as u32).map(|page| page * 10).collect();
    let Some(position) = last_position.filter(|value| *value > 0) else {
        return linear;
    };
    let expected = ((position.saturating_sub(1)) / 10) * 10;
    let mut ordered = vec![expected];
    for step in 1..max_pages {
        if ordered.len() >= 10 {
            break;
        }
        let lower = expected.saturating_sub(step as u32 * 10);
        if lower != expected && !ordered.contains(&lower) {
            ordered.push(lower);
        }
        let upper = expected + (step as u32) * 10;
        if upper < max_pages as u32 * 10 && !ordered.contains(&upper) {
            ordered.push(upper);
        }
    }
    for start in linear {
        if !ordered.contains(&start) {
            ordered.push(start);
        }
    }
    ordered.sort();
    ordered.into_iter().take(10).collect()
}

fn should_stop_pagination(job: &WorkerJobRequest, result: &WorkerJobResult) -> bool {
    if job.stop_after_first_target_match && result.target_matches.iter().any(|row| row.found) {
        return true;
    }
    result.results_inspected >= job.max_depth
}

fn merge_page_results(
    prior: Option<WorkerJobResult>,
    page: WorkerJobResult,
    job: &WorkerJobRequest,
) -> WorkerJobResult {
    let Some(mut merged) = prior else {
        return page;
    };
    merged.pages_fetched = merged.pages_fetched.saturating_add(page.pages_fetched);
    merged.results_inspected = merged.results_inspected.saturating_add(page.results_inspected);
    if job.capture_results {
        merged.organic_results.extend(page.organic_results);
    }
    for (index, target) in merged.target_matches.iter_mut().enumerate() {
        let page_match = page.target_matches.get(index);
        if let Some(candidate) = page_match {
            if !target.found && candidate.found {
                *target = candidate.clone();
            } else if target.found
                && candidate.found
                && candidate.position.unwrap_or(u32::MAX) < target.position.unwrap_or(u32::MAX)
            {
                *target = candidate.clone();
            }
        }
    }
    merged.status = page.status;
    merged.blocked_reason = page.blocked_reason;
    merged.ok = merged.ok && page.ok;
    merged.error = page.error.or(merged.error);
    merged
}

fn build_success_result(
    job: &WorkerJobRequest,
    search_url_hash: String,
    parsed: &Value,
    config: &DataSourceGoogleSerpRanksPluginConfig,
    page_start: u32,
) -> WorkerJobResult {
    let entries = organic_entries(parsed);
    let max_depth = job.max_depth as usize;
    let targets: Vec<String> = job.targets.clone();

    let mut organic_results = Vec::new();
    let mut target_matches: Vec<TargetMatchRow> = targets
        .iter()
        .map(|target| TargetMatchRow {
            target_site: target.clone(),
            matched_url: None,
            matched_domain: None,
            position: None,
            page_start: None,
            found: false,
        })
        .collect();

    for (idx, entry) in entries.iter().take(max_depth).enumerate() {
        let Some(url) = row_url(entry) else {
            continue;
        };
        let domain = normalize_domain(&url);
        if domain.is_empty() {
            continue;
        }
        let position = row_position(entry, idx, page_start);
        if job.capture_results {
            organic_results.push(OrganicResultRow {
                position,
                title: entry.title.clone(),
                url: url.clone(),
                domain: domain.clone(),
                snippet: row_snippet(entry),
                page_start,
            });
        }

        for (i, target) in targets.iter().enumerate() {
            if domain_matches_target(&domain, target) {
                if let Some(slot) = target_matches.get_mut(i) {
                    if !slot.found {
                        slot.found = true;
                        slot.matched_url = Some(url.clone());
                        slot.matched_domain = Some(domain.clone());
                        slot.position = Some(position);
                        slot.page_start = Some(page_start);
                    }
                }
            }
        }

        if config.stop_after_first_target_match && target_matches.iter().any(|m| m.found) {
            break;
        }
    }

    let results_inspected = entries.len().min(max_depth) as u32;

    WorkerJobResult {
        job_id: job.job_id.clone(),
        ok: true,
        status: "ok".into(),
        blocked_reason: None,
        organic_results,
        target_matches,
        results_inspected,
        pages_fetched: 1,
        search_url_hash: Some(search_url_hash),
        error: None,
    }
}

fn blocked_result(
    job: &WorkerJobRequest,
    search_url_hash: String,
    reason: String,
    parsed: &Value,
    pages_fetched: u32,
) -> WorkerJobResult {
    let entries = organic_entries(parsed);
    WorkerJobResult {
        job_id: job.job_id.clone(),
        ok: true,
        status: "blocked".into(),
        blocked_reason: Some(reason),
        organic_results: vec![],
        target_matches: vec![],
        results_inspected: entries.len() as u32,
        pages_fetched,
        search_url_hash: Some(search_url_hash),
        error: None,
    }
}

fn error_result(
    job: &WorkerJobRequest,
    search_url_hash: String,
    code: &str,
    message: String,
) -> WorkerJobResult {
    WorkerJobResult {
        job_id: job.job_id.clone(),
        ok: false,
        status: "error".into(),
        blocked_reason: None,
        organic_results: vec![],
        target_matches: job
            .targets
            .iter()
            .map(|target| TargetMatchRow {
                target_site: target.clone(),
                matched_url: None,
                matched_domain: None,
                position: None,
                page_start: None,
                found: false,
            })
            .collect(),
        results_inspected: 0,
        pages_fetched: 0,
        search_url_hash: Some(search_url_hash),
        error: Some(serde_json::json!({
            "code": code,
            "message": message,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{SerpDevice, TargetEntry};

    #[test]
    fn build_search_url_includes_gl_and_brd_json() {
        let url = build_search_url("pizza", "us", "en", SerpDevice::Desktop, 0);
        assert!(url.contains("q=pizza"));
        assert!(url.contains("gl=us"));
        assert!(url.contains("brd_json=1"));
    }

    #[test]
    fn parse_organic_from_parsed_light_shape() {
        let raw = r#"{
            "organic": [
                {
                    "link": "https://www.example.com/",
                    "title": "Example",
                    "description": "Example site",
                    "global_rank": 3
                }
            ]
        }"#;
        let parsed = parse_response_payload(raw).unwrap();
        let entries = organic_entries(&parsed);
        assert_eq!(entries.len(), 1);
        assert_eq!(row_url(&entries[0]).unwrap(), "https://www.example.com/");
    }

    #[test]
    fn success_result_finds_target() {
        let job = WorkerJobRequest {
            job_id: "j".into(),
            keyword: "test".into(),
            country: "us".into(),
            language: "en".into(),
            device: "desktop".into(),
            max_depth: 10,
            targets: vec!["example.com".into()],
            stop_after_first_target_match: true,
            capture_results: true,
            navigation_timeout_ms: 45_000,
            user_agent: None,
            hint_last_position: None,
            hint_last_page_start: None,
        };
        let parsed: Value = serde_json::from_str(
            r#"{"organic":[{"link":"https://www.example.com/","title":"Ex","global_rank":2}]}"#,
        )
        .unwrap();
        let cfg = DataSourceGoogleSerpRanksPluginConfig {
            targets: vec![TargetEntry {
                site: "example.com".into(),
                aliases: vec![],
            }],
            keywords: vec!["test".into()],
            country: "us".into(),
            language: "en".into(),
            device: SerpDevice::Desktop,
            max_depth: 10,
            min_query_interval_ms: 5_000,
            max_queries_per_run: 1,
            stop_after_first_target_match: true,
            capture_results: true,
            force_refresh_today: false,
            navigation_timeout_ms: 45_000,
            worker_node_path: "node".into(),
            playwright_executable_path: None,
            user_agent: None,
            brightdata_zone: Some("serp_api1".into()),
            brightdata_api_base: None,
        };
        let result = build_success_result(&job, "sha256:test".into(), &parsed, &cfg, 0);
        assert_eq!(result.status, "ok");
        assert!(result.target_matches[0].found);
        assert_eq!(result.target_matches[0].position, Some(2));
    }
}
