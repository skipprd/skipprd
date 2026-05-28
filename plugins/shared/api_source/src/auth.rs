use std::fs;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::Deserialize;

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

/// Refreshes an OAuth2 access token using a refresh token endpoint.
#[derive(Clone)]
pub struct OAuth2RefreshTokenAuth {
    client: reqwest::Client,
    token_url: String,
    client_id: String,
    client_secret: String,
    refresh_token: String,
    cached: Arc<RwLock<Option<String>>>,
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
        }
    }

    pub async fn refresh(&self) -> Result<String, String> {
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
        if !response.status().is_success() {
            return Err(format!("token refresh failed: {}", response.status()));
        }
        let body: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;
        let token = body
            .get("access_token")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| "token response missing access_token".to_string())?;
        if let Ok(mut guard) = self.cached.write() {
            *guard = Some(token.clone());
        }
        Ok(token)
    }
}

impl BearerAuth for OAuth2RefreshTokenAuth {
    fn authorization_header(&self) -> Result<String, String> {
        if let Ok(guard) = self.cached.read() {
            if let Some(token) = guard.clone() {
                return Ok(format!("Bearer {token}"));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_bearer_formats_header() {
        let auth = StaticBearerAuth::new("secret");
        assert_eq!(auth.authorization_header().unwrap(), "Bearer secret");
    }
}
