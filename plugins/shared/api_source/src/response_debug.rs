use serde_json::Value;
use tracing::warn;

pub const DEFAULT_BODY_PREVIEW_LEN: usize = 500;

pub fn truncate_response_body(body: &str, max: usize) -> String {
    if body.len() <= max {
        body.to_string()
    } else {
        format!("{}…", &body[..max])
    }
}

pub fn body_debug_suffix(body: &str, max: usize) -> String {
    format!(
        "body_len={}, preview={}",
        body.len(),
        truncate_response_body(body, max)
    )
}

pub fn log_api_response_issue(
    provider: &str,
    endpoint: &str,
    context: &str,
    http_status: Option<u16>,
    body: &str,
) {
    warn!(
        provider = %provider,
        endpoint = %endpoint,
        context = %context,
        http_status = http_status,
        body_len = body.len(),
        body_preview = %truncate_response_body(body, DEFAULT_BODY_PREVIEW_LEN),
        "External API response issue"
    );
}

pub fn log_api_task_issue(
    provider: &str,
    endpoint: &str,
    context: &str,
    task_status_code: i64,
    task_status_message: &str,
) {
    warn!(
        provider = %provider,
        endpoint = %endpoint,
        context = %context,
        task_status_code,
        task_status_message = %task_status_message,
        "External API response issue"
    );
}

pub fn parse_json_response(
    provider: &str,
    endpoint: &str,
    context: &str,
    http_status: Option<u16>,
    body: &str,
) -> Result<Value, std::io::Error> {
    if body.trim().is_empty() {
        log_api_response_issue(
            provider,
            endpoint,
            &format!("{context}_empty"),
            http_status,
            body,
        );
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{provider} response is empty ({})",
                body_debug_suffix(body, 300)
            ),
        ));
    }
    serde_json::from_str(body).map_err(|e| {
        log_api_response_issue(
            provider,
            endpoint,
            &format!("{context}_invalid_json"),
            http_status,
            body,
        );
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{provider} response is not JSON: {e} ({})",
                body_debug_suffix(body, 300)
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_response_body_shortens_long_payload() {
        assert_eq!(truncate_response_body("abcdef", 3), "abc…");
        assert_eq!(truncate_response_body("ab", 3), "ab");
    }

    #[test]
    fn parse_json_response_rejects_empty_body() {
        let err = parse_json_response("OpenAI", "/v1/chat", "chat", Some(200), "").unwrap_err();
        assert!(err.to_string().contains("body_len=0"));
    }

    #[test]
    fn parse_json_response_includes_preview_on_invalid_json() {
        let err =
            parse_json_response("DataForSEO", "/live", "live", Some(200), "not-json").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("body_len="));
        assert!(message.contains("preview=not-json"));
    }
}
