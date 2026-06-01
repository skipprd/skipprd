/// Normalize a hostname or URL to a comparable registrable domain (no `www.`).
pub fn normalize_domain(input: &str) -> String {
    let mut value = input.trim().to_lowercase();
    if let Some(rest) = value.strip_prefix("https://") {
        value = rest.to_string();
    } else if let Some(rest) = value.strip_prefix("http://") {
        value = rest.to_string();
    }
    if let Some(host) = value.split('/').next() {
        value = host.to_string();
    }
    if let Some(host) = value.split('?').next() {
        value = host.to_string();
    }
    if let Some(host) = value.split('#').next() {
        value = host.to_string();
    }
    if let Some(rest) = value.strip_prefix("www.") {
        value = rest.to_string();
    }
    value
}

/// Whether `domain` equals `target` or is a subdomain of `target`.
pub fn domain_matches_target(domain: &str, target: &str) -> bool {
    let domain = normalize_domain(domain);
    let target = normalize_domain(target);
    if domain.is_empty() || target.is_empty() {
        return false;
    }
    domain == target || domain.ends_with(&format!(".{target}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_www_and_path() {
        assert_eq!(
            normalize_domain("https://www.Example.com/path?q=1"),
            "example.com"
        );
    }

    #[test]
    fn subdomain_matches_parent_target() {
        assert!(domain_matches_target("blog.example.com", "example.com"));
        assert!(!domain_matches_target("notexample.com", "example.com"));
    }

    #[test]
    fn empty_domain_never_matches() {
        assert!(!domain_matches_target("", "example.com"));
        assert!(!domain_matches_target("example.com", ""));
    }
}
