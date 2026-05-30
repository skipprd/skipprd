//! Minimal OpenAI Chat Completions client for runtime source plugins.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::retry::{RetryConfig, RetryDecision, RetryableHttpClient};

const DEFAULT_BASE_URL: &str = "https://api.openai.com";
const DEFAULT_TIMEOUT_SECS: u64 = 120;

#[derive(Debug, thiserror::Error)]
pub enum OpenAiError {
    #[error("OPENAI_API_KEY is not set")]
    MissingApiKey,
    #[error("HTTP request failed: {0}")]
    Http(String),
    #[error("OpenAI API error ({status}): {body}")]
    Api { status: u16, body: String },
    #[error("response missing choices[0].message.content")]
    MissingContent,
    #[error("failed to parse JSON content: {0}")]
    JsonParse(String),
}

#[derive(Clone)]
pub struct OpenAiChatClient {
    http: RetryableHttpClient,
    api_key: String,
    base_url: String,
}

impl OpenAiChatClient {
    pub fn from_env() -> Result<Self, OpenAiError> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .map(|k| k.trim().to_string())
            .map_err(|_| OpenAiError::MissingApiKey)?;
        if api_key.is_empty() {
            return Err(OpenAiError::MissingApiKey);
        }
        let base_url = std::env::var("OPENAI_BASE_URL")
            .ok()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        Ok(Self::new(api_key, base_url, RetryConfig::default()))
    }

    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>, retry: RetryConfig) -> Self {
        Self {
            http: RetryableHttpClient::new(retry),
            api_key: api_key.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
        }
    }

    pub fn with_http_client(
        http: RetryableHttpClient,
        api_key: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            http,
            api_key: api_key.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
        }
    }

    pub async fn chat_json_object(
        &self,
        system: &str,
        user: &str,
        model: &str,
        timeout: Duration,
    ) -> Result<Value, OpenAiError> {
        let body = json!({
            "model": model,
            "response_format": { "type": "json_object" },
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user }
            ]
        });
        self.chat_completions(body, timeout).await
    }

    pub async fn chat_json_schema(
        &self,
        system: &str,
        user: &str,
        model: &str,
        schema_name: &str,
        schema: Value,
        timeout: Duration,
    ) -> Result<Value, OpenAiError> {
        let body = json!({
            "model": model,
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": schema_name,
                    "strict": true,
                    "schema": schema
                }
            },
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user }
            ]
        });
        self.chat_completions(body, timeout).await
    }

    async fn chat_completions(&self, body: Value, timeout: Duration) -> Result<Value, OpenAiError> {
        let url = format!("{}/v1/chat/completions", self.base_url);
        let response = self
            .post_json_with_retry(&url, body, timeout)
            .await?;
        let content = response
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .ok_or(OpenAiError::MissingContent)?;
        serde_json::from_str(content).map_err(|e| OpenAiError::JsonParse(e.to_string()))
    }

    async fn post_json_with_retry(
        &self,
        url: &str,
        body: Value,
        timeout: Duration,
    ) -> Result<Value, OpenAiError> {
        let max_attempts = self.http.config.max_attempts.max(1);
        let mut attempt = 0u32;
        loop {
            let request = self
                .http
                .client
                .post(url)
                .header("Authorization", format!("Bearer {}", self.api_key))
                .header("Content-Type", "application/json")
                .timeout(timeout)
                .json(&body);
            let response = request
                .send()
                .await
                .map_err(|e| OpenAiError::Http(e.to_string()))?;
            let status = response.status();
            if status.is_success() {
                return response
                    .json::<Value>()
                    .await
                    .map_err(|e| OpenAiError::Http(e.to_string()));
            }
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());
            let body_text = response.text().await.unwrap_or_default();
            match RetryableHttpClient::classify_status(status, retry_after) {
                RetryDecision::Success => unreachable!(),
                RetryDecision::GiveUp => {
                    return Err(OpenAiError::Api {
                        status: status.as_u16(),
                        body: body_text,
                    });
                }
                RetryDecision::RetryAfter(delay) => {
                    attempt += 1;
                    if attempt >= max_attempts {
                        return Err(OpenAiError::Api {
                            status: status.as_u16(),
                            body: body_text,
                        });
                    }
                    self.http.backoff(attempt, delay).await;
                }
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatCompletionFixture {
    pub content: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    static MOCK_PORT: std::sync::OnceLock<u16> = std::sync::OnceLock::new();

    async fn spawn_mock_server(response_body: &'static str, call_counter: Arc<AtomicUsize>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let counter = call_counter.clone();
                let body = response_body;
                tokio::spawn(async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    let mut buf = vec![0u8; 8192];
                    let _ = socket.read(&mut buf).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        port
    }

    #[tokio::test]
    async fn chat_json_object_parses_content() {
        let calls = Arc::new(AtomicUsize::new(0));
        let api_body = r#"{"choices":[{"message":{"content":"{\"ok\":true}"}}]}"#;
        let port = spawn_mock_server(api_body, calls.clone()).await;
        let client = OpenAiChatClient::with_http_client(
            RetryableHttpClient::new(RetryConfig {
                max_attempts: 1,
                ..RetryConfig::default()
            }),
            "test-key",
            format!("http://127.0.0.1:{port}"),
        );
        let value = client
            .chat_json_object("sys", "user", "gpt-test", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retries_on_rate_limit() {
        static ATTEMPT: AtomicUsize = AtomicUsize::new(0);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let n = ATTEMPT.fetch_add(1, Ordering::SeqCst);
                    let (status, body) = if n == 0 {
                        ("429 Too Many Requests", "{}")
                    } else {
                        (
                            "200 OK",
                            r#"{"choices":[{"message":{"content":"{\"retried\":true}"}}]}"#,
                        )
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    let mut buf = vec![0u8; 8192];
                    let _ = socket.read(&mut buf).await;
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        let client = OpenAiChatClient::with_http_client(
            RetryableHttpClient::new(RetryConfig {
                max_attempts: 3,
                initial_backoff_ms: 10,
                max_backoff_ms: 50,
            }),
            "test-key",
            format!("http://127.0.0.1:{port}"),
        );
        let value = client
            .chat_json_object("s", "u", "m", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(value["retried"], true);
        assert!(ATTEMPT.load(Ordering::SeqCst) >= 2);
    }
}
