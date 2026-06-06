use serde_json::{json, Value};
use skippr_plugin_shared_api_source::{OAuth2RefreshTokenAuth, RetryableHttpClient};
use tokio::time::{sleep, Duration};

pub const FIXTURE_ENV: &str = "SKIPPR_STRIPE_FIXTURE_DIR";
const API_BASE: &str = "https://api.stripe.com/v1";
const STRIPE_API_VERSION: &str = "2024-11-20.acacia";

#[derive(Clone)]
pub struct StripeApiClient {
    pub http: RetryableHttpClient,
    pub stripe_account_id: String,
    access_token: String,
    pub min_interval_ms: u64,
    fixture_dir: Option<String>,
}

impl StripeApiClient {
    pub fn new(
        http: RetryableHttpClient,
        stripe_account_id: String,
        access_token: String,
        min_interval_ms: u64,
    ) -> Self {
        let fixture_dir = std::env::var(FIXTURE_ENV)
            .ok()
            .map(|dir| dir.trim().to_string())
            .filter(|dir| !dir.is_empty());
        Self::with_fixture_dir(
            http,
            stripe_account_id,
            access_token,
            min_interval_ms,
            fixture_dir,
        )
    }

    pub fn with_fixture_dir(
        http: RetryableHttpClient,
        stripe_account_id: String,
        access_token: String,
        min_interval_ms: u64,
        fixture_dir: Option<String>,
    ) -> Self {
        Self {
            http,
            stripe_account_id,
            access_token,
            min_interval_ms,
            fixture_dir: fixture_dir
                .map(|dir| dir.trim().to_string())
                .filter(|dir| !dir.is_empty()),
        }
    }

    pub async fn from_oauth(
        http: RetryableHttpClient,
        stripe_account_id: String,
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
            stripe_account_id,
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

    async fn get_list(
        &self,
        path: &str,
        fixture: &str,
        created_gte: Option<i64>,
        expand: Option<&str>,
    ) -> Result<Vec<Value>, std::io::Error> {
        if let Some(dir) = self.fixture_dir.as_deref() {
            if let Some(body) = Self::fixture_path(dir, fixture) {
                return Ok(Self::extract_list_data(&body));
            }
            return Ok(Vec::new());
        }

        let mut all_rows = Vec::new();
        let mut starting_after: Option<String> = None;
        loop {
            self.throttle().await;
            let mut url = format!("{API_BASE}{path}?limit=100");
            if let Some(ts) = created_gte {
                url.push_str(&format!("&created[gte]={ts}"));
            }
            if let Some(token) = starting_after.as_deref() {
                url.push_str(&format!("&starting_after={token}"));
            }
            if let Some(exp) = expand {
                url.push_str(&format!("&expand[]={exp}"));
            }

            let response = self
                .http
                .client
                .get(&url)
                .header("Authorization", self.auth_header())
                .header("Stripe-Version", STRIPE_API_VERSION)
                .send()
                .await
                .map_err(std::io::Error::other)?;

            let body = Self::parse_response(response).await?;
            let page = Self::extract_list_data(&body);
            let has_more = body
                .get("has_more")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if let Some(last) = page
                .last()
                .and_then(|o| o.get("id"))
                .and_then(|v| v.as_str())
            {
                starting_after = Some(last.to_string());
            }
            all_rows.extend(page);
            if !has_more {
                break;
            }
        }
        Ok(all_rows)
    }

    async fn get_singleton(&self, path: &str, fixture: &str) -> Result<Value, std::io::Error> {
        if let Some(dir) = self.fixture_dir.as_deref() {
            if let Some(body) = Self::fixture_path(dir, fixture) {
                return Ok(body);
            }
            return Ok(json!({ "id": self.stripe_account_id }));
        }
        self.throttle().await;
        let url = format!("{API_BASE}{path}");
        let response = self
            .http
            .client
            .get(&url)
            .header("Authorization", self.auth_header())
            .header("Stripe-Version", STRIPE_API_VERSION)
            .send()
            .await
            .map_err(std::io::Error::other)?;
        Self::parse_response(response).await
    }

    fn extract_list_data(body: &Value) -> Vec<Value> {
        body.get("data")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
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
                format!("Stripe HTTP {status}: {text}"),
            ));
        }
        serde_json::from_str(&text).map_err(std::io::Error::other)
    }

    pub async fn account(&self) -> Result<Value, std::io::Error> {
        self.get_singleton("/account", "account.json").await
    }

    pub async fn list_products(
        &self,
        created_gte: Option<i64>,
    ) -> Result<Vec<Value>, std::io::Error> {
        self.get_list("/products", "products.json", created_gte, None)
            .await
    }

    pub async fn list_prices(
        &self,
        created_gte: Option<i64>,
    ) -> Result<Vec<Value>, std::io::Error> {
        self.get_list("/prices", "prices.json", created_gte, None)
            .await
    }

    pub async fn list_customers(
        &self,
        created_gte: Option<i64>,
    ) -> Result<Vec<Value>, std::io::Error> {
        self.get_list("/customers", "customers.json", created_gte, None)
            .await
    }

    pub async fn list_subscriptions(
        &self,
        created_gte: Option<i64>,
    ) -> Result<Vec<Value>, std::io::Error> {
        self.get_list(
            "/subscriptions",
            "subscriptions.json",
            created_gte,
            Some("data.items.data.price"),
        )
        .await
    }

    pub async fn list_invoices(
        &self,
        created_gte: Option<i64>,
    ) -> Result<Vec<Value>, std::io::Error> {
        self.get_list(
            "/invoices",
            "invoices.json",
            created_gte,
            Some("data.lines"),
        )
        .await
    }

    pub async fn list_charges(
        &self,
        created_gte: Option<i64>,
    ) -> Result<Vec<Value>, std::io::Error> {
        self.get_list("/charges", "charges.json", created_gte, None)
            .await
    }

    pub async fn list_payment_intents(
        &self,
        created_gte: Option<i64>,
    ) -> Result<Vec<Value>, std::io::Error> {
        self.get_list(
            "/payment_intents",
            "payment_intents.json",
            created_gte,
            None,
        )
        .await
    }

    pub async fn list_refunds(
        &self,
        created_gte: Option<i64>,
    ) -> Result<Vec<Value>, std::io::Error> {
        self.get_list("/refunds", "refunds.json", created_gte, None)
            .await
    }

    pub async fn list_disputes(
        &self,
        created_gte: Option<i64>,
    ) -> Result<Vec<Value>, std::io::Error> {
        self.get_list("/disputes", "disputes.json", created_gte, None)
            .await
    }

    pub async fn list_balance_transactions(
        &self,
        created_gte: Option<i64>,
    ) -> Result<Vec<Value>, std::io::Error> {
        self.get_list(
            "/balance_transactions",
            "balance_transactions.json",
            created_gte,
            None,
        )
        .await
    }

    pub async fn list_payouts(
        &self,
        created_gte: Option<i64>,
    ) -> Result<Vec<Value>, std::io::Error> {
        self.get_list("/payouts", "payouts.json", created_gte, None)
            .await
    }

    pub async fn list_coupons(
        &self,
        created_gte: Option<i64>,
    ) -> Result<Vec<Value>, std::io::Error> {
        self.get_list("/coupons", "coupons.json", created_gte, None)
            .await
    }

    pub async fn list_promotion_codes(
        &self,
        created_gte: Option<i64>,
    ) -> Result<Vec<Value>, std::io::Error> {
        self.get_list(
            "/promotion_codes",
            "promotion_codes.json",
            created_gte,
            None,
        )
        .await
    }
}

pub fn stripe_id(obj: &Value) -> Option<&str> {
    obj.get("id").and_then(|v| v.as_str())
}

pub fn stripe_str(obj: &Value, key: &str) -> Option<String> {
    obj.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

pub fn stripe_i64(obj: &Value, key: &str) -> Option<i64> {
    obj.get(key).and_then(|v| v.as_i64())
}

pub fn stripe_bool(obj: &Value, key: &str) -> Option<bool> {
    obj.get(key).and_then(|v| v.as_bool())
}

pub fn unix_to_iso(ts: Option<i64>) -> Option<String> {
    ts.and_then(|t| {
        chrono::DateTime::from_timestamp(t, 0).map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
    })
}

pub fn subscription_mrr_cents(sub: &Value) -> i64 {
    let mut total = 0i64;
    if let Some(items) = sub
        .pointer("/items/data")
        .or_else(|| sub.pointer("/items/data"))
        .and_then(|v| v.as_array())
    {
        for item in items {
            let qty = item.get("quantity").and_then(|v| v.as_i64()).unwrap_or(1);
            let unit = item
                .pointer("/price/unit_amount")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let interval = item
                .pointer("/price/recurring/interval")
                .and_then(|v| v.as_str())
                .unwrap_or("month");
            let interval_count = item
                .pointer("/price/recurring/interval_count")
                .and_then(|v| v.as_i64())
                .unwrap_or(1)
                .max(1);
            let monthly = match interval {
                "year" => unit / (12 * interval_count),
                "week" => unit * 4 / interval_count,
                "day" => unit * 30 / interval_count,
                _ => unit / interval_count,
            };
            total += monthly * qty;
        }
    }
    total
}

pub fn map_account(ingest_run_date: &str, stripe_account_id: &str, obj: &Value) -> Value {
    json!({
        "ingest_run_date": ingest_run_date,
        "stripe_account_id": stripe_account_id,
        "account_id": stripe_id(obj),
        "business_type": stripe_str(obj, "business_type"),
        "country": stripe_str(obj, "country"),
        "default_currency": stripe_str(obj, "default_currency"),
        "charges_enabled": stripe_bool(obj, "charges_enabled"),
        "payouts_enabled": stripe_bool(obj, "payouts_enabled"),
        "created_at": unix_to_iso(stripe_i64(obj, "created")),
    })
}

pub fn map_product(ingest_run_date: &str, stripe_account_id: &str, obj: &Value) -> Value {
    json!({
        "ingest_run_date": ingest_run_date,
        "stripe_account_id": stripe_account_id,
        "product_id": stripe_id(obj),
        "name": stripe_str(obj, "name"),
        "active": stripe_bool(obj, "active"),
        "created_at": unix_to_iso(stripe_i64(obj, "created")),
        "updated_at": unix_to_iso(stripe_i64(obj, "updated")),
    })
}

pub fn map_price(ingest_run_date: &str, stripe_account_id: &str, obj: &Value) -> Value {
    json!({
        "ingest_run_date": ingest_run_date,
        "stripe_account_id": stripe_account_id,
        "price_id": stripe_id(obj),
        "product_id": stripe_str(obj, "product"),
        "currency": stripe_str(obj, "currency"),
        "unit_amount": stripe_i64(obj, "unit_amount"),
        "billing_scheme": stripe_str(obj, "billing_scheme"),
        "recurring_interval": obj.pointer("/recurring/interval").and_then(|v| v.as_str()),
        "recurring_interval_count": obj.pointer("/recurring/interval_count").and_then(|v| v.as_i64()),
        "active": stripe_bool(obj, "active"),
        "created_at": unix_to_iso(stripe_i64(obj, "created")),
    })
}

pub fn map_customer(ingest_run_date: &str, stripe_account_id: &str, obj: &Value) -> Value {
    json!({
        "ingest_run_date": ingest_run_date,
        "stripe_account_id": stripe_account_id,
        "customer_id": stripe_id(obj),
        "currency": stripe_str(obj, "currency"),
        "balance": stripe_i64(obj, "balance"),
        "delinquent": stripe_bool(obj, "delinquent"),
        "created_at": unix_to_iso(stripe_i64(obj, "created")),
    })
}

pub fn map_subscription(ingest_run_date: &str, stripe_account_id: &str, obj: &Value) -> Value {
    json!({
        "ingest_run_date": ingest_run_date,
        "stripe_account_id": stripe_account_id,
        "subscription_id": stripe_id(obj),
        "customer_id": stripe_str(obj, "customer"),
        "status": stripe_str(obj, "status"),
        "currency": stripe_str(obj, "currency"),
        "mrr_cents": subscription_mrr_cents(obj),
        "cancel_at_period_end": stripe_bool(obj, "cancel_at_period_end"),
        "current_period_start": unix_to_iso(stripe_i64(obj, "current_period_start")),
        "current_period_end": unix_to_iso(stripe_i64(obj, "current_period_end")),
        "canceled_at": unix_to_iso(stripe_i64(obj, "canceled_at")),
        "created_at": unix_to_iso(stripe_i64(obj, "created")),
    })
}

pub fn map_invoice(ingest_run_date: &str, stripe_account_id: &str, obj: &Value) -> Value {
    json!({
        "ingest_run_date": ingest_run_date,
        "stripe_account_id": stripe_account_id,
        "invoice_id": stripe_id(obj),
        "customer_id": stripe_str(obj, "customer"),
        "subscription_id": stripe_str(obj, "subscription"),
        "status": stripe_str(obj, "status"),
        "currency": stripe_str(obj, "currency"),
        "amount_due": stripe_i64(obj, "amount_due"),
        "amount_paid": stripe_i64(obj, "amount_paid"),
        "amount_remaining": stripe_i64(obj, "amount_remaining"),
        "total": stripe_i64(obj, "total"),
        "subtotal": stripe_i64(obj, "subtotal"),
        "period_start": unix_to_iso(stripe_i64(obj, "period_start")),
        "period_end": unix_to_iso(stripe_i64(obj, "period_end")),
        "created_at": unix_to_iso(stripe_i64(obj, "created")),
    })
}

pub fn map_invoice_lines(
    ingest_run_date: &str,
    stripe_account_id: &str,
    invoice: &Value,
) -> Vec<Value> {
    let invoice_id = stripe_id(invoice).unwrap_or("");
    let lines = invoice
        .pointer("/lines/data")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    lines
        .iter()
        .map(|line| {
            json!({
                "ingest_run_date": ingest_run_date,
                "stripe_account_id": stripe_account_id,
                "invoice_id": invoice_id,
                "line_id": stripe_id(line),
                "amount": stripe_i64(line, "amount"),
                "currency": stripe_str(line, "currency"),
                "quantity": stripe_i64(line, "quantity"),
                "description": stripe_str(line, "description"),
                "price_id": line.pointer("/price/id").and_then(|v| v.as_str()),
                "product_id": line.pointer("/price/product").and_then(|v| v.as_str()),
            })
        })
        .collect()
}

pub fn map_charge(ingest_run_date: &str, stripe_account_id: &str, obj: &Value) -> Value {
    json!({
        "ingest_run_date": ingest_run_date,
        "stripe_account_id": stripe_account_id,
        "charge_id": stripe_id(obj),
        "customer_id": stripe_str(obj, "customer"),
        "invoice_id": stripe_str(obj, "invoice"),
        "payment_intent_id": stripe_str(obj, "payment_intent"),
        "status": stripe_str(obj, "status"),
        "paid": stripe_bool(obj, "paid"),
        "refunded": stripe_bool(obj, "refunded"),
        "amount": stripe_i64(obj, "amount"),
        "amount_refunded": stripe_i64(obj, "amount_refunded"),
        "currency": stripe_str(obj, "currency"),
        "failure_code": stripe_str(obj, "failure_code"),
        "failure_message": stripe_str(obj, "failure_message"),
        "created_at": unix_to_iso(stripe_i64(obj, "created")),
    })
}

pub fn map_payment_intent(ingest_run_date: &str, stripe_account_id: &str, obj: &Value) -> Value {
    json!({
        "ingest_run_date": ingest_run_date,
        "stripe_account_id": stripe_account_id,
        "payment_intent_id": stripe_id(obj),
        "customer_id": stripe_str(obj, "customer"),
        "invoice_id": stripe_str(obj, "invoice"),
        "status": stripe_str(obj, "status"),
        "amount": stripe_i64(obj, "amount"),
        "amount_received": stripe_i64(obj, "amount_received"),
        "currency": stripe_str(obj, "currency"),
        "created_at": unix_to_iso(stripe_i64(obj, "created")),
    })
}

pub fn map_refund(ingest_run_date: &str, stripe_account_id: &str, obj: &Value) -> Value {
    json!({
        "ingest_run_date": ingest_run_date,
        "stripe_account_id": stripe_account_id,
        "refund_id": stripe_id(obj),
        "charge_id": stripe_str(obj, "charge"),
        "payment_intent_id": stripe_str(obj, "payment_intent"),
        "status": stripe_str(obj, "status"),
        "amount": stripe_i64(obj, "amount"),
        "currency": stripe_str(obj, "currency"),
        "reason": stripe_str(obj, "reason"),
        "created_at": unix_to_iso(stripe_i64(obj, "created")),
    })
}

pub fn map_dispute(ingest_run_date: &str, stripe_account_id: &str, obj: &Value) -> Value {
    json!({
        "ingest_run_date": ingest_run_date,
        "stripe_account_id": stripe_account_id,
        "dispute_id": stripe_id(obj),
        "charge_id": stripe_str(obj, "charge"),
        "status": stripe_str(obj, "status"),
        "reason": stripe_str(obj, "reason"),
        "amount": stripe_i64(obj, "amount"),
        "currency": stripe_str(obj, "currency"),
        "created_at": unix_to_iso(stripe_i64(obj, "created")),
    })
}

pub fn map_balance_transaction(
    ingest_run_date: &str,
    stripe_account_id: &str,
    obj: &Value,
) -> Value {
    json!({
        "ingest_run_date": ingest_run_date,
        "stripe_account_id": stripe_account_id,
        "balance_transaction_id": stripe_id(obj),
        "type": stripe_str(obj, "type"),
        "status": stripe_str(obj, "status"),
        "amount": stripe_i64(obj, "amount"),
        "fee": stripe_i64(obj, "fee"),
        "net": stripe_i64(obj, "net"),
        "currency": stripe_str(obj, "currency"),
        "source_id": stripe_str(obj, "source"),
        "created_at": unix_to_iso(stripe_i64(obj, "created")),
    })
}

pub fn map_payout(ingest_run_date: &str, stripe_account_id: &str, obj: &Value) -> Value {
    json!({
        "ingest_run_date": ingest_run_date,
        "stripe_account_id": stripe_account_id,
        "payout_id": stripe_id(obj),
        "status": stripe_str(obj, "status"),
        "amount": stripe_i64(obj, "amount"),
        "currency": stripe_str(obj, "currency"),
        "arrival_date": unix_to_iso(stripe_i64(obj, "arrival_date")),
        "created_at": unix_to_iso(stripe_i64(obj, "created")),
    })
}

pub fn map_coupon(ingest_run_date: &str, stripe_account_id: &str, obj: &Value) -> Value {
    json!({
        "ingest_run_date": ingest_run_date,
        "stripe_account_id": stripe_account_id,
        "coupon_id": stripe_id(obj),
        "name": stripe_str(obj, "name"),
        "percent_off": obj.get("percent_off").cloned(),
        "amount_off": stripe_i64(obj, "amount_off"),
        "currency": stripe_str(obj, "currency"),
        "duration": stripe_str(obj, "duration"),
        "valid": stripe_bool(obj, "valid"),
        "created_at": unix_to_iso(stripe_i64(obj, "created")),
    })
}

pub fn map_promotion_code(ingest_run_date: &str, stripe_account_id: &str, obj: &Value) -> Value {
    json!({
        "ingest_run_date": ingest_run_date,
        "stripe_account_id": stripe_account_id,
        "promotion_code_id": stripe_id(obj),
        "coupon_id": stripe_str(obj, "coupon"),
        "code": stripe_str(obj, "code"),
        "active": stripe_bool(obj, "active"),
        "created_at": unix_to_iso(stripe_i64(obj, "created")),
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
            "skippr_stripe_fixture_missing_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let client = StripeApiClient::with_fixture_dir(
            RetryableHttpClient::new(skippr_plugin_shared_api_source::RetryConfig::default()),
            "acct_fixture".to_string(),
            "fixture".to_string(),
            0,
            Some(dir.to_string_lossy().to_string()),
        );

        let rows = client.list_refunds(None).await.unwrap();

        assert!(rows.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn fixture_mode_missing_singleton_file_returns_account_stub() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!(
            "skippr_stripe_fixture_singleton_missing_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let client = StripeApiClient::with_fixture_dir(
            RetryableHttpClient::new(skippr_plugin_shared_api_source::RetryConfig::default()),
            "acct_fixture".to_string(),
            "fixture".to_string(),
            0,
            Some(dir.to_string_lossy().to_string()),
        );

        let account = client.account().await.unwrap();

        assert_eq!(account["id"], "acct_fixture");
        let _ = std::fs::remove_dir_all(dir);
    }
}
