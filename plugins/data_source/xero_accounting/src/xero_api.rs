use serde_json::{json, Value};
use skippr_plugin_shared_api_source::{OAuth2RefreshTokenAuth, RetryableHttpClient};
use tokio::time::{sleep, Duration};

pub const FIXTURE_ENV: &str = "SKIPPR_XERO_FIXTURE_DIR";
const API_BASE: &str = "https://api.xero.com/api.xro/2.0";
pub const DEFAULT_TOKEN_URL: &str = "https://identity.xero.com/connect/token";

#[derive(Clone)]
pub struct XeroApiClient {
    pub http: RetryableHttpClient,
    pub tenant_id: String,
    access_token: String,
    pub min_interval_ms: u64,
    pub page_size: u32,
    fixture_dir: Option<String>,
}

impl XeroApiClient {
    pub fn new(
        http: RetryableHttpClient,
        tenant_id: String,
        access_token: String,
        min_interval_ms: u64,
        page_size: u32,
    ) -> Self {
        let fixture_dir = std::env::var(FIXTURE_ENV)
            .ok()
            .map(|dir| dir.trim().to_string())
            .filter(|dir| !dir.is_empty());
        Self::with_fixture_dir(
            http,
            tenant_id,
            access_token,
            min_interval_ms,
            page_size,
            fixture_dir,
        )
    }

    pub fn with_fixture_dir(
        http: RetryableHttpClient,
        tenant_id: String,
        access_token: String,
        min_interval_ms: u64,
        page_size: u32,
        fixture_dir: Option<String>,
    ) -> Self {
        Self {
            http,
            tenant_id,
            access_token,
            min_interval_ms,
            page_size,
            fixture_dir: fixture_dir
                .map(|dir| dir.trim().to_string())
                .filter(|dir| !dir.is_empty()),
        }
    }

    pub async fn from_oauth(
        http: RetryableHttpClient,
        tenant_id: String,
        token_url: &str,
        client_id: &str,
        client_secret: &str,
        refresh_token: &str,
        min_interval_ms: u64,
        page_size: u32,
    ) -> Result<Self, std::io::Error> {
        let oauth = OAuth2RefreshTokenAuth::new(token_url, client_id, client_secret, refresh_token);
        let access_token = oauth.refresh().await.map_err(std::io::Error::other)?;
        Ok(Self::new(
            http,
            tenant_id,
            access_token,
            min_interval_ms,
            page_size,
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
                format!("Xero HTTP {status}: {text}"),
            ));
        }
        serde_json::from_str(&text).map_err(std::io::Error::other)
    }

    fn extract_array(body: &Value, key: &str) -> Vec<Value> {
        body.get(key)
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
    }

    fn page_count(body: &Value) -> u32 {
        body.get("pagination")
            .and_then(|p| p.get("pageCount"))
            .and_then(|v| v.as_u64())
            .unwrap_or(1) as u32
    }

    async fn get_json(
        &self,
        path: &str,
        fixture: &str,
        if_modified_since: Option<&str>,
    ) -> Result<Value, std::io::Error> {
        if let Some(dir) = self.fixture_dir.as_deref() {
            if let Some(body) = Self::fixture_path(dir, fixture) {
                return Ok(body);
            }
            return Ok(json!({}));
        }
        let url = format!("{API_BASE}{path}");
        for attempt in 0..3 {
            self.throttle().await;
            let mut req = self
                .http
                .client
                .get(&url)
                .header("Authorization", self.auth_header())
                .header("xero-tenant-id", &self.tenant_id)
                .header("Accept", "application/json");
            if let Some(since) = if_modified_since {
                req = req.header("If-Modified-Since", since);
            }
            let response = req.send().await.map_err(std::io::Error::other)?;
            let status = response.status();
            if attempt < 2 && (status.as_u16() == 429 || status.is_server_error()) {
                Self::sleep_before_retry(&response, attempt).await;
                continue;
            }
            return Self::parse_response(response).await;
        }
        unreachable!("bounded retry loop always returns");
    }

    async fn list_paged(
        &self,
        path: &str,
        array_key: &str,
        fixture: &str,
        if_modified_since: Option<&str>,
        where_clause: Option<&str>,
    ) -> Result<Vec<Value>, std::io::Error> {
        if let Some(dir) = self.fixture_dir.as_deref() {
            if let Some(body) = Self::fixture_path(dir, fixture) {
                return Ok(Self::extract_array(&body, array_key));
            }
            return Ok(Vec::new());
        }

        let mut all_rows = Vec::new();
        let mut page: u32 = 1;
        loop {
            self.throttle().await;
            let mut url = format!("{API_BASE}{path}?page={page}&pageSize={}", self.page_size);
            if let Some(w) = where_clause {
                url.push_str(&format!("&where={}", urlencoding::encode(w)));
            }
            let mut body: Option<Value> = None;
            for attempt in 0..3 {
                let mut req = self
                    .http
                    .client
                    .get(&url)
                    .header("Authorization", self.auth_header())
                    .header("xero-tenant-id", &self.tenant_id)
                    .header("Accept", "application/json");
                if let Some(since) = if_modified_since {
                    req = req.header("If-Modified-Since", since);
                }
                let response = req.send().await.map_err(std::io::Error::other)?;
                let status = response.status();
                if attempt < 2 && (status.as_u16() == 429 || status.is_server_error()) {
                    Self::sleep_before_retry(&response, attempt).await;
                    continue;
                }
                body = Some(Self::parse_response(response).await?);
                break;
            }
            let body = body.expect("bounded retry loop always sets body");
            let page_rows = Self::extract_array(&body, array_key);
            let page_len = page_rows.len();
            all_rows.extend(page_rows);
            let total_pages = Self::page_count(&body);
            if page >= total_pages || page_len == 0 {
                break;
            }
            page += 1;
        }
        Ok(all_rows)
    }

    pub async fn organisation(&self) -> Result<Value, std::io::Error> {
        let body = self
            .get_json("/Organisation", "organisation.json", None)
            .await?;
        Ok(Self::extract_array(&body, "Organisations")
            .into_iter()
            .next()
            .unwrap_or(body))
    }

    pub async fn list_accounts(
        &self,
        if_modified_since: Option<&str>,
    ) -> Result<Vec<Value>, std::io::Error> {
        let body = self
            .get_json("/Accounts", "accounts.json", if_modified_since)
            .await?;
        Ok(Self::extract_array(&body, "Accounts"))
    }

    pub async fn list_contacts(
        &self,
        if_modified_since: Option<&str>,
    ) -> Result<Vec<Value>, std::io::Error> {
        self.list_paged(
            "/Contacts",
            "Contacts",
            "contacts.json",
            if_modified_since,
            None,
        )
        .await
    }

    pub async fn list_invoices(
        &self,
        if_modified_since: Option<&str>,
        where_clause: Option<&str>,
    ) -> Result<Vec<Value>, std::io::Error> {
        self.list_paged(
            "/Invoices",
            "Invoices",
            "invoices.json",
            if_modified_since,
            where_clause,
        )
        .await
    }

    pub async fn list_payments(
        &self,
        if_modified_since: Option<&str>,
        where_clause: Option<&str>,
    ) -> Result<Vec<Value>, std::io::Error> {
        self.list_paged(
            "/Payments",
            "Payments",
            "payments.json",
            if_modified_since,
            where_clause,
        )
        .await
    }

    pub async fn list_bank_transactions(
        &self,
        if_modified_since: Option<&str>,
        where_clause: Option<&str>,
    ) -> Result<Vec<Value>, std::io::Error> {
        self.list_paged(
            "/BankTransactions",
            "BankTransactions",
            "bank_transactions.json",
            if_modified_since,
            where_clause,
        )
        .await
    }
}

pub fn xero_str(obj: &Value, key: &str) -> Option<String> {
    obj.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

pub fn xero_f64(obj: &Value, key: &str) -> Option<f64> {
    obj.get(key).and_then(|v| v.as_f64())
}

pub fn xero_bool(obj: &Value, key: &str) -> Option<bool> {
    obj.get(key).and_then(|v| v.as_bool())
}

pub fn nested_str(obj: &Value, path: &[&str]) -> Option<String> {
    let mut current = obj;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_str().map(str::to_string)
}

pub fn record_id(obj: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| xero_str(obj, key))
        .or_else(|| obj.get("id").and_then(|v| v.as_str()).map(str::to_string))
}

pub fn map_organisation(
    ingest_run_date: &str,
    run_date: &str,
    tenant_id: &str,
    obj: &Value,
) -> Value {
    json!({
        "ingest_run_date": ingest_run_date,
        "run_date": run_date,
        "tenant_id": tenant_id,
        "organisation_id": record_id(obj, &["OrganisationID", "organisation_id"]),
        "organisation_type": xero_str(obj, "OrganisationType"),
        "country_code": xero_str(obj, "CountryCode"),
        "base_currency": xero_str(obj, "BaseCurrency"),
        "timezone": xero_str(obj, "Timezone"),
        "version": xero_str(obj, "Version"),
        "financial_year_end_day": obj.get("FinancialYearEndDay").and_then(|v| v.as_i64()),
        "financial_year_end_month": obj.get("FinancialYearEndMonth").and_then(|v| v.as_i64()),
    })
}

pub fn map_account(ingest_run_date: &str, run_date: &str, tenant_id: &str, obj: &Value) -> Value {
    json!({
        "ingest_run_date": ingest_run_date,
        "run_date": run_date,
        "tenant_id": tenant_id,
        "account_id": record_id(obj, &["AccountID", "account_id"]),
        "account_code": xero_str(obj, "Code"),
        "account_type": xero_str(obj, "Type"),
        "status": xero_str(obj, "Status"),
        "class": xero_str(obj, "Class"),
        "currency_code": xero_str(obj, "CurrencyCode"),
        "reporting_code": xero_str(obj, "ReportingCode"),
        "tax_type": xero_str(obj, "TaxType"),
        "enable_payments_to_account": xero_bool(obj, "EnablePaymentsToAccount"),
    })
}

pub fn map_contact(ingest_run_date: &str, run_date: &str, tenant_id: &str, obj: &Value) -> Value {
    json!({
        "ingest_run_date": ingest_run_date,
        "run_date": run_date,
        "tenant_id": tenant_id,
        "contact_id": record_id(obj, &["ContactID", "contact_id"]),
        "status": xero_str(obj, "ContactStatus"),
        "is_customer": xero_bool(obj, "IsCustomer"),
        "is_supplier": xero_bool(obj, "IsSupplier"),
    })
}

pub fn map_invoice(ingest_run_date: &str, tenant_id: &str, obj: &Value) -> Value {
    json!({
        "ingest_run_date": ingest_run_date,
        "tenant_id": tenant_id,
        "invoice_id": record_id(obj, &["InvoiceID", "invoice_id"]),
        "invoice_type": xero_str(obj, "Type"),
        "status": xero_str(obj, "Status"),
        "currency_code": xero_str(obj, "CurrencyCode"),
        "date": xero_str(obj, "Date"),
        "due_date": xero_str(obj, "DueDate"),
        "updated_date_utc": xero_str(obj, "UpdatedDateUTC"),
        "sub_total": xero_f64(obj, "SubTotal"),
        "total_tax": xero_f64(obj, "TotalTax"),
        "total": xero_f64(obj, "Total"),
        "amount_due": xero_f64(obj, "AmountDue"),
        "amount_paid": xero_f64(obj, "AmountPaid"),
        "amount_credited": xero_f64(obj, "AmountCredited"),
        "contact_id": nested_str(obj, &["Contact", "ContactID"]),
        "line_amount_types": xero_str(obj, "LineAmountTypes"),
        "fully_paid_on_date": xero_str(obj, "FullyPaidOnDate"),
    })
}

pub fn map_invoice_lines(ingest_run_date: &str, tenant_id: &str, invoice: &Value) -> Vec<Value> {
    let invoice_id = record_id(invoice, &["InvoiceID", "invoice_id"]).unwrap_or_default();
    invoice
        .get("LineItems")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .map(|line| {
                    json!({
                        "ingest_run_date": ingest_run_date,
                        "tenant_id": tenant_id,
                        "invoice_id": invoice_id,
                        "line_id": record_id(line, &["LineItemID", "line_id"]),
                        "account_code": xero_str(line, "AccountCode"),
                        "tax_type": xero_str(line, "TaxType"),
                        "quantity": xero_f64(line, "Quantity"),
                        "unit_amount": xero_f64(line, "UnitAmount"),
                        "line_amount": xero_f64(line, "LineAmount"),
                        "discount_rate": xero_f64(line, "DiscountRate"),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn map_payment(ingest_run_date: &str, tenant_id: &str, obj: &Value) -> Value {
    json!({
        "ingest_run_date": ingest_run_date,
        "tenant_id": tenant_id,
        "payment_id": record_id(obj, &["PaymentID", "payment_id"]),
        "date": xero_str(obj, "Date"),
        "status": xero_str(obj, "Status"),
        "amount": xero_f64(obj, "Amount"),
        "currency_rate": xero_f64(obj, "CurrencyRate"),
        "payment_type": xero_str(obj, "PaymentType"),
        "invoice_id": nested_str(obj, &["Invoice", "InvoiceID"]),
        "account_id": nested_str(obj, &["Account", "AccountID"]),
        "updated_date_utc": xero_str(obj, "UpdatedDateUTC"),
    })
}

pub fn map_bank_transaction(ingest_run_date: &str, tenant_id: &str, obj: &Value) -> Value {
    json!({
        "ingest_run_date": ingest_run_date,
        "tenant_id": tenant_id,
        "bank_transaction_id": record_id(obj, &["BankTransactionID", "bank_transaction_id"]),
        "transaction_type": xero_str(obj, "Type"),
        "status": xero_str(obj, "Status"),
        "date": xero_str(obj, "Date"),
        "currency_code": xero_str(obj, "CurrencyCode"),
        "total": xero_f64(obj, "Total"),
        "sub_total": xero_f64(obj, "SubTotal"),
        "total_tax": xero_f64(obj, "TotalTax"),
        "is_reconciled": xero_bool(obj, "IsReconciled"),
        "contact_id": nested_str(obj, &["Contact", "ContactID"]),
        "updated_date_utc": xero_str(obj, "UpdatedDateUTC"),
    })
}

pub fn max_updated_date_utc(rows: &[Value]) -> Option<String> {
    rows.iter()
        .filter_map(|row| xero_str(row, "UpdatedDateUTC"))
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;
    use skippr_plugin_shared_api_source::RetryableHttpClient;
    use std::sync::{LazyLock, Mutex};

    static ENV_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    #[test]
    fn map_contact_is_id_only() {
        let row = json!({
            "ContactID": "c-1",
            "Name": "Secret Name",
            "EmailAddress": "secret@example.com",
            "ContactStatus": "ACTIVE",
            "IsCustomer": true,
            "IsSupplier": false
        });
        let mapped = map_contact("2024-06-01", "2024-06-01", "tenant-1", &row);
        assert_eq!(mapped["contact_id"], "c-1");
        assert_eq!(mapped["status"], "ACTIVE");
        assert!(mapped.get("Name").is_none());
        assert!(mapped.get("EmailAddress").is_none());
    }

    #[tokio::test]
    async fn fixture_mode_missing_list_file_returns_empty_rows() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!(
            "skippr_xero_fixture_missing_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let client = XeroApiClient::with_fixture_dir(
            RetryableHttpClient::new(skippr_plugin_shared_api_source::RetryConfig::default()),
            "tenant-1".to_string(),
            "fixture".to_string(),
            0,
            100,
            Some(dir.to_string_lossy().to_string()),
        );

        let rows = client.list_invoices(None, None).await.unwrap();
        assert!(rows.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
