/// Normalize a domain or page target per DataForSEO Backlinks API rules.
pub fn normalize_target(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("target must not be empty".into());
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        let without_scheme = trimmed
            .split("://")
            .nth(1)
            .unwrap_or(trimmed)
            .trim_end_matches('/');
        let (authority, path) = match without_scheme.split_once('/') {
            Some((host, rest)) if !rest.is_empty() => (host, format!("/{rest}")),
            Some((host, _)) => (host, String::new()),
            None => (without_scheme, String::new()),
        };
        let domain = authority
            .strip_prefix("www.")
            .unwrap_or(authority)
            .to_string();
        if path.is_empty() || path == "/" {
            return Ok(domain);
        }
        return Ok(trimmed.trim_end_matches('/').to_string());
    }
    let mut domain = trimmed.to_string();
    if let Some(rest) = domain.strip_prefix("www.") {
        domain = rest.to_string();
    }
    domain = domain.trim_end_matches('/').to_string();
    if domain.is_empty() {
        return Err("target must not be empty after normalization".into());
    }
    Ok(domain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_domain_target() {
        assert_eq!(
            normalize_target("https://www.example.com/").unwrap(),
            "example.com"
        );
        assert_eq!(
            normalize_target("http://example.com").unwrap(),
            "example.com"
        );
        assert_eq!(normalize_target("example.com").unwrap(), "example.com");
    }

    #[test]
    fn normalize_page_url_keeps_scheme() {
        assert_eq!(
            normalize_target("https://example.com/blog/post").unwrap(),
            "https://example.com/blog/post"
        );
    }

    #[test]
    fn empty_target_fails() {
        assert!(normalize_target("  ").is_err());
    }
}
