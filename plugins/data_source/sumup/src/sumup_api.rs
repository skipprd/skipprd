use serde_json::{json, Value};
use skippr_plugin_shared_api_source::{OAuth2RefreshTokenAuth, RetryableHttpClient};
use tokio::time::{sleep, Duration};

pub const FIXTURE_ENV: &str = "SKIPPR_SUMUP_FIXTURE_DIR";
const API_BASE: &str = "https://api.sumup.com";
pub const DEFAULT_TOKEN_URL: &str = "https://api.sumup.com/token";

#[derive(Clone)]
pub struct SumUpApiClient {
    pub http: RetryableHttpClient,
    pub merchant_code: String,
    access_token: String,
    pub min_interval_ms: u64,
    fixture_dir: Option<String>,
}

impl SumUpApiClient {
    pub fn new(
        http: RetryableHttpClient,
        merchant_code: String,
        access_token: String,
        min_interval_ms: u64,
    ) -> Self {
        let fixture_dir = std::env::var(FIXTURE_ENV)
            .ok()
            .map(|dir| dir.trim().to_string())
            .filter(|dir| !dir.is_empty());
        Self::with_fixture_dir(
            http,
            merchant_code,
            access_token,
            min_interval_ms,
            fixture_dir,
        )
    }

    pub fn with_fixture_dir(
        http: RetryableHttpClient,
        merchant_code: String,
        access_token: String,
        min_interval_ms: u64,
        fixture_dir: Option<String>,
    ) -> Self {
        Self {
            http,
            merchant_code,
            access_token,
            min_interval_ms,
            fixture_dir: fixture_dir
                .map(|dir| dir.trim().to_string())
                .filter(|dir| !dir.is_empty()),
        }
    }

    pub async fn from_oauth(
        http: RetryableHttpClient,
        merchant_code: String,
        token_url: &str,
        client_id: &str,
        client_secret: &str,
        refresh_token: &str,
        min_interval_ms: u64,
    ) -> Result<Self, std::io::Error> {
        let oauth = OAuth2RefreshTokenAuth::new(token_url, client_id, client_secret, refresh_token);
        let access_token = oauth.refresh().await.map_err(std::io::Error::other)?;
        Ok(Self::new(
            http,
            merchant_code,
            access_token,
            min_interval_ms,
        ))
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.access_token)
    }

    fn fixture_path(dir: &str, name: &str) -> Option<Value> {
        let path = format!("{}/{}", dir.trim_end_matches('/'), name);
        let bytes = std::fs::read(&path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    async fn throttle(&self) {
        if self.min_interval_ms > 0 {
            sleep(Duration::from_millis(self.min_interval_ms)).await;
        }
    }

    fn retry_after_secs(response: &reqwest::Response) -> u64 {
        response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(1)
    }

    async fn sleep_before_retry(response: &reqwest::Response, attempt: usize) {
        let retry_after = Self::retry_after_secs(response);
        let backoff = 2_u64.saturating_pow(attempt as u32);
        sleep(Duration::from_secs(retry_after.max(backoff).min(30))).await;
    }

    async fn get_json(&self, url: &str, fixture: &str) -> Result<Value, std::io::Error> {
        if let Some(dir) = self.fixture_dir.as_deref() {
            if let Some(body) = Self::fixture_path(dir, fixture) {
                return Ok(body);
            }
            return Ok(json!({}));
        }
        for attempt in 0..3 {
            self.throttle().await;
            let response = self
                .http
                .client
                .get(url)
                .header("Authorization", self.auth_header())
                .send()
                .await
                .map_err(std::io::Error::other)?;
            let status = response.status();
            if attempt < 2 && (status.as_u16() == 429 || status.is_server_error()) {
                Self::sleep_before_retry(&response, attempt).await;
                continue;
            }
            return Self::parse_response(response).await;
        }
        unreachable!("bounded retry loop always returns")
    }

    async fn parse_response(response: reqwest::Response) -> Result<Value, std::io::Error> {
        let status = response.status();
        if status.as_u16() == 429 {
            if let Some(retry) = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
            {
                sleep(Duration::from_secs(retry)).await;
            }
        }
        let text = response.text().await.map_err(std::io::Error::other)?;
        if !status.is_success() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("SumUp HTTP {status}: {text}"),
            ));
        }
        serde_json::from_str(&text).map_err(std::io::Error::other)
    }

    fn extract_items(body: &Value) -> Vec<Value> {
        if let Some(items) = body.get("items").and_then(|v| v.as_array()) {
            return items.clone();
        }
        if let Some(arr) = body.as_array() {
            return arr.clone();
        }
        Vec::new()
    }

    pub async fn merchant(&self) -> Result<Value, std::io::Error> {
        let url = format!("{API_BASE}/v1/merchants/{}", self.merchant_code);
        self.get_json(&url, "merchant.json").await
    }

    pub async fn list_transactions(
        &self,
        oldest_time: Option<&str>,
        changes_since: Option<&str>,
    ) -> Result<Vec<Value>, std::io::Error> {
        if let Some(dir) = self.fixture_dir.as_deref() {
            if let Some(body) = Self::fixture_path(dir, "transactions.json") {
                return Ok(Self::extract_items(&body));
            }
            return Ok(Vec::new());
        }

        let mut all_rows = Vec::new();
        let mut oldest_ref: Option<String> = None;
        loop {
            self.throttle().await;
            let mut url = format!(
                "{API_BASE}/v2.1/merchants/{}/transactions/history?limit=100&order=ascending",
                self.merchant_code
            );
            if let Some(ts) = oldest_time {
                url.push_str(&format!("&oldest_time={ts}"));
            }
            if let Some(ts) = changes_since {
                url.push_str(&format!("&changes_since={ts}"));
            }
            if let Some(ref_token) = oldest_ref.as_deref() {
                url.push_str(&format!("&oldest_ref={ref_token}"));
            }

            let mut body: Option<Value> = None;
            for attempt in 0..3 {
                let response = self
                    .http
                    .client
                    .get(&url)
                    .header("Authorization", self.auth_header())
                    .send()
                    .await
                    .map_err(std::io::Error::other)?;
                let status = response.status();
                if attempt < 2 && (status.as_u16() == 429 || status.is_server_error()) {
                    Self::sleep_before_retry(&response, attempt).await;
                    continue;
                }
                body = Some(Self::parse_response(response).await?);
                break;
            }
            let body = body.expect("bounded retry loop always sets body");
            let page = Self::extract_items(&body);
            let page_len = page.len();
            if let Some(last) = page.last().and_then(|row| {
                row.get("transaction_id")
                    .or_else(|| row.get("id"))
                    .and_then(|v| v.as_str())
            }) {
                oldest_ref = Some(last.to_string());
            }
            all_rows.extend(page);
            if page_len < 100 {
                break;
            }
        }
        Ok(all_rows)
    }

    pub async fn list_payouts(
        &self,
        start_date: &str,
        end_date: &str,
    ) -> Result<Vec<Value>, std::io::Error> {
        if let Some(dir) = self.fixture_dir.as_deref() {
            if let Some(body) = Self::fixture_path(dir, "payouts.json") {
                return Ok(Self::extract_items(&body));
            }
            return Ok(Vec::new());
        }

        let url = format!(
            "{API_BASE}/v1/merchants/{}/payouts?start_date={start_date}&end_date={end_date}&format=json",
            self.merchant_code
        );
        let body = self.get_json(&url, "payouts.json").await?;
        Ok(Self::extract_items(&body))
    }

    pub async fn list_checkouts(&self) -> Result<Vec<Value>, std::io::Error> {
        if let Some(dir) = self.fixture_dir.as_deref() {
            if let Some(body) = Self::fixture_path(dir, "checkouts.json") {
                return Ok(Self::extract_items(&body));
            }
            return Ok(Vec::new());
        }

        let url = format!("{API_BASE}/v0.1/checkouts");
        let body = self.get_json(&url, "checkouts.json").await?;
        Ok(Self::extract_items(&body))
    }
}

pub fn sumup_str(obj: &Value, key: &str) -> Option<String> {
    obj.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

pub fn sumup_f64(obj: &Value, key: &str) -> Option<f64> {
    obj.get(key).and_then(|v| v.as_f64())
}

pub fn sumup_i64(obj: &Value, key: &str) -> Option<i64> {
    obj.get(key).and_then(|v| v.as_i64())
}

pub fn record_id(obj: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| sumup_str(obj, key))
        .or_else(|| obj.get("id").and_then(|v| v.as_str()).map(str::to_string))
}

pub fn map_merchant(
    ingest_run_date: &str,
    run_date: &str,
    merchant_code: &str,
    obj: &Value,
) -> Value {
    json!({
        "ingest_run_date": ingest_run_date,
        "run_date": run_date,
        "merchant_code": merchant_code,
        "country": sumup_str(obj, "country"),
        "currency": sumup_str(obj, "currency"),
        "business_type": sumup_str(obj, "business_type"),
        "merchant_profile_id": sumup_str(obj, "merchant_profile_id"),
        "activation_status": sumup_str(obj, "activation_status"),
        "locale": sumup_str(obj, "locale"),
        "timezone": sumup_str(obj, "timezone"),
    })
}

pub fn map_transaction(ingest_run_date: &str, merchant_code: &str, obj: &Value) -> Value {
    json!({
        "ingest_run_date": ingest_run_date,
        "merchant_code": merchant_code,
        "transaction_id": record_id(obj, &["transaction_id", "id"]),
        "transaction_code": sumup_str(obj, "transaction_code"),
        "amount": sumup_f64(obj, "amount"),
        "currency": sumup_str(obj, "currency"),
        "status": sumup_str(obj, "status"),
        "payment_type": sumup_str(obj, "payment_type"),
        "type": sumup_str(obj, "type"),
        "timestamp": sumup_str(obj, "timestamp"),
        "payout_date": sumup_str(obj, "payout_date"),
        "payout_type": sumup_str(obj, "payout_type"),
        "refunded_amount": sumup_f64(obj, "refunded_amount"),
        "installments_count": sumup_i64(obj, "installments_count"),
    })
}

pub fn map_payout(ingest_run_date: &str, merchant_code: &str, obj: &Value) -> Value {
    json!({
        "ingest_run_date": ingest_run_date,
        "merchant_code": merchant_code,
        "payout_id": record_id(obj, &["id", "payout_id"]),
        "amount": sumup_f64(obj, "amount"),
        "currency": sumup_str(obj, "currency"),
        "date": sumup_str(obj, "date"),
        "fee": sumup_f64(obj, "fee"),
        "status": sumup_str(obj, "status"),
        "type": sumup_str(obj, "type"),
        "transaction_code": sumup_str(obj, "transaction_code"),
    })
}

pub fn map_checkout(
    ingest_run_date: &str,
    run_date: &str,
    merchant_code: &str,
    obj: &Value,
) -> Value {
    json!({
        "ingest_run_date": ingest_run_date,
        "run_date": run_date,
        "merchant_code": merchant_code,
        "checkout_id": record_id(obj, &["id", "checkout_id"]),
        "amount": sumup_f64(obj, "amount"),
        "currency": sumup_str(obj, "currency"),
        "status": sumup_str(obj, "status"),
        "date": sumup_str(obj, "date"),
        "transaction_id": sumup_str(obj, "transaction_id"),
        "transaction_code": sumup_str(obj, "transaction_code"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use skippr_plugin_shared_api_source::RetryableHttpClient;
    use std::sync::{LazyLock, Mutex};

    static ENV_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    #[tokio::test]
    async fn fixture_mode_missing_list_file_returns_empty_rows() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!(
            "skippr_sumup_fixture_missing_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let client = SumUpApiClient::with_fixture_dir(
            RetryableHttpClient::new(skippr_plugin_shared_api_source::RetryConfig::default()),
            "MH4H92C7".to_string(),
            "fixture".to_string(),
            0,
            Some(dir.to_string_lossy().to_string()),
        );

        let rows = client.list_transactions(None, None).await.unwrap();
        assert!(rows.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
