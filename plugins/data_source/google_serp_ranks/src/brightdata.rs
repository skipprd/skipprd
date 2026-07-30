use std::time::Duration;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use skippr_plugin_shared_api_source::{
    body_debug_suffix, log_api_response_issue, truncate_response_body,
};
use tracing::{info, warn};
use url::Url;

use crate::config::{DataSourceGoogleSerpRanksPluginConfig, SerpDevice};
use crate::domain::{domain_matches_target, normalize_domain};
use crate::worker::{
    OrganicResultRow, SerpFeatureFlags, TargetMatchRow, WorkerJobRequest, WorkerJobResult,
};

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
            let search_url = build_search_url(
                &job.keyword,
                &job.country,
                &job.language,
                device,
                page_start,
            );
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

            let body = Self::serp_api_request_body(&self.zone, &search_url, true);
            let endpoint = format!("{}/request", self.api_base.trim_end_matches('/'));
            let fetch = self
                .fetch_parsed_with_soft_retry(&job.keyword, &endpoint, &body, page_start)
                .await?;

            let (status, raw_text, parsed) = match fetch {
                SoftFetchOutcome::Parsed {
                    status,
                    raw_text,
                    parsed,
                } => (status, raw_text, parsed),
                SoftFetchOutcome::HttpError { status, raw_text } => {
                    return Ok(error_result(
                        job,
                        first_hash.unwrap_or(search_url_hash),
                        "BRIGHTDATA_HTTP_ERROR",
                        format!(
                            "Bright Data HTTP {}: {} ({})",
                            status,
                            truncate_response_body(&raw_text, 500),
                            body_debug_suffix(&raw_text, 300)
                        ),
                    ));
                }
                SoftFetchOutcome::SoftError { status, raw_text } => {
                    // Prefer partial pages already fetched over aborting the whole keyword/job.
                    if merged.is_some() {
                        warn!(
                            keyword = %job.keyword,
                            page_start,
                            http_status = status.map(|s| s.as_u16()),
                            body_preview = %truncate_response_body(&raw_text, 120),
                            "Bright Data soft error on later page; keeping earlier pages"
                        );
                        break;
                    }
                    return Ok(error_result(
                        job,
                        first_hash.unwrap_or(search_url_hash),
                        "BRIGHTDATA_SOFT_ERROR",
                        format!(
                            "Bright Data soft error after retries (status={:?}, {})",
                            status.map(|s| s.as_u16()),
                            body_debug_suffix(&raw_text, 300)
                        ),
                    ));
                }
            };
            info!(
                keyword = %job.keyword,
                zone = %self.zone,
                page_start,
                http_status = status.as_u16(),
                body_len = raw_text.len(),
                organic_count = organic_entries(&parsed).len(),
                "Bright Data SERP: received response"
            );
            if let Some(reason) = detect_blocked(&parsed, &raw_text) {
                return Ok(blocked_result(
                    job,
                    first_hash.unwrap_or(search_url_hash),
                    reason,
                    &parsed,
                    pages_fetched + 1,
                ));
            }

            let page_result =
                build_success_result(job, search_url_hash, &parsed, &self.config, page_start);
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

        Ok(merged.unwrap_or_else(|| {
            error_result(
                job,
                first_hash.unwrap_or_else(|| "sha256:empty".into()),
                "BRIGHTDATA_EMPTY",
                "No SERP pages fetched".into(),
            )
        }))
    }

    fn serp_api_request_body(zone: &str, search_url: &str, parsed_light: bool) -> Value {
        if parsed_light {
            serde_json::json!({
                "zone": zone,
                "url": search_url,
                "format": "raw",
                "data_format": "parsed_light"
            })
        } else {
            // Full JSON (`general.results_cnt`) — URL already carries `brd_json=1`.
            serde_json::json!({
                "zone": zone,
                "url": search_url,
                "format": "raw",
            })
        }
    }

    pub async fn throttle_delay(&self) {
        tokio::time::sleep(Duration::from_millis(self.config.min_query_interval_ms)).await;
    }

    /// Fetch `allintitle:{keyword}` and return Google's reported result count.
    pub async fn fetch_allintitle_count(
        &self,
        keyword: &str,
    ) -> Result<Option<u64>, std::io::Error> {
        let device = if self.config.device == SerpDevice::Mobile {
            SerpDevice::Mobile
        } else {
            SerpDevice::Desktop
        };
        let query = format!("allintitle:{keyword}");
        let search_url = build_search_url(
            &query,
            &self.config.country,
            &self.config.language,
            device,
            0,
        );
        info!(
            keyword = %keyword,
            zone = %self.zone,
            search_url = %search_url,
            "Bright Data allintitle: requesting results count"
        );

        let body = Self::serp_api_request_body(&self.zone, &search_url, false);
        let endpoint = format!("{}/request", self.api_base.trim_end_matches('/'));
        let fetch = self
            .fetch_parsed_with_soft_retry(keyword, &endpoint, &body, 0)
            .await?;
        match fetch {
            SoftFetchOutcome::Parsed {
                raw_text, parsed, ..
            } => {
                if let Some(reason) = detect_blocked(&parsed, &raw_text) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("Bright Data allintitle blocked: {reason}"),
                    ));
                }
                Ok(parse_results_cnt(&parsed))
            }
            SoftFetchOutcome::HttpError { status, raw_text } => Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!(
                    "Bright Data HTTP {}: {} ({})",
                    status,
                    truncate_response_body(&raw_text, 500),
                    body_debug_suffix(&raw_text, 300)
                ),
            )),
            SoftFetchOutcome::SoftError { status, raw_text } => Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!(
                    "Bright Data soft error after retries (status={:?}, {})",
                    status.map(|s| s.as_u16()),
                    body_debug_suffix(&raw_text, 300)
                ),
            )),
        }
    }

    async fn fetch_parsed_with_soft_retry(
        &self,
        keyword: &str,
        endpoint: &str,
        body: &Value,
        page_start: u32,
    ) -> Result<SoftFetchOutcome, std::io::Error> {
        const MAX_ATTEMPTS: u32 = 3;
        let mut last_status: Option<reqwest::StatusCode> = None;
        let mut last_body = String::new();

        for attempt in 1..=MAX_ATTEMPTS {
            let (status, raw_text, brd_err_code, brd_err_msg) =
                self.post_serp_request_with_headers(body).await?;
            last_status = Some(status);
            last_body = raw_text.clone();

            if !status.is_success() {
                log_api_response_issue(
                    "bright_data",
                    endpoint,
                    "http_error",
                    Some(status.as_u16()),
                    &raw_text,
                );
                return Ok(SoftFetchOutcome::HttpError { status, raw_text });
            }

            if is_brightdata_soft_error_body(&raw_text) {
                log_api_response_issue(
                    "bright_data",
                    endpoint,
                    if raw_text.trim().is_empty() {
                        "empty"
                    } else {
                        "soft_error_body"
                    },
                    Some(status.as_u16()),
                    &raw_text,
                );
                warn!(
                    keyword = %keyword,
                    zone = %self.zone,
                    page_start,
                    endpoint = %endpoint,
                    attempt,
                    http_status = status.as_u16(),
                    brd_err_code = brd_err_code.as_deref().unwrap_or(""),
                    brd_err_msg = brd_err_msg.as_deref().unwrap_or(""),
                    body_preview = %truncate_response_body(&raw_text, 120),
                    "Bright Data soft error body; retrying"
                );
                if attempt < MAX_ATTEMPTS {
                    let backoff_ms = 750u64.saturating_mul(attempt as u64);
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    continue;
                }
                return Ok(SoftFetchOutcome::SoftError {
                    status: Some(status),
                    raw_text,
                });
            }

            match parse_response_payload(endpoint, &raw_text) {
                Ok(parsed) => {
                    return Ok(SoftFetchOutcome::Parsed {
                        status,
                        raw_text,
                        parsed,
                    });
                }
                Err(_) => {
                    // Non-JSON that didn't match soft-error heuristics — still retry once/twice.
                    warn!(
                        keyword = %keyword,
                        page_start,
                        attempt,
                        http_status = status.as_u16(),
                        body_preview = %truncate_response_body(&raw_text, 120),
                        "Bright Data invalid JSON; retrying"
                    );
                    if attempt < MAX_ATTEMPTS {
                        let backoff_ms = 750u64.saturating_mul(attempt as u64);
                        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                        continue;
                    }
                    return Ok(SoftFetchOutcome::SoftError {
                        status: Some(status),
                        raw_text,
                    });
                }
            }
        }

        Ok(SoftFetchOutcome::SoftError {
            status: last_status,
            raw_text: last_body,
        })
    }

    async fn post_serp_request_with_headers(
        &self,
        body: &Value,
    ) -> Result<(reqwest::StatusCode, String, Option<String>, Option<String>), std::io::Error> {
        let endpoint = format!("{}/request", self.api_base.trim_end_matches('/'));
        let response = self
            .http
            .post(&endpoint)
            .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
            .header(CONTENT_TYPE, "application/json")
            .json(body)
            .send()
            .await
            .map_err(std::io::Error::other)?;
        let status = response.status();
        let brd_err_code = response
            .headers()
            .get("x-brd-err-code")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let brd_err_msg = response
            .headers()
            .get("x-brd-err-msg")
            .or_else(|| response.headers().get("x-brd-error"))
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let raw_text = response.text().await.map_err(std::io::Error::other)?;
        Ok((status, raw_text, brd_err_code, brd_err_msg))
    }
}

enum SoftFetchOutcome {
    Parsed {
        status: reqwest::StatusCode,
        raw_text: String,
        parsed: Value,
    },
    HttpError {
        status: reqwest::StatusCode,
        raw_text: String,
    },
    SoftError {
        status: Option<reqwest::StatusCode>,
        raw_text: String,
    },
}

/// Bright Data sometimes returns HTTP 200 with an empty body or a plain-text
/// soft error (e.g. "Error while processing request") instead of JSON.
pub fn is_brightdata_soft_error_body(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with('{') || lower.starts_with('[') {
        return false;
    }
    lower.contains("error while processing request")
        || lower.contains("unexpected error")
        || lower == "error"
        || (!trimmed.starts_with('<') && serde_json::from_str::<Value>(trimmed).is_err())
}

/// Parse Bright Data `general.results_cnt` (allintitle total).
pub fn parse_results_cnt(parsed: &Value) -> Option<u64> {
    parsed
        .get("general")
        .and_then(|g| g.get("results_cnt"))
        .and_then(json_u64)
}

fn json_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|n| u64::try_from(n).ok()))
        .or_else(|| value.as_f64().map(|n| n as u64))
}

fn parse_response_payload(endpoint: &str, raw: &str) -> Result<Value, std::io::Error> {
    if raw.trim().is_empty() {
        log_api_response_issue("bright_data", endpoint, "empty", None, raw);
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "Bright Data response is empty ({})",
                body_debug_suffix(raw, 300)
            ),
        ));
    }
    let value: Value = serde_json::from_str(raw).map_err(|e| {
        log_api_response_issue("bright_data", endpoint, "invalid_json", None, raw);
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "Bright Data response is not JSON: {e} ({})",
                body_debug_suffix(raw, 300)
            ),
        )
    })?;
    if let Some(inner) = value.get("body").and_then(|b| b.as_str()) {
        if let Ok(parsed) = serde_json::from_str::<Value>(inner) {
            return Ok(parsed);
        }
    }
    Ok(value)
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
    merged.results_inspected = merged
        .results_inspected
        .saturating_add(page.results_inspected);
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
    merged.serp_features = Some(merge_serp_features(
        merged
            .serp_features
            .as_ref()
            .unwrap_or(&SerpFeatureFlags::default()),
        page.serp_features
            .as_ref()
            .unwrap_or(&SerpFeatureFlags::default()),
    ));
    merged
}

fn merge_serp_features(a: &SerpFeatureFlags, b: &SerpFeatureFlags) -> SerpFeatureFlags {
    SerpFeatureFlags {
        has_ai_overview: a.has_ai_overview || b.has_ai_overview,
        has_paa: a.has_paa || b.has_paa,
        has_video: a.has_video || b.has_video,
        has_sitelinks: a.has_sitelinks || b.has_sitelinks,
        has_featured_snippet: a.has_featured_snippet || b.has_featured_snippet,
        owns_featured_snippet: a.owns_featured_snippet || b.owns_featured_snippet,
        has_local_pack: a.has_local_pack || b.has_local_pack,
        has_shopping: a.has_shopping || b.has_shopping,
        has_images: a.has_images || b.has_images,
        has_knowledge_graph: a.has_knowledge_graph || b.has_knowledge_graph,
        has_answer_box: a.has_answer_box || b.has_answer_box,
        has_related_searches: a.has_related_searches || b.has_related_searches,
    }
}

fn value_has_content(value: &Value) -> bool {
    match value {
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
        Value::String(text) => !text.trim().is_empty(),
        Value::Bool(flag) => *flag,
        Value::Number(_) => true,
        _ => false,
    }
}

fn parsed_has_keys(parsed: &Value, keys: &[&str]) -> bool {
    keys.iter()
        .any(|key| parsed.get(*key).map(value_has_content).unwrap_or(false))
}

fn urls_from_value(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) if text.contains("://") || text.contains('.') => {
            out.push(text.clone());
        }
        Value::Array(items) => {
            for item in items {
                urls_from_value(item, out);
            }
        }
        Value::Object(map) => {
            for key in ["link", "url", "href", "displayed_link"] {
                if let Some(url) = map.get(key).and_then(|v| v.as_str()) {
                    out.push(url.to_string());
                }
            }
            for child in map.values() {
                urls_from_value(child, out);
            }
        }
        _ => {}
    }
}

fn owns_featured_snippet(parsed: &Value, targets: &[String]) -> bool {
    let featured_keys = ["featured_snippet", "answer_box", "instant_answer"];
    for key in featured_keys {
        let Some(block) = parsed.get(key) else {
            continue;
        };
        let mut urls = Vec::new();
        urls_from_value(block, &mut urls);
        for url in urls {
            let domain = normalize_domain(&url);
            if domain.is_empty() {
                continue;
            }
            if targets
                .iter()
                .any(|target| domain_matches_target(&domain, target))
            {
                return true;
            }
        }
    }
    false
}

pub fn extract_serp_features(parsed: &Value, targets: &[String]) -> SerpFeatureFlags {
    let has_featured_snippet = parsed_has_keys(
        parsed,
        &["featured_snippet", "answer_box", "instant_answer"],
    );
    SerpFeatureFlags {
        has_ai_overview: parsed_has_keys(
            parsed,
            &["ai_overview", "ai_overviews", "generative_ai", "sge"],
        ),
        has_paa: parsed_has_keys(parsed, &["people_also_ask", "related_questions", "paa"]),
        has_video: parsed_has_keys(
            parsed,
            &["videos", "video", "video_results", "inline_videos"],
        ),
        has_sitelinks: parsed_has_keys(
            parsed,
            &["sitelinks", "inline_sitelinks", "expanded_sitelinks"],
        ),
        has_featured_snippet,
        owns_featured_snippet: has_featured_snippet && owns_featured_snippet(parsed, targets),
        has_local_pack: parsed_has_keys(
            parsed,
            &["local_pack", "local_results", "local_results_map", "maps"],
        ),
        has_shopping: parsed_has_keys(
            parsed,
            &[
                "shopping",
                "shopping_results",
                "ads_shopping",
                "popular_products",
            ],
        ),
        has_images: parsed_has_keys(parsed, &["images", "image_results", "inline_images"]),
        has_knowledge_graph: parsed_has_keys(
            parsed,
            &[
                "knowledge_graph",
                "knowledge",
                "knowledge_panel",
                "knowledge_card",
            ],
        ),
        has_answer_box: parsed_has_keys(parsed, &["answer_box", "instant_answer"]),
        has_related_searches: parsed_has_keys(
            parsed,
            &[
                "related_searches",
                "related_searches_list",
                "related_queries",
            ],
        ),
    }
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
    let serp_features = extract_serp_features(parsed, &targets);

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
        serp_features: Some(serp_features),
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
        serp_features: Some(extract_serp_features(parsed, &job.targets)),
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
        serp_features: None,
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
    fn soft_error_body_detects_empty_and_plain_text() {
        assert!(is_brightdata_soft_error_body(""));
        assert!(is_brightdata_soft_error_body("   "));
        assert!(is_brightdata_soft_error_body(
            "Error while processing request"
        ));
        assert!(is_brightdata_soft_error_body("Unexpected error. The server encountered an unexpected error while processing the request."));
        assert!(!is_brightdata_soft_error_body(r#"{"organic":[]}"#));
        assert!(!is_brightdata_soft_error_body(r#"[{"link":"https://x"}]"#));
    }

    #[test]
    fn parse_response_payload_rejects_empty_body() {
        let err = parse_response_payload("https://api.brightdata.com/request", "").unwrap_err();
        assert!(err.to_string().contains("body_len=0"));
    }

    #[test]
    fn parse_response_payload_includes_preview_on_invalid_json() {
        let err =
            parse_response_payload("https://api.brightdata.com/request", "not-json").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("body_len="));
        assert!(message.contains("preview=not-json"));
    }

    #[test]
    fn parse_results_cnt_from_fixture() {
        let raw = include_str!("../fixtures/allintitle_brightdata.json");
        let parsed: Value = serde_json::from_str(raw).unwrap();
        assert_eq!(parse_results_cnt(&parsed), Some(42));
    }

    #[test]
    fn parse_results_cnt_missing_general() {
        let parsed: Value = serde_json::from_str(r#"{"organic":[]}"#).unwrap();
        assert_eq!(parse_results_cnt(&parsed), None);
    }

    #[test]
    fn parsed_light_body_omits_results_cnt_field() {
        let body = BrightDataClient::serp_api_request_body(
            "serp_api1",
            "https://www.google.com/search?q=allintitle%3Apizza&brd_json=1",
            true,
        );
        assert_eq!(body["data_format"], "parsed_light");
        let light: Value = serde_json::from_str(r#"{"organic":[]}"#).unwrap();
        assert_eq!(parse_results_cnt(&light), None);
    }

    #[test]
    fn allintitle_body_uses_full_json_not_parsed_light() {
        let body = BrightDataClient::serp_api_request_body(
            "serp_api1",
            "https://www.google.com/search?q=allintitle%3Apizza&brd_json=1",
            false,
        );
        assert!(body.get("data_format").is_none());
    }

    #[test]
    fn build_search_url_includes_gl_and_brd_json() {
        let url = build_search_url("pizza", "us", "en", SerpDevice::Desktop, 0);
        assert!(url.contains("q=pizza"));
        assert!(url.contains("gl=us"));
        assert!(url.contains("brd_json=1"));
    }

    #[test]
    fn build_search_url_mobile_includes_brd_mobile() {
        let url = build_search_url("pizza", "us", "en", SerpDevice::Mobile, 0);
        assert!(url.contains("brd_mobile=1"));
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
        let parsed = parse_response_payload("https://api.brightdata.com/request", raw).unwrap();
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
            include_allintitle: false,
            allintitle_keywords: vec![],
            allintitle_only: false,
            max_allintitle_queries_per_run: None,
        };
        let result = build_success_result(&job, "sha256:test".into(), &parsed, &cfg, 0);
        assert_eq!(result.status, "ok");
        assert!(result.target_matches[0].found);
        assert_eq!(result.target_matches[0].position, Some(2));
        assert!(!result.serp_features.as_ref().unwrap().has_featured_snippet);
    }

    #[test]
    fn extract_serp_features_from_parsed_light_blocks() {
        let parsed: Value = serde_json::from_str(
            r#"{
              "organic":[{"link":"https://www.example.com/","title":"Ex","global_rank":2}],
              "people_also_ask":[{"question":"what is example"}],
              "videos":[{"link":"https://www.youtube.com/watch?v=abc"}],
              "featured_snippet":{"link":"https://www.example.com/snippet"}
            }"#,
        )
        .unwrap();
        let features = extract_serp_features(&parsed, &["example.com".into()]);
        assert!(features.has_paa);
        assert!(features.has_video);
        assert!(features.has_featured_snippet);
        assert!(features.owns_featured_snippet);
    }

    #[test]
    fn extract_serp_features_from_brightdata_feature_fixture() {
        let parsed: Value = serde_json::from_str(
            r#"{
              "ai_overviews":[{"text":"AI summary"}],
              "related_questions":[{"question":"what is example"}],
              "inline_videos":[{"link":"https://www.youtube.com/watch?v=abc"}],
              "expanded_sitelinks":[{"link":"https://www.example.com/features"}],
              "featured_snippet":{"link":"https://www.example.com/snippet"},
              "local_results_map":[{"title":"Example HQ"}],
              "popular_products":[{"title":"Example plan"}],
              "inline_images":[{"image":"https://images.example.com/a.jpg"}],
              "knowledge_card":{"title":"Example"},
              "answer_box":{"answer":"42"},
              "related_queries":["example pricing"]
            }"#,
        )
        .unwrap();
        let features = extract_serp_features(&parsed, &["example.com".into()]);
        assert!(features.has_ai_overview);
        assert!(features.has_paa);
        assert!(features.has_video);
        assert!(features.has_sitelinks);
        assert!(features.has_featured_snippet);
        assert!(features.owns_featured_snippet);
        assert!(features.has_local_pack);
        assert!(features.has_shopping);
        assert!(features.has_images);
        assert!(features.has_knowledge_graph);
        assert!(features.has_answer_box);
        assert!(features.has_related_searches);
    }

    #[test]
    fn extract_serp_features_empty_payload_all_false() {
        let parsed: Value = serde_json::from_str(r#"{"organic":[]}"#).unwrap();
        let features = extract_serp_features(&parsed, &["example.com".into()]);
        assert!(!features.has_ai_overview);
        assert!(!features.has_paa);
        assert!(!features.has_featured_snippet);
        assert!(!features.owns_featured_snippet);
    }

    #[test]
    fn competitor_owns_featured_snippet_not_target() {
        let parsed: Value =
            serde_json::from_str(r#"{"featured_snippet":{"link":"https://www.rival.com/page"}}"#)
                .unwrap();
        let features = extract_serp_features(&parsed, &["example.com".into()]);
        assert!(features.has_featured_snippet);
        assert!(!features.owns_featured_snippet);
    }

    #[test]
    fn error_result_emits_no_serp_features() {
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
        let result = error_result(&job, "sha256:err".into(), "fetch_failed", "timeout".into());
        assert_eq!(result.status, "error");
        assert!(result.serp_features.is_none());
    }
}
