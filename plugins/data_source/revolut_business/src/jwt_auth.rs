use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::Serialize;
use uuid::Uuid;

const CLIENT_ASSERTION_TYPE: &str = "urn:ietf:params:oauth:client-assertion-type:jwt-bearer";
const DEFAULT_JWT_TTL_SECS: u64 = 300;
const DEFAULT_REFRESH_AHEAD: Duration = Duration::from_secs(60);
const DEFAULT_OAUTH_EXPIRES_IN_SECS: u64 = 2400;

#[derive(Debug, Serialize)]
struct ClientAssertionClaims<'a> {
    iss: &'a str,
    sub: &'a str,
    aud: &'a str,
    iat: u64,
    exp: u64,
    jti: String,
}

#[derive(Clone, Debug)]
struct CachedAccessToken {
    access_token: String,
    expires_at: SystemTime,
}

/// Revolut Business OAuth: client-assertion JWT + refresh token (no client_secret).
#[derive(Clone)]
pub struct RevolutJwtAuth {
    client: reqwest::Client,
    token_url: String,
    client_id: String,
    private_key_pem: String,
    issuer_domain: String,
    refresh_token: String,
    cached: Arc<RwLock<Option<CachedAccessToken>>>,
}

impl RevolutJwtAuth {
    pub fn new(
        token_url: impl Into<String>,
        client_id: impl Into<String>,
        private_key_pem: impl Into<String>,
        issuer_domain: impl Into<String>,
        refresh_token: impl Into<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            token_url: token_url.into(),
            client_id: client_id.into(),
            private_key_pem: private_key_pem.into(),
            issuer_domain: issuer_domain.into(),
            refresh_token: refresh_token.into(),
            cached: Arc::new(RwLock::new(None)),
        }
    }

    pub fn build_client_assertion_jwt(&self, now_secs: u64) -> Result<String, String> {
        let claims = ClientAssertionClaims {
            iss: &self.issuer_domain,
            sub: &self.client_id,
            aud: &self.token_url,
            iat: now_secs,
            exp: now_secs + DEFAULT_JWT_TTL_SECS,
            jti: Uuid::new_v4().to_string(),
        };
        let encoding_key = EncodingKey::from_rsa_pem(self.private_key_pem.as_bytes())
            .map_err(|e| e.to_string())?;
        encode(&Header::new(Algorithm::RS256), &claims, &encoding_key).map_err(|e| e.to_string())
    }

    fn token_still_valid(entry: &CachedAccessToken) -> bool {
        entry
            .expires_at
            .checked_sub(DEFAULT_REFRESH_AHEAD)
            .is_some_and(|refresh_at| SystemTime::now() < refresh_at)
    }

    pub async fn access_token(&self) -> Result<String, String> {
        if let Ok(guard) = self.cached.read() {
            if let Some(entry) = guard.as_ref() {
                if Self::token_still_valid(entry) {
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

    async fn fetch_access_token(&self) -> Result<CachedAccessToken, String> {
        let now = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_secs();
        let client_assertion = self.build_client_assertion_jwt(now)?;
        let response = self
            .client
            .post(&self.token_url)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", self.refresh_token.as_str()),
                ("client_id", self.client_id.as_str()),
                ("client_assertion_type", CLIENT_ASSERTION_TYPE),
                ("client_assertion", client_assertion.as_str()),
            ])
            .send()
            .await
            .map_err(|e| e.to_string())?;

        let status = response.status();
        let text = response.text().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!(
                "Revolut token refresh failed HTTP {status}: {text}"
            ));
        }
        let body: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let access_token = body
            .get("access_token")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| "token response missing access_token".to_string())?;
        let expires_in = body
            .get("expires_in")
            .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|n| n.max(0) as u64)))
            .filter(|&secs| secs > 0)
            .unwrap_or(DEFAULT_OAUTH_EXPIRES_IN_SECS);
        let expires_at = SystemTime::now() + Duration::from_secs(expires_in);
        Ok(CachedAccessToken {
            access_token,
            expires_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_client_assertion_jwt() {
        let pem = include_str!("../tests/fixtures/test_private_key.pem");
        let auth = RevolutJwtAuth::new(
            "https://b2b.revolut.com/api/1.0/auth/token",
            "client-id",
            pem,
            "api.upfoundry.co",
            "refresh-token",
        );
        let jwt = auth.build_client_assertion_jwt(1_700_000_000).unwrap();
        assert_eq!(jwt.matches('.').count(), 2);
    }
}
