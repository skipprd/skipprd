use chrono::{NaiveDate, Utc};
use serde_json::{json, Value};
use skippr_plugin_shared_api_source::RetryableHttpClient;
use tokio::time::{sleep, Duration};

use crate::jwt_auth::RevolutJwtAuth;

pub const FIXTURE_ENV: &str = "SKIPPR_REVOLUT_FIXTURE_DIR";
pub const DEFAULT_API_BASE: &str = "https://b2b.revolut.com/api/1.0";
const MAX_TX_PAGE_SIZE: u32 = 1000;

#[derive(Clone)]
pub struct RevolutApiClient {
    pub http: RetryableHttpClient,
    api_base: String,
    access_token: String,
    pub min_interval_ms: u64,
    fixture_dir: Option<String>,
    jwt_auth: Option<RevolutJwtAuth>,
}

impl RevolutApiClient {
    pub fn new(
        http: RetryableHttpClient,
        api_base: String,
        access_token: String,
        min_interval_ms: u64,
    ) -> Self {
        let fixture_dir = std::env::var(FIXTURE_ENV)
            .ok()
            .map(|dir| dir.trim().to_string())
            .filter(|dir| !dir.is_empty());
        Self::with_fixture_dir(
            http,
            api_base,
            access_token,
            min_interval_ms,
            fixture_dir,
            None,
        )
    }

    pub fn with_fixture_dir(
        http: RetryableHttpClient,
        api_base: String,
        access_token: String,
        min_interval_ms: u64,
        fixture_dir: Option<String>,
        jwt_auth: Option<RevolutJwtAuth>,
    ) -> Self {
        Self {
            http,
            api_base: api_base.trim_end_matches('/').to_string(),
            access_token,
            min_interval_ms,
            fixture_dir: fixture_dir
                .map(|dir| dir.trim().to_string())
                .filter(|dir| !dir.is_empty()),
            jwt_auth,
        }
    }

    pub async fn from_refresh_token(
        http: RetryableHttpClient,
        api_base: String,
        client_id: &str,
        private_key_pem: &str,
        issuer_domain: &str,
        refresh_token: &str,
        min_interval_ms: u64,
    ) -> Result<Self, std::io::Error> {
        let token_url = format!("{}/auth/token", api_base.trim_end_matches('/'));
        let jwt_auth = RevolutJwtAuth::new(
            token_url,
            client_id,
            private_key_pem,
            issuer_domain,
            refresh_token,
        );
        let access_token = jwt_auth
            .access_token()
            .await
            .map_err(std::io::Error::other)?;
        Ok(Self::with_fixture_dir(
            http,
            api_base,
            access_token,
            min_interval_ms,
            None,
            Some(jwt_auth),
        ))
    }

    async fn bearer_token(&self) -> Result<String, std::io::Error> {
        if let Some(auth) = &self.jwt_auth {
            return auth.access_token().await.map_err(std::io::Error::other);
        }
        Ok(self.access_token.clone())
    }

    fn auth_header(&self, token: &str) -> String {
        format!("Bearer {token}")
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
            return Ok(json!([]));
        }
        for attempt in 0..3 {
            self.throttle().await;
            let token = self.bearer_token().await?;
            let response = self
                .http
                .client
                .get(url)
                .header("Authorization", self.auth_header(&token))
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
                format!("Revolut HTTP {status}: {text}"),
            ));
        }
        serde_json::from_str(&text).map_err(std::io::Error::other)
    }

    fn extract_array(body: &Value) -> Vec<Value> {
        if let Some(items) = body.as_array() {
            return items.clone();
        }
        if let Some(items) = body.get("items").and_then(|v| v.as_array()) {
            return items.clone();
        }
        Vec::new()
    }

    pub async fn list_accounts(&self) -> Result<Vec<Value>, std::io::Error> {
        let url = format!("{}/accounts", self.api_base);
        let body = self.get_json(&url, "accounts.json").await?;
        Ok(Self::extract_array(&body))
    }

    pub async fn list_transactions(
        &self,
        from_iso: &str,
        to_iso: Option<&str>,
    ) -> Result<Vec<Value>, std::io::Error> {
        if let Some(dir) = self.fixture_dir.as_deref() {
            if let Some(body) = Self::fixture_path(dir, "transactions.json") {
                return Ok(Self::extract_array(&body));
            }
            return Ok(Vec::new());
        }

        let mut all_rows = Vec::new();
        let mut cursor_to = to_iso.map(str::to_string);
        loop {
            self.throttle().await;
            let mut url = format!(
                "{}/transactions?from={}&count={}",
                self.api_base,
                urlencoding(from_iso),
                MAX_TX_PAGE_SIZE
            );
            if let Some(ref to) = cursor_to {
                url.push_str(&format!("&to={}", urlencoding(to)));
            }

            let mut body: Option<Value> = None;
            for attempt in 0..3 {
                let token = self.bearer_token().await?;
                let response = self
                    .http
                    .client
                    .get(&url)
                    .header("Authorization", self.auth_header(&token))
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
            let page = Self::extract_array(&body);
            let page_len = page.len();
            if page_len == 0 {
                break;
            }
            let oldest_created_at = page.last().and_then(|row| revolut_str(row, "created_at"));
            all_rows.extend(page);
            if page_len < MAX_TX_PAGE_SIZE as usize {
                break;
            }
            let Some(next_to) = oldest_created_at else {
                break;
            };
            if cursor_to.as_deref() == Some(next_to.as_str()) {
                break;
            }
            cursor_to = Some(next_to);
        }
        Ok(all_rows)
    }
}

fn urlencoding(value: &str) -> String {
    value
        .chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u8),
        })
        .collect()
}

pub fn revolut_str(obj: &Value, key: &str) -> Option<String> {
    obj.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

pub fn revolut_f64(obj: &Value, key: &str) -> Option<f64> {
    obj.get(key).and_then(|v| v.as_f64())
}

pub fn revolut_bool(obj: &Value, key: &str) -> Option<bool> {
    obj.get(key).and_then(|v| v.as_bool())
}

pub fn record_id(obj: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| revolut_str(obj, key))
        .or_else(|| obj.get("id").and_then(|v| v.as_str()).map(str::to_string))
}

pub fn lookback_from_iso(start_date: &str, lookback_days: u32) -> Result<String, std::io::Error> {
    let start = NaiveDate::parse_from_str(start_date.trim(), "%Y-%m-%d")
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
    let effective = start - chrono::Duration::days(lookback_days as i64);
    Ok(effective
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string())
}

pub fn now_iso() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn leg_direction(amount: f64) -> &'static str {
    if amount > 0.0 {
        "in"
    } else if amount < 0.0 {
        "out"
    } else {
        "zero"
    }
}

fn leg_amount_abs(amount: f64) -> f64 {
    amount.abs()
}

pub fn map_account(
    ingest_run_date: &str,
    run_date: &str,
    connection_id: &str,
    obj: &Value,
) -> Value {
    json!({
        "ingest_run_date": ingest_run_date,
        "run_date": run_date,
        "connection_id": connection_id,
        "account_id": record_id(obj, &["id", "account_id"]),
        "currency": revolut_str(obj, "currency"),
        "balance": revolut_f64(obj, "balance"),
        "state": revolut_str(obj, "state"),
        "public_account": revolut_bool(obj, "public"),
        "created_at": revolut_str(obj, "created_at"),
        "updated_at": revolut_str(obj, "updated_at"),
    })
}

pub fn map_transaction(ingest_run_date: &str, connection_id: &str, obj: &Value) -> Value {
    let legs = obj.get("legs").and_then(|v| v.as_array());
    let (currency, amount) = if let Some(legs) = legs {
        let first = legs.first();
        let currency = first.and_then(|leg| revolut_str(leg, "currency"));
        let amount = legs
            .iter()
            .filter_map(|leg| revolut_f64(leg, "amount"))
            .reduce(|a, b| a + b)
            .or_else(|| first.and_then(|leg| revolut_f64(leg, "amount")));
        (currency, amount)
    } else {
        (revolut_str(obj, "currency"), revolut_f64(obj, "amount"))
    };
    json!({
        "ingest_run_date": ingest_run_date,
        "connection_id": connection_id,
        "transaction_id": record_id(obj, &["id", "transaction_id"]),
        "transaction_type": revolut_str(obj, "type"),
        "state": revolut_str(obj, "state"),
        "currency": currency,
        "amount": amount,
        "created_at": revolut_str(obj, "created_at"),
        "completed_at": revolut_str(obj, "completed_at"),
    })
}

pub fn map_transaction_legs(ingest_run_date: &str, connection_id: &str, obj: &Value) -> Vec<Value> {
    let transaction_id = record_id(obj, &["id", "transaction_id"]).unwrap_or_default();
    let created_at = revolut_str(obj, "created_at");
    let legs = match obj.get("legs").and_then(|v| v.as_array()) {
        Some(legs) => legs.clone(),
        None => return Vec::new(),
    };
    legs.iter()
        .enumerate()
        .map(|(index, leg)| {
            let amount_raw = revolut_f64(leg, "amount").unwrap_or(0.0);
            let leg_id = record_id(leg, &["leg_id", "id"])
                .unwrap_or_else(|| format!("{transaction_id}:{index}"));
            json!({
                "ingest_run_date": ingest_run_date,
                "connection_id": connection_id,
                "leg_id": leg_id,
                "transaction_id": transaction_id,
                "account_id": revolut_str(leg, "account_id"),
                "currency": revolut_str(leg, "currency"),
                "amount": leg_amount_abs(amount_raw),
                "direction": leg_direction(amount_raw),
                "created_at": created_at,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use skippr_plugin_shared_api_source::RetryableHttpClient;
    use std::sync::{LazyLock, Mutex};

    static ENV_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    #[test]
    fn map_account_omits_name_field() {
        let row = map_account(
            "2024-06-01",
            "2024-06-01",
            "conn-1",
            &json!({
                "id": "acc-1",
                "name": "Secret Account",
                "currency": "GBP",
                "balance": 100.0,
                "state": "active",
                "public": false,
                "created_at": "2024-01-01T00:00:00Z",
                "updated_at": "2024-06-01T00:00:00Z"
            }),
        );
        assert_eq!(row["account_id"], "acc-1");
        assert!(row.get("name").is_none());
        assert_eq!(row["currency"], "GBP");
    }

    #[test]
    fn map_transaction_legs_sets_direction() {
        let legs = map_transaction_legs(
            "2024-06-01",
            "conn-1",
            &json!({
                "id": "tx-1",
                "created_at": "2024-06-01T10:00:00Z",
                "legs": [
                    {"leg_id": "leg-in", "account_id": "acc-1", "amount": 50.0, "currency": "GBP"},
                    {"leg_id": "leg-out", "account_id": "acc-2", "amount": -50.0, "currency": "GBP"}
                ]
            }),
        );
        assert_eq!(legs.len(), 2);
        assert_eq!(legs[0]["direction"], "in");
        assert_eq!(legs[0]["amount"], 50.0);
        assert_eq!(legs[1]["direction"], "out");
        assert_eq!(legs[1]["amount"], 50.0);
    }

    #[tokio::test]
    async fn fixture_mode_missing_list_file_returns_empty_rows() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!(
            "skippr_revolut_fixture_missing_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let client = RevolutApiClient::with_fixture_dir(
            RetryableHttpClient::new(skippr_plugin_shared_api_source::RetryConfig::default()),
            DEFAULT_API_BASE.to_string(),
            "fixture".to_string(),
            0,
            Some(dir.to_string_lossy().to_string()),
            None,
        );

        let rows = client
            .list_transactions("2024-01-01T00:00:00Z", None)
            .await
            .unwrap();
        assert!(rows.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
