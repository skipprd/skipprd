use crate::response_debug::{
    body_debug_suffix, log_api_response_issue, parse_json_response, truncate_response_body,
};
use crate::retry::{RetryConfig, RetryDecision, RetryableHttpClient};
use tracing::warn;

const DEFAULT_OPENAI_BASE: &str = "https://api.openai.com/v1";

pub type OpenAiError = std::io::Error;

#[derive(Clone)]
pub struct OpenAiChatClient {
    http: RetryableHttpClient,
    api_key: String,
    base_url: String,
}

impl OpenAiChatClient {
    pub fn from_env() -> Result<Self, std::io::Error> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "OPENAI_API_KEY is required for OpenAI requests",
                )
            })?;
        let base_url = std::env::var("OPENAI_BASE_URL")
            .ok()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_OPENAI_BASE.to_string());
        Ok(Self::new(api_key, base_url))
    }

    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            http: RetryableHttpClient::new(RetryConfig::default()),
            api_key: api_key.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
        }
    }

    pub async fn chat_json_object(
        &self,
        model: &str,
        system: &str,
        user: &str,
    ) -> Result<serde_json::Value, std::io::Error> {
        if let Ok(dir) = std::env::var("SKIPPR_OPENAI_FIXTURE_DIR") {
            if let Some(body) = load_openai_fixture(&dir, user) {
                return Ok(body);
            }
        }

        let url = format!("{}/chat/completions", self.base_url);
        let body = serde_json::json!({
            "model": model,
            "response_format": { "type": "json_object" },
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user }
            ]
        });

        let mut attempt = 0u32;
        loop {
            let response = self
                .http
                .client
                .post(&url)
                .header("Authorization", format!("Bearer {}", self.api_key))
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
                RetryDecision::Success => {
                    let text = response
                        .text()
                        .await
                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                    let envelope = parse_json_response(
                        "OpenAI",
                        &url,
                        "chat_completions",
                        Some(status.as_u16()),
                        &text,
                    )?;
                    return parse_chat_json_content(&envelope, &text);
                }
                RetryDecision::RetryAfter(delay) => {
                    attempt += 1;
                    if attempt >= self.http.config.max_attempts {
                        let text = response.text().await.unwrap_or_default();
                        log_api_response_issue(
                            "openai",
                            &url,
                            "retry_exhausted",
                            Some(status.as_u16()),
                            &text,
                        );
                        return Err(std::io::Error::other(format!(
                            "OpenAI request failed after {attempt} attempts: HTTP {status} ({})",
                            body_debug_suffix(&text, 300)
                        )));
                    }
                    warn!(attempt, %status, "OpenAI transient error; backing off");
                    self.http.backoff(attempt, delay).await;
                }
                RetryDecision::GiveUp => {
                    let text = response.text().await.unwrap_or_default();
                    log_api_response_issue(
                        "openai",
                        &url,
                        "http_error",
                        Some(status.as_u16()),
                        &text,
                    );
                    return Err(std::io::Error::other(format!(
                        "OpenAI request failed: HTTP {status} {} ({})",
                        truncate_response_body(&text, 500),
                        body_debug_suffix(&text, 300)
                    )));
                }
            }
        }
    }
}

fn parse_chat_json_content(
    envelope: &serde_json::Value,
    raw_body: &str,
) -> Result<serde_json::Value, std::io::Error> {
    let content = envelope
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            log_api_response_issue(
                "openai",
                "/v1/chat/completions",
                "missing_message_content",
                Some(200),
                raw_body,
            );
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "OpenAI response missing choices[0].message.content ({})",
                    body_debug_suffix(raw_body, 300)
                ),
            )
        })?;
    serde_json::from_str(content).map_err(|e| {
        log_api_response_issue(
            "openai",
            "/v1/chat/completions",
            "invalid_message_json",
            Some(200),
            content,
        );
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "OpenAI content is not valid JSON: {e} ({})",
                body_debug_suffix(content, 300)
            ),
        )
    })
}

fn load_openai_fixture(dir: &str, user: &str) -> Option<serde_json::Value> {
    let slug = user
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(32)
        .collect::<String>();
    let path = format!("{}/openai_{slug}.json", dir.trim_end_matches('/'));
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_chat_json_content_extracts_object() {
        let envelope = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "{\"score\": 0.9}"
                }
            }]
        });
        let parsed = parse_chat_json_content(
            &envelope,
            r#"{"choices":[{"message":{"content":"{\"score\": 0.9}"}}]}"#,
        )
        .unwrap();
        assert_eq!(parsed["score"], 0.9);
    }
}
