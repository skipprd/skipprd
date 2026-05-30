use url::Url;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SiteOrigin {
    pub origin: String,
    pub host: String,
    pub scheme: String,
}

pub fn normalize_site(site: &str) -> Result<SiteOrigin, std::io::Error> {
    let trimmed = site.trim();
    if trimmed.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "site is required",
        ));
    }
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let parsed = Url::parse(&with_scheme).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid site URL '{site}': {e}"),
        )
    })?;
    let host = parsed.host_str().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "site must include a host")
    })?;
    let scheme = parsed.scheme().to_string();
    let origin = format!("{}://{}", scheme, host);
    Ok(SiteOrigin {
        origin,
        host: host.to_string(),
        scheme,
    })
}

pub fn url_on_site(url: &Url, origin: &SiteOrigin) -> bool {
    url.host_str() == Some(origin.host.as_str()) && url.scheme() == origin.scheme
}

pub fn normalize_url_for_crawl(raw: &str, origin: &SiteOrigin) -> Option<String> {
    let base = Url::parse(&origin.origin).ok()?;
    let resolved = base.join(raw.trim()).ok()?;
    if !url_on_site(&resolved, origin) {
        return None;
    }
    let mut out = resolved.clone();
    out.set_fragment(None);
    if out.path().is_empty() {
        out.set_path("/");
    }
    Some(out.to_string())
}

pub fn url_hash(url: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(url.as_bytes());
    format!("{:x}", digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_bare_domain() {
        let o = normalize_site("example.com").unwrap();
        assert_eq!(o.origin, "https://example.com");
        assert_eq!(o.host, "example.com");
    }

    #[test]
    fn rejects_empty_site() {
        assert!(normalize_site("  ").is_err());
    }
}
