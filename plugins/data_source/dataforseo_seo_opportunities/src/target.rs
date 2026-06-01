/// Normalize a domain target for SEO opportunity analysis.
pub fn normalize_site(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("site must not be empty".into());
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        let without_scheme = trimmed
            .split("://")
            .nth(1)
            .unwrap_or(trimmed)
            .trim_end_matches('/');
        let authority = without_scheme.split('/').next().unwrap_or(without_scheme);
        let domain = authority
            .strip_prefix("www.")
            .unwrap_or(authority)
            .to_string();
        if domain.is_empty() {
            return Err("site must not be empty after normalization".into());
        }
        return Ok(domain);
    }
    let mut domain = trimmed.to_string();
    if let Some(rest) = domain.strip_prefix("www.") {
        domain = rest.to_string();
    }
    domain = domain.trim_end_matches('/').to_string();
    if domain.is_empty() {
        return Err("site must not be empty after normalization".into());
    }
    Ok(domain)
}

pub fn domain_from_url(url: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return None;
    }
    let host = if let Some(rest) = trimmed.strip_prefix("https://") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        rest
    } else {
        return None;
    };
    let authority = host.split('/').next().unwrap_or(host);
    Some(
        authority
            .strip_prefix("www.")
            .unwrap_or(authority)
            .to_lowercase(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_site_strips_scheme() {
        assert_eq!(
            normalize_site("https://www.example.com/").unwrap(),
            "example.com"
        );
        assert_eq!(normalize_site("example.com").unwrap(), "example.com");
    }

    #[test]
    fn domain_from_url_works() {
        assert_eq!(
            domain_from_url("https://www.example.com/path").as_deref(),
            Some("example.com")
        );
    }
}
