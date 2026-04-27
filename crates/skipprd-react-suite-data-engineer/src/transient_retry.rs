use std::future::Future;

const DEFAULT_MAX_RETRIES: usize = 2;
const BACKOFF_BASE_MS: u64 = 500;

/// Retry an async fallible operation when the error is a transient infrastructure issue.
///
/// The operation `f` is called once. If it returns `Err` and the error text is classified
/// as transient by [`crate::failure_text::is_infra_transient`], it is retried up to
/// `max_retries` additional times with exponential backoff (500ms, 1s, 2s, ...).
///
/// Non-transient errors are returned immediately without retry.
///
/// `label` is used in warning logs to identify the call site.
pub async fn retry_transient<F, Fut, T>(label: &str, max_retries: usize, f: F) -> Result<T, String>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, String>>,
{
    let mut last_err;
    match f().await {
        Ok(v) => return Ok(v),
        Err(e) => last_err = e,
    }

    for attempt in 1..=max_retries {
        if !crate::failure_text::is_infra_transient(&crate::failure_text::normalize_text(&last_err))
        {
            return Err(last_err);
        }
        let backoff_ms = BACKOFF_BASE_MS * (1u64 << (attempt - 1).min(4));
        tracing::warn!(
            label,
            attempt,
            backoff_ms,
            error = %last_err,
            "transient infra error; retrying after backoff"
        );
        tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

/// Convenience wrapper using the default retry count (2 retries = 3 total attempts).
pub async fn retry_transient_default<F, Fut, T>(label: &str, f: F) -> Result<T, String>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, String>>,
{
    retry_transient(label, DEFAULT_MAX_RETRIES, f).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn succeeds_on_first_try() {
        let calls = AtomicUsize::new(0);
        let result = retry_transient("test", 2, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Ok::<_, String>(42) }
        })
        .await;
        assert_eq!(result.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retries_transient_then_succeeds() {
        let calls = AtomicUsize::new(0);
        let result = retry_transient("test", 2, || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if n == 0 {
                    Err("service error".to_string())
                } else {
                    Ok(99)
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 99);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn does_not_retry_non_transient() {
        let calls = AtomicUsize::new(0);
        let result = retry_transient("test", 2, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err::<i32, _>("syntax error at or near SELECT".to_string()) }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn exhausts_retries_on_persistent_transient() {
        let calls = AtomicUsize::new(0);
        let result = retry_transient("test", 2, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err::<i32, _>("service error".to_string()) }
        })
        .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("service error"));
        assert_eq!(calls.load(Ordering::SeqCst), 3); // 1 initial + 2 retries
    }
}
