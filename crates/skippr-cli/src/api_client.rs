use crate::auth::StoredCredentials;
use react_suite_data_engineer::metering::TokenProvider;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;

#[derive(Clone)]
pub struct ApiClient {
    base_url: String,
    http: reqwest::Client,
    tokens: Option<Arc<TokenProvider>>,
}

#[derive(Debug)]
pub struct ApiError {
    pub context: String,
    pub body: String,
    pub status: Option<StatusCode>,
}

impl ApiError {
    fn network(context: &str, err: reqwest::Error) -> Self {
        Self {
            context: context.to_string(),
            body: format!("Network error: {}", err),
            status: None,
        }
    }

    fn response(context: &str, status: StatusCode, body: String) -> Self {
        Self {
            context: context.to_string(),
            body,
            status: Some(status),
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.body.is_empty() {
            write!(f, "{}", self.context)
        } else {
            write!(f, "{}: {}", self.context, self.body)
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct DailyCost {
    pub date: String,
    pub cost: f64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct AccountResponse {
    pub profile: AccountProfile,
    pub balance: Balance,
    #[serde(default)]
    pub daily_costs_est: Vec<DailyCost>,
    #[serde(default)]
    pub monthly_cost_est: f64,
    #[serde(default)]
    pub recent_usage: Vec<serde_json::Value>,
    pub subscription: Option<Subscription>,
    #[serde(default)]
    pub eula: EulaAcceptance,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct EulaAcceptance {
    pub accepted_at: Option<String>,
    pub version: Option<String>,
    pub accepted_via: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Subscription {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub price_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct AccountProfile {
    pub plan: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Balance {
    #[serde(default)]
    pub balance: f64,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    pub token: Option<String>,
    pub refresh_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AddFundsResponse {
    pub checkout_url: String,
}

impl ApiClient {
    /// Create an unauthenticated client (for sign-in, confirm, API key exchange).
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
            tokens: None,
        }
    }

    /// Create an authenticated client backed by a shared TokenProvider.
    pub fn authenticated(base_url: &str, tokens: Arc<TokenProvider>) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
            tokens: Some(tokens),
        }
    }

    /// Send an authenticated request via the shared TokenProvider (auto-refreshes on 401).
    async fn send_with_auth(
        &self,
        context: &str,
        build: impl Fn(&str) -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, ApiError> {
        let tokens = self.tokens.as_ref().ok_or_else(|| ApiError {
            context: context.to_string(),
            body: "Not authenticated — call ApiClient::authenticated()".to_string(),
            status: None,
        })?;
        tokens
            .send_authenticated(&self.http, build)
            .await
            .map_err(|e| ApiError {
                context: context.to_string(),
                body: e,
                status: None,
            })
    }

    // -------------------------------------------------------------------
    // Unauthenticated endpoints
    // -------------------------------------------------------------------

    pub async fn sign_in(&self, email: &str) -> Result<(), String> {
        let url = format!("{}/auth/sign-in", self.base_url);
        let body = serde_json::json!({ "email": email });
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Network error: {}", e))?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("Sign-in failed: {}", text));
        }
        Ok(())
    }

    pub async fn confirm(&self, email: &str, code: &str) -> Result<StoredCredentials, String> {
        let url = format!("{}/auth/confirm", self.base_url);
        let body = serde_json::json!({ "email": email, "code": code });
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Network error: {}", e))?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("Confirm failed: {}", text));
        }

        let data: TokenResponse = resp
            .json()
            .await
            .map_err(|e| format!("Parse error: {}", e))?;

        Ok(StoredCredentials {
            access_token: data.token.unwrap_or_default(),
            refresh_token: data.refresh_token.unwrap_or_default(),
        })
    }

    pub async fn refresh(&self, refresh_token: &str) -> Result<StoredCredentials, ApiError> {
        let url = format!("{}/auth/refresh", self.base_url);
        let body = serde_json::json!({ "refresh_token": refresh_token });
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| ApiError::network("Token refresh failed", e))?;

        if !resp.status().is_success() {
            return Err(response_error("Token refresh failed", resp).await);
        }

        let data: TokenResponse = resp
            .json()
            .await
            .map_err(|e| ApiError::network("Token refresh parse failed", e))?;

        Ok(StoredCredentials {
            access_token: data.token.unwrap_or_default(),
            refresh_token: data.refresh_token.unwrap_or_default(),
        })
    }

    pub async fn exchange_api_key(&self, api_key: &str) -> Result<StoredCredentials, String> {
        let url = format!("{}/auth/api-key-exchange", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .send()
            .await
            .map_err(|e| format!("Network error: {}", e))?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("API key exchange failed: {}", text));
        }

        let data: TokenExchangeResponse = resp
            .json()
            .await
            .map_err(|e| format!("Parse error: {}", e))?;

        Ok(StoredCredentials {
            access_token: data.token,
            refresh_token: data.refresh_token,
        })
    }

    // -------------------------------------------------------------------
    // Authenticated endpoints (all go through send_with_auth → TokenProvider)
    // -------------------------------------------------------------------

    pub async fn get_account(&self) -> Result<AccountResponse, ApiError> {
        let url = format!("{}/account", self.base_url);
        let resp = self
            .send_with_auth("Account fetch failed", |token| {
                self.http
                    .get(&url)
                    .header("Authorization", format!("Bearer {}", token))
            })
            .await?;

        if !resp.status().is_success() {
            return Err(response_error("Account fetch failed", resp).await);
        }

        resp.json()
            .await
            .map_err(|e| ApiError::network("Account parse failed", e))
    }

    pub async fn accept_eula(&self, version: &str) -> Result<(), ApiError> {
        let url = format!("{}/auth/accept-eula", self.base_url);
        let body = serde_json::json!({ "version": version });
        let resp = self
            .send_with_auth("EULA acceptance failed", |token| {
                self.http
                    .post(&url)
                    .header("Authorization", format!("Bearer {}", token))
                    .json(&body)
            })
            .await?;

        if !resp.status().is_success() {
            return Err(response_error("EULA acceptance failed", resp).await);
        }

        Ok(())
    }

    pub async fn add_funds(&self, amount: f64) -> Result<String, ApiError> {
        let url = format!("{}/account/buy-credits", self.base_url);
        let body = serde_json::json!({ "amount": amount });
        let resp = self
            .send_with_auth("Add funds failed", |token| {
                self.http
                    .post(&url)
                    .header("Authorization", format!("Bearer {}", token))
                    .json(&body)
            })
            .await?;

        if !resp.status().is_success() {
            return Err(response_error("Add funds failed", resp).await);
        }

        let data: AddFundsResponse = resp
            .json()
            .await
            .map_err(|e| ApiError::network("Add funds parse failed", e))?;
        Ok(data.checkout_url)
    }

    pub async fn create_api_key(&self, name: &str) -> Result<CreateApiKeyResponse, ApiError> {
        let url = format!("{}/auth/api-keys", self.base_url);
        let body = serde_json::json!({ "name": name });
        let resp = self
            .send_with_auth("Create API key failed", |token| {
                self.http
                    .post(&url)
                    .header("Authorization", format!("Bearer {}", token))
                    .json(&body)
            })
            .await?;

        if !resp.status().is_success() {
            return Err(response_error("Create API key failed", resp).await);
        }

        resp.json()
            .await
            .map_err(|e| ApiError::network("Create API key parse failed", e))
    }

    pub async fn list_api_keys(&self) -> Result<Vec<ApiKeyInfo>, ApiError> {
        let url = format!("{}/auth/api-keys", self.base_url);
        let resp = self
            .send_with_auth("List API keys failed", |token| {
                self.http
                    .get(&url)
                    .header("Authorization", format!("Bearer {}", token))
            })
            .await?;

        if !resp.status().is_success() {
            return Err(response_error("List API keys failed", resp).await);
        }

        resp.json()
            .await
            .map_err(|e| ApiError::network("List API keys parse failed", e))
    }

    pub async fn revoke_api_key(&self, key_id: &str) -> Result<(), ApiError> {
        let url = format!("{}/auth/api-keys/{}", self.base_url, key_id);
        let resp = self
            .send_with_auth("Revoke API key failed", |token| {
                self.http
                    .delete(&url)
                    .header("Authorization", format!("Bearer {}", token))
            })
            .await?;

        if !resp.status().is_success() {
            return Err(response_error("Revoke API key failed", resp).await);
        }

        Ok(())
    }

    pub async fn get_credentials(&self) -> Result<CredentialsResponse, ApiError> {
        let url = format!("{}/auth/credentials", self.base_url);
        let resp = self
            .send_with_auth("Credentials fetch failed", |token| {
                self.http
                    .post(&url)
                    .header("Authorization", format!("Bearer {}", token))
            })
            .await?;

        if !resp.status().is_success() {
            return Err(response_error("Credentials fetch failed", resp).await);
        }

        resp.json()
            .await
            .map_err(|e| ApiError::network("Credentials parse failed", e))
    }

    // -------------------------------------------------------------------
    // Workspace run registry (discover / sync / model lock)
    // -------------------------------------------------------------------

    pub async fn acquire_run_lock(
        &self,
        workspace: &str,
        command: &str,
        pipeline: Option<&str>,
        run_id: Option<&str>,
    ) -> Result<AcquireLockResponse, ApiError> {
        let url = format!(
            "{}/auth/workspaces/{}/runs/lock/acquire",
            self.base_url, workspace
        );
        let body = serde_json::json!({
            "command": command,
            "pipeline": pipeline,
            "runId": run_id,
        });
        let resp = self
            .send_with_auth("Acquire run lock failed", |token| {
                self.http
                    .post(&url)
                    .header("Authorization", format!("Bearer {}", token))
                    .json(&body)
            })
            .await?;
        if resp.status() == StatusCode::CONFLICT {
            return Err(response_error("Acquire run lock failed", resp).await);
        }
        if !resp.status().is_success() {
            return Err(response_error("Acquire run lock failed", resp).await);
        }
        resp.json()
            .await
            .map_err(|e| ApiError::network("Acquire run lock parse failed", e))
    }

    pub async fn heartbeat_run_lock(
        &self,
        workspace: &str,
        run_id: &str,
        version: i64,
    ) -> Result<AcquireLockResponse, ApiError> {
        let url = format!(
            "{}/auth/workspaces/{}/runs/lock/heartbeat",
            self.base_url, workspace
        );
        let body = serde_json::json!({ "runId": run_id, "version": version });
        let resp = self
            .send_with_auth("Run lock heartbeat failed", |token| {
                self.http
                    .post(&url)
                    .header("Authorization", format!("Bearer {}", token))
                    .json(&body)
            })
            .await?;
        if !resp.status().is_success() {
            return Err(response_error("Run lock heartbeat failed", resp).await);
        }
        resp.json()
            .await
            .map_err(|e| ApiError::network("Run lock heartbeat parse failed", e))
    }

    pub async fn complete_run_lock(
        &self,
        workspace: &str,
        run_id: &str,
        version: i64,
        status: &str,
    ) -> Result<(), ApiError> {
        let url = format!(
            "{}/auth/workspaces/{}/runs/lock/complete",
            self.base_url, workspace
        );
        let body = serde_json::json!({
            "runId": run_id,
            "version": version,
            "status": status,
        });
        let resp = self
            .send_with_auth("Complete run lock failed", |token| {
                self.http
                    .post(&url)
                    .header("Authorization", format!("Bearer {}", token))
                    .json(&body)
            })
            .await?;
        if !resp.status().is_success() {
            return Err(response_error("Complete run lock failed", resp).await);
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub async fn cancel_run(&self, workspace: &str, run_id: &str) -> Result<(), ApiError> {
        let url = format!(
            "{}/auth/workspaces/{}/runs/{}/cancel",
            self.base_url, workspace, run_id
        );
        let resp = self
            .send_with_auth("Cancel run failed", |token| {
                self.http
                    .post(&url)
                    .header("Authorization", format!("Bearer {}", token))
            })
            .await?;
        if !resp.status().is_success() {
            return Err(response_error("Cancel run failed", resp).await);
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub async fn get_run_lock(&self, workspace: &str) -> Result<RunLockStatusResponse, ApiError> {
        let url = format!("{}/auth/workspaces/{}/runs/lock", self.base_url, workspace);
        let resp = self
            .send_with_auth("Get run lock failed", |token| {
                self.http
                    .get(&url)
                    .header("Authorization", format!("Bearer {}", token))
            })
            .await?;
        if !resp.status().is_success() {
            return Err(response_error("Get run lock failed", resp).await);
        }
        resp.json()
            .await
            .map_err(|e| ApiError::network("Get run lock parse failed", e))
    }

    #[allow(dead_code)]
    pub async fn put_run_record(
        &self,
        workspace: &str,
        run_id: &str,
        body: &serde_json::Value,
    ) -> Result<(), ApiError> {
        let url = format!(
            "{}/auth/workspaces/{}/runs/{}",
            self.base_url, workspace, run_id
        );
        let resp = self
            .send_with_auth("Put run record failed", |token| {
                self.http
                    .put(&url)
                    .header("Authorization", format!("Bearer {}", token))
                    .json(body)
            })
            .await?;
        if !resp.status().is_success() {
            return Err(response_error("Put run record failed", resp).await);
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub async fn list_runs(&self, workspace: &str) -> Result<ListRunsResponse, ApiError> {
        let url = format!("{}/auth/workspaces/{}/runs", self.base_url, workspace);
        let resp = self
            .send_with_auth("List runs failed", |token| {
                self.http
                    .get(&url)
                    .header("Authorization", format!("Bearer {}", token))
            })
            .await?;
        if !resp.status().is_success() {
            return Err(response_error("List runs failed", resp).await);
        }
        resp.json()
            .await
            .map_err(|e| ApiError::network("List runs parse failed", e))
    }

    #[allow(dead_code)]
    pub async fn get_run_record(
        &self,
        workspace: &str,
        run_id: &str,
    ) -> Result<RunRecordResponse, ApiError> {
        let url = format!(
            "{}/auth/workspaces/{}/runs/{}",
            self.base_url, workspace, run_id
        );
        let resp = self
            .send_with_auth("Get run record failed", |token| {
                self.http
                    .get(&url)
                    .header("Authorization", format!("Bearer {}", token))
            })
            .await?;
        if !resp.status().is_success() {
            return Err(response_error("Get run record failed", resp).await);
        }
        resp.json()
            .await
            .map_err(|e| ApiError::network("Get run record parse failed", e))
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AcquireLockResponse {
    pub run_id: String,
    pub version: i64,
    pub lease_expires_at: String,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunLockStatus {
    pub run_id: String,
    pub command: String,
    #[serde(default)]
    pub pipeline: Option<String>,
    pub status: String,
    pub version: i64,
    pub cancel_requested: bool,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunLockStatusResponse {
    pub lock: Option<RunLockStatus>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListRunsResponse {
    pub runs: Vec<RunSummary>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunSummary {
    pub run_id: String,
    pub command: String,
    #[serde(default)]
    pub pipeline: Option<String>,
    pub status: String,
    pub started_at: String,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRecordResponse {
    pub run_id: String,
    pub command: String,
    #[serde(default)]
    pub pipeline: Option<String>,
    pub status: String,
    #[serde(default)]
    pub payload: Option<serde_json::Value>,
}

async fn response_error(context: &str, resp: reqwest::Response) -> ApiError {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    ApiError::response(context, status, body)
}

#[derive(Debug, Deserialize)]
pub struct CredentialsResponse {
    pub credentials: StsCreds,
    pub bucket: String,
    pub tenant_id: String,
    pub llm_api_key: String,
    pub accounting_url: String,
    /// STS for read-only access to `public_vectors_bucket` / `skippr-docs/*` (when returned by auth).
    #[serde(default)]
    pub knowledge_credentials: Option<StsCreds>,
    #[serde(default)]
    pub public_vectors_bucket: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct StsCreds {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: String,
    pub expiration: String,
}

#[derive(Debug, Deserialize)]
struct TokenExchangeResponse {
    pub token: String,
    pub refresh_token: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CreateApiKeyResponse {
    pub key_id: String,
    pub raw_key: String,
    pub name: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ApiKeyInfo {
    pub key_id: String,
    pub name: String,
    pub created_at: String,
    pub status: String,
}
