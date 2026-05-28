use std::fs;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::Deserialize;

use crate::retry::{RetryConfig, RetryDecision, RetryableHttpClient};

pub trait BearerAuth: Send + Sync {
    fn authorization_header(&self) -> Result<String, String>;
}

#[derive(Clone)]
pub struct StaticBearerAuth {
    token: String,
}

impl StaticBearerAuth {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
        }
    }
}

impl BearerAuth for StaticBearerAuth {
    fn authorization_header(&self) -> Result<String, String> {
        Ok(format!("Bearer {}", self.token))
    }
}

#[derive(Clone, Debug)]
struct CachedOAuthToken {
    access_token: String,
    expires_at: SystemTime,
}

/// Default `expires_in` when the token response omits it (Google typically uses 3600s).
const DEFAULT_OAUTH_EXPIRES_IN_SECS: u64 = 3600;

/// Refresh this long before `expires_at` so in-flight API calls do not race expiry.
const DEFAULT_OAUTH_REFRESH_AHEAD: Duration = Duration::from_secs(60);

fn oauth_token_still_valid(entry: &CachedOAuthToken, refresh_ahead: Duration) -> bool {
    entry
        .expires_at
        .checked_sub(refresh_ahead)
        .is_some_and(|refresh_at| SystemTime::now() < refresh_at)
}

fn oauth_expires_at_from_response(expires_in_secs: u64) -> SystemTime {
    SystemTime::now() + Duration::from_secs(expires_in_secs)
}

fn parse_oauth_expires_in_secs(body: &serde_json::Value) -> u64 {
    body.get("expires_in")
        .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|n| n.max(0) as u64)))
        .filter(|&secs| secs > 0)
        .unwrap_or(DEFAULT_OAUTH_EXPIRES_IN_SECS)
}

/// Refreshes an OAuth2 access token using a refresh token endpoint.
#[derive(Clone)]
pub struct OAuth2RefreshTokenAuth {
    client: reqwest::Client,
    token_url: String,
    client_id: String,
    client_secret: String,
    refresh_token: String,
    cached: Arc<RwLock<Option<CachedOAuthToken>>>,
    refresh_ahead: Duration,
    retry_config: RetryConfig,
}

impl OAuth2RefreshTokenAuth {
    pub fn new(
        token_url: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        refresh_token: impl Into<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            token_url: token_url.into(),
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            refresh_token: refresh_token.into(),
            cached: Arc::new(RwLock::new(None)),
            refresh_ahead: DEFAULT_OAUTH_REFRESH_AHEAD,
            retry_config: RetryConfig::default(),
        }
    }

    /// Returns a valid access token, reusing the cache until shortly before expiry.
    pub async fn refresh(&self) -> Result<String, String> {
        if let Ok(guard) = self.cached.read() {
            if let Some(entry) = guard.as_ref() {
                if oauth_token_still_valid(entry, self.refresh_ahead) {
                    return Ok(entry.access_token.clone());
                }
            }
        }

        let entry = self.fetch_access_token().await?;
        let token = entry.access_token.clone();
        if let Ok(mut guard) = self.cached.write() {
            *guard = Some(entry);
        }
        Ok(token)
    }

    async fn fetch_access_token(&self) -> Result<CachedOAuthToken, String> {
        let retryable = RetryableHttpClient::new(self.retry_config.clone());
        let mut attempt = 0u32;

        loop {
            let response = self
                .client
                .post(&self.token_url)
                .form(&[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", self.refresh_token.as_str()),
                    ("client_id", self.client_id.as_str()),
                    ("client_secret", self.client_secret.as_str()),
                ])
                .send()
                .await
                .map_err(|e| e.to_string())?;

            let status = response.status();
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());

            match RetryableHttpClient::classify_status(status, retry_after) {
                RetryDecision::Success => {
                    let body: serde_json::Value =
                        response.json().await.map_err(|e| e.to_string())?;
                    let access_token = body
                        .get("access_token")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .ok_or_else(|| "token response missing access_token".to_string())?;
                    let expires_at =
                        oauth_expires_at_from_response(parse_oauth_expires_in_secs(&body));
                    return Ok(CachedOAuthToken {
                        access_token,
                        expires_at,
                    });
                }
                RetryDecision::RetryAfter(delay) => {
                    attempt += 1;
                    if attempt >= retryable.config.max_attempts {
                        return Err(format!(
                            "token refresh failed after {} attempts: HTTP {}",
                            attempt, status
                        ));
                    }
                    tracing::debug!(
                        attempt,
                        status = %status,
                        "OAuth token refresh retrying after transient error"
                    );
                    retryable.backoff(attempt, delay).await;
                }
                RetryDecision::GiveUp => {
                    return Err(format!("token refresh failed: {}", status));
                }
            }
        }
    }
}

impl BearerAuth for OAuth2RefreshTokenAuth {
    fn authorization_header(&self) -> Result<String, String> {
        if let Ok(guard) = self.cached.read() {
            if let Some(entry) = guard.as_ref() {
                if oauth_token_still_valid(entry, self.refresh_ahead) {
                    return Ok(format!("Bearer {}", entry.access_token));
                }
            }
        }
        Err(
            "OAuth2RefreshTokenAuth requires async refresh — call refresh().await before requests"
                .into(),
        )
    }
}

#[derive(Debug, Deserialize)]
struct ServiceAccountKeyFile {
    client_email: String,
    private_key: String,
    token_uri: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct ServiceAccountClaims<'a> {
    iss: &'a str,
    scope: &'a str,
    aud: &'a str,
    iat: u64,
    exp: u64,
}

/// Google service-account JSON credentials with cached OAuth access tokens.
#[derive(Clone)]
pub struct ServiceAccountAuth {
    client: reqwest::Client,
    client_email: String,
    private_key: String,
    token_uri: String,
    scope: String,
    cached: Arc<RwLock<Option<(String, SystemTime)>>>,
}

impl ServiceAccountAuth {
    pub fn from_json_path(path: &str, scope: impl Into<String>) -> Result<Self, String> {
        let bytes = fs::read(path).map_err(|e| e.to_string())?;
        Self::from_json_bytes(&bytes, scope)
    }

    pub fn from_json_bytes(bytes: &[u8], scope: impl Into<String>) -> Result<Self, String> {
        let key: ServiceAccountKeyFile =
            serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        Ok(Self {
            client: reqwest::Client::new(),
            client_email: key.client_email,
            private_key: key.private_key,
            token_uri: key
                .token_uri
                .unwrap_or_else(|| "https://oauth2.googleapis.com/token".to_string()),
            scope: scope.into(),
            cached: Arc::new(RwLock::new(None)),
        })
    }

    pub async fn access_token(&self) -> Result<String, String> {
        if let Ok(guard) = self.cached.read() {
            if let Some((token, fetched_at)) = guard.as_ref() {
                if fetched_at.elapsed().unwrap_or(Duration::MAX) < Duration::from_secs(3000) {
                    return Ok(token.clone());
                }
            }
        }
        let token = self.fetch_access_token().await?;
        if let Ok(mut guard) = self.cached.write() {
            *guard = Some((token.clone(), SystemTime::now()));
        }
        Ok(token)
    }

    async fn fetch_access_token(&self) -> Result<String, String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_secs();
        let claims = ServiceAccountClaims {
            iss: &self.client_email,
            scope: &self.scope,
            aud: &self.token_uri,
            iat: now,
            exp: now + 3600,
        };
        let jwt = encode(
            &Header::new(Algorithm::RS256),
            &claims,
            &EncodingKey::from_rsa_pem(self.private_key.as_bytes()).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        let response = self
            .client
            .post(&self.token_uri)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", jwt.as_str()),
            ])
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("service account token exchange failed: {}", response.status()));
        }
        let body: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;
        body.get("access_token")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| "token response missing access_token".to_string())
    }
}

impl BearerAuth for ServiceAccountAuth {
    fn authorization_header(&self) -> Result<String, String> {
        Err("ServiceAccountAuth requires async access_token().await before requests".into())
    }
}

const APPLE_ADS_TOKEN_URL: &str = "https://appleid.apple.com/auth/oauth2/token";
const APPLE_ADS_JWT_AUD: &str = "https://appleid.apple.com";
const APPLE_ADS_JWT_TTL_SECS: u64 = 20 * 60;

#[derive(Debug, serde::Serialize)]
struct AppleAdsClientSecretClaims<'a> {
    iss: &'a str,
    sub: String,
    aud: &'a str,
    iat: u64,
    exp: u64,
}

/// Apple Search Ads OAuth2 client-credentials (ES256 JWT client secret).
#[derive(Clone)]
pub struct AppleAdsClientCredentialsAuth {
    client: reqwest::Client,
    client_id: String,
    team_id: String,
    key_id: String,
    private_key_pem: String,
    cached: Arc<RwLock<Option<CachedOAuthToken>>>,
    refresh_ahead: Duration,
    retry_config: RetryConfig,
}

impl AppleAdsClientCredentialsAuth {
    pub fn new(
        client_id: impl Into<String>,
        team_id: impl Into<String>,
        key_id: impl Into<String>,
        private_key_pem: impl Into<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            client_id: client_id.into(),
            team_id: team_id.into(),
            key_id: key_id.into(),
            private_key_pem: private_key_pem.into(),
            cached: Arc::new(RwLock::new(None)),
            refresh_ahead: Duration::from_secs(300),
            retry_config: RetryConfig::default(),
        }
    }

    pub fn from_private_key_path(
        client_id: impl Into<String>,
        team_id: impl Into<String>,
        key_id: impl Into<String>,
        path: &str,
    ) -> Result<Self, String> {
        let pem = fs::read_to_string(path).map_err(|e| e.to_string())?;
        Ok(Self::new(client_id, team_id, key_id, pem))
    }

    pub fn from_private_key_pem(
        client_id: impl Into<String>,
        team_id: impl Into<String>,
        key_id: impl Into<String>,
        pem_bytes: &[u8],
    ) -> Result<Self, String> {
        let pem = std::str::from_utf8(pem_bytes)
            .map_err(|e| e.to_string())?
            .to_string();
        Ok(Self::new(client_id, team_id, key_id, pem))
    }

    pub fn searchads_subject(&self) -> String {
        format!("SEARCHADS.{}", self.team_id.trim())
    }

    pub fn build_client_secret_jwt(&self, now_secs: u64) -> Result<String, String> {
        let claims = AppleAdsClientSecretClaims {
            iss: &self.team_id,
            sub: self.searchads_subject(),
            aud: APPLE_ADS_JWT_AUD,
            iat: now_secs,
            exp: now_secs + APPLE_ADS_JWT_TTL_SECS,
        };
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(self.key_id.clone());
        encode(
            &header,
            &claims,
            &EncodingKey::from_ec_pem(self.private_key_pem.as_bytes()).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())
    }

    pub async fn access_token(&self) -> Result<String, String> {
        if let Ok(guard) = self.cached.read() {
            if let Some(entry) = guard.as_ref() {
                if oauth_token_still_valid(entry, self.refresh_ahead) {
                    return Ok(entry.access_token.clone());
                }
            }
        }
        let entry = self.fetch_access_token().await?;
        let token = entry.access_token.clone();
        if let Ok(mut guard) = self.cached.write() {
            *guard = Some(entry);
        }
        Ok(token)
    }

    async fn fetch_access_token(&self) -> Result<CachedOAuthToken, String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_secs();
        let client_secret = self.build_client_secret_jwt(now)?;
        let retryable = RetryableHttpClient::new(self.retry_config.clone());
        let mut attempt = 0u32;

        loop {
            let response = self
                .client
                .post(APPLE_ADS_TOKEN_URL)
                .form(&[
                    ("grant_type", "client_credentials"),
                    ("client_id", self.client_id.as_str()),
                    ("client_secret", client_secret.as_str()),
                    ("scope", "searchadsorg"),
                ])
                .send()
                .await
                .map_err(|e| e.to_string())?;

            let status = response.status();
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());

            match RetryableHttpClient::classify_status(status, retry_after) {
                RetryDecision::Success => {
                    let body: serde_json::Value =
                        response.json().await.map_err(|e| e.to_string())?;
                    let access_token = body
                        .get("access_token")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .ok_or_else(|| "token response missing access_token".to_string())?;
                    let expires_at =
                        oauth_expires_at_from_response(parse_oauth_expires_in_secs(&body));
                    return Ok(CachedOAuthToken {
                        access_token,
                        expires_at,
                    });
                }
                RetryDecision::RetryAfter(delay) => {
                    attempt += 1;
                    if attempt >= retryable.config.max_attempts {
                        return Err(format!(
                            "Apple Ads token exchange failed after {attempt}/{} attempts: HTTP {status}",
                            retryable.config.max_attempts
                        ));
                    }
                    retryable.backoff(attempt, delay).await;
                }
                RetryDecision::GiveUp => {
                    return Err(format!("Apple Ads token exchange failed: {}", status));
                }
            }
        }
    }
}

impl BearerAuth for AppleAdsClientCredentialsAuth {
    fn authorization_header(&self) -> Result<String, String> {
        if let Ok(guard) = self.cached.read() {
            if let Some(entry) = guard.as_ref() {
                if oauth_token_still_valid(entry, self.refresh_ahead) {
                    return Ok(format!("Bearer {}", entry.access_token));
                }
            }
        }
        Err(
            "AppleAdsClientCredentialsAuth requires async access_token().await before requests"
                .into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_bearer_formats_header() {
        let auth = StaticBearerAuth::new("secret");
        assert_eq!(auth.authorization_header().unwrap(), "Bearer secret");
    }

    #[test]
    fn parse_oauth_expires_in_prefers_response_value() {
        let body = serde_json::json!({ "expires_in": 1800 });
        assert_eq!(parse_oauth_expires_in_secs(&body), 1800);
    }

    #[test]
    fn parse_oauth_expires_in_defaults_when_missing() {
        let body = serde_json::json!({ "access_token": "x" });
        assert_eq!(parse_oauth_expires_in_secs(&body), DEFAULT_OAUTH_EXPIRES_IN_SECS);
    }

    const TEST_EC_PRIVATE_KEY_PEM: &str =
        include_str!("../tests/fixtures/apple_ads_test_ec_private.pem");

    #[test]
    fn apple_ads_jwt_uses_searchads_subject() {
        let auth = AppleAdsClientCredentialsAuth::new(
            "CLIENT_ID",
            "TEAM123",
            "KEY456",
            TEST_EC_PRIVATE_KEY_PEM,
        );
        assert_eq!(auth.searchads_subject(), "SEARCHADS.TEAM123");
        let jwt = auth.build_client_secret_jwt(1_700_000_000).expect("sign jwt");
        assert_eq!(jwt.split('.').count(), 3);
    }

    #[test]
    fn oauth_token_still_valid_honours_refresh_ahead() {
        let entry = CachedOAuthToken {
            access_token: "tok".into(),
            expires_at: SystemTime::now() + Duration::from_secs(30),
        };
        assert!(!oauth_token_still_valid(&entry, Duration::from_secs(60)));
        assert!(oauth_token_still_valid(
            &CachedOAuthToken {
                expires_at: SystemTime::now() + Duration::from_secs(120),
                ..entry
            },
            Duration::from_secs(60),
        ));
    }

    #[test]
    fn apple_ads_jwt_builds_with_ec_pem() {
        let auth = AppleAdsClientCredentialsAuth::new(
            "client-id",
            "TEAM123",
            "KEY123",
            TEST_EC_PRIVATE_KEY_PEM,
        );
        let jwt = auth.build_client_secret_jwt(1_700_000_000).unwrap();
        assert!(jwt.contains('.'));
        let parts: Vec<_> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
    }
}
