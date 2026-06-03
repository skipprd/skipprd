use regex::Regex;
use std::sync::LazyLock;

static AWS_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"AKIA[0-9A-Z]{16}").expect("aws key regex"));
static GITHUB_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"ghp_[a-zA-Z0-9]{20,}").expect("github regex"));
static GENERIC_API: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(api[_-]?key|apikey|secret[_-]?key)\s*[:=]\s*['"]?[a-zA-Z0-9_\-]{16,}"#)
        .expect("api regex")
});

#[derive(Debug, Clone, Default)]
pub struct SecretScan {
    pub aws_key: bool,
    pub github_token: bool,
    pub generic_api: bool,
}

pub fn scan_body(snippet: Option<&str>) -> SecretScan {
    let Some(body) = snippet else {
        return SecretScan::default();
    };
    if body.is_empty() {
        return SecretScan::default();
    }
    SecretScan {
        aws_key: AWS_KEY.is_match(body),
        github_token: GITHUB_TOKEN.is_match(body),
        generic_api: GENERIC_API.is_match(body),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_aws_key_pattern() {
        let s = scan_body(Some("config AKIAIOSFODNN7EXAMPLE here"));
        assert!(s.aws_key);
    }

    #[test]
    fn empty_snippet_is_clean() {
        assert!(!scan_body(None).aws_key);
        assert!(!scan_body(Some("")).github_token);
    }
}
