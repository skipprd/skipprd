use std::time::Duration;
use tokio::time::sleep;

#[derive(Clone, Debug)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub initial_backoff_ms: u64,
    pub max_backoff_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            initial_backoff_ms: 500,
            max_backoff_ms: 30_000,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryDecision {
    RetryAfter(Duration),
    GiveUp,
    Success,
}

pub struct RetryableHttpClient {
    pub client: reqwest::Client,
    pub config: RetryConfig,
}

impl RetryableHttpClient {
    pub fn new(config: RetryConfig) -> Self {
        Self {
            client: reqwest::Client::new(),
            config,
        }
    }

    pub fn classify_status(status: reqwest::StatusCode, retry_after: Option<u64>) -> RetryDecision {
        if status.is_success() {
            return RetryDecision::Success;
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            let delay = retry_after
                .map(Duration::from_secs)
                .unwrap_or_else(|| Duration::from_millis(1000));
            return RetryDecision::RetryAfter(delay);
        }
        RetryDecision::GiveUp
    }

    pub async fn backoff(&self, attempt: u32, suggested: Duration) {
        let exp = self
            .config
            .initial_backoff_ms
            .saturating_mul(1 << attempt.min(8));
        let capped = exp.min(self.config.max_backoff_ms);
        let delay = suggested.max(Duration::from_millis(capped));
        sleep(delay).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_retries() {
        assert_eq!(
            RetryableHttpClient::classify_status(reqwest::StatusCode::TOO_MANY_REQUESTS, Some(2)),
            RetryDecision::RetryAfter(Duration::from_secs(2))
        );
    }

    #[test]
    fn client_error_gives_up() {
        assert_eq!(
            RetryableHttpClient::classify_status(reqwest::StatusCode::BAD_REQUEST, None),
            RetryDecision::GiveUp
        );
    }
}
