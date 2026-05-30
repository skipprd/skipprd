use serde_json::{json, Value};
use skippr_plugin_shared_api_source::{BasicAuth, RetryConfig, RetryableHttpClient};
use tracing::warn;

pub const BACKLINKS_LIVE_URL: &str = "https://api.dataforseo.com/v3/backlinks/backlinks/live";
pub const PAGE_INTERSECTION_LIVE_URL: &str =
    "https://api.dataforseo.com/v3/backlinks/page_intersection/live";

pub const TASK_OK_STATUS: i64 = 20_000;

#[derive(Debug, Clone)]
pub struct ParsedTaskResponse {
    pub task_cost: f64,
    pub items: Vec<Value>,
    pub items_count: u32,
    pub total_count: Option<u64>,
    pub search_after_token: Option<String>,
    pub task_status_code: i64,
    pub task_status_message: String,
    pub task_ok: bool,
}

#[derive(Debug, Clone)]
pub struct LiveApiResponse {
    pub top_level_cost: f64,
    pub tasks: Vec<ParsedTaskResponse>,
}

#[derive(Clone)]
pub struct DataForSeoClient {
    http: RetryableHttpClient,
    auth: BasicAuth,
    request_interval_ms: u64,
    fixture_dir: Option<String>,
}

impl DataForSeoClient {
    pub fn new(
        login: String,
        password: String,
        max_api_retries: u32,
        request_interval_ms: u64,
    ) -> Self {
        let mut retry = RetryConfig::default();
        retry.max_attempts = max_api_retries.max(1);
        let fixture_dir = std::env::var("SKIPPR_DATAFORSEO_BACKLINKS_FIXTURE_DIR")
            .ok()
            .filter(|d| !d.trim().is_empty());
        Self {
            http: RetryableHttpClient::new(retry),
            auth: BasicAuth::new(login, password),
            request_interval_ms,
            fixture_dir,
        }
    }

    pub fn auth_header(&self) -> String {
        self.auth.authorization_header_value()
    }

    pub async fn post_backlinks_live(
        &self,
        tasks: Vec<Value>,
        fixture_name: &str,
    ) -> Result<LiveApiResponse, std::io::Error> {
        self.post_live(BACKLINKS_LIVE_URL, tasks, fixture_name).await
    }

    pub async fn post_page_intersection_live(
        &self,
        tasks: Vec<Value>,
        fixture_name: &str,
    ) -> Result<LiveApiResponse, std::io::Error> {
        self.post_live(PAGE_INTERSECTION_LIVE_URL, tasks, fixture_name)
            .await
    }

    /// Minimal credential probe (`limit: 1`) for `skippr doctor`.
    pub async fn probe_credentials(login: &str, password: &str) -> Result<(), String> {
        let client = DataForSeoClient::new(
            login.to_string(),
            password.to_string(),
            3,
            0,
        );
        if client.fixture_dir.is_some() {
            return Ok(());
        }
        let body = json!([{
            "target": "example.com",
            "limit": 1,
            "offset": 0
        }]);
        let response = client
            .post_backlinks_live(vec![body], "probe")
            .await
            .map_err(|e| e.to_string())?;
        if response.tasks.iter().any(|t| t.task_ok) {
            return Ok(());
        }
        let msg = response
            .tasks
            .first()
            .map(|t| t.task_status_message.clone())
            .unwrap_or_else(|| "DataForSEO probe failed".into());
        Err(msg)
    }

    async fn post_live(
        &self,
        url: &str,
        tasks: Vec<Value>,
        fixture_name: &str,
    ) -> Result<LiveApiResponse, std::io::Error> {
        if let Some(dir) = &self.fixture_dir {
            return load_fixture(dir, fixture_name);
        }

        if self.request_interval_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(self.request_interval_ms)).await;
        }

        let body = Value::Array(tasks);
        let mut attempt = 0u32;
        loop {
            let response = self
                .http
                .client
                .post(url)
                .header("Authorization", self.auth_header())
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;

            let status = response.status();
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());

            match RetryableHttpClient::classify_status(status, retry_after) {
                skippr_plugin_shared_api_source::RetryDecision::Success => {
                    let json: Value = response
                        .json()
                        .await
                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                    return parse_live_response(&json);
                }
                skippr_plugin_shared_api_source::RetryDecision::RetryAfter(delay) => {
                    attempt += 1;
                    if attempt >= self.http.config.max_attempts {
                        return Err(std::io::Error::other(format!(
                            "DataForSEO API failed after {attempt} attempts: HTTP {status}"
                        )));
                    }
                    warn!(attempt, %status, "DataForSEO API retry");
                    self.http.backoff(attempt, delay).await;
                }
                skippr_plugin_shared_api_source::RetryDecision::GiveUp => {
                    let text = response.text().await.unwrap_or_default();
                    if status == reqwest::StatusCode::UNAUTHORIZED {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            format!("DataForSEO authentication failed: {text}"),
                        ));
                    }
                    return Err(std::io::Error::other(format!(
                        "DataForSEO API HTTP {status}: {text}"
                    )));
                }
            }
        }
    }
}

pub fn parse_live_response(body: &Value) -> Result<LiveApiResponse, std::io::Error> {
    let top_level_cost = body
        .get("cost")
        .and_then(json_f64)
        .unwrap_or(0.0);
    let tasks = body
        .get("tasks")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let parsed = tasks
        .iter()
        .map(parse_task)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(LiveApiResponse {
        top_level_cost,
        tasks: parsed,
    })
}

fn parse_task(task: &Value) -> Result<ParsedTaskResponse, std::io::Error> {
    let task_status_code = task
        .get("status_code")
        .and_then(json_i64)
        .unwrap_or(0);
    let task_ok = task_status_code == TASK_OK_STATUS;
    let task_status_message = task
        .get("status_message")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let task_cost = task.get("cost").and_then(json_f64).unwrap_or(0.0);

    let result0 = task
        .get("result")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first());

    let (items, items_count, total_count, search_after_token) = if let Some(result) = result0 {
        let items = result
            .get("items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let items_count = result
            .get("items_count")
            .and_then(json_u32)
            .unwrap_or(items.len() as u32);
        let total_count = result.get("total_count").and_then(json_u64);
        let search_after_token = result
            .get("search_after_token")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        (items, items_count, total_count, search_after_token)
    } else {
        (Vec::new(), 0, None, None)
    };

    Ok(ParsedTaskResponse {
        task_cost,
        items,
        items_count,
        total_count,
        search_after_token,
        task_status_code,
        task_status_message,
        task_ok,
    })
}

fn load_fixture(dir: &str, name: &str) -> Result<LiveApiResponse, std::io::Error> {
    let base = dir.trim_end_matches('/');
    let path = format!("{base}/{name}.json");
    let bytes = std::fs::read(&path).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("fixture {path}: {e}"),
        )
    })?;
    let json: Value = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
    parse_live_response(&json)
}

fn json_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}

fn json_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().map(|n| n as i64))
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}

fn json_u32(value: &Value) -> Option<u32> {
    value
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .or_else(|| value.as_i64().and_then(|n| u32::try_from(n).ok()))
}

fn json_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|n| u64::try_from(n).ok()))
}
