use url::Url;

pub fn normalize_site_origin(site: &str) -> Result<String, std::io::Error> {
    let trimmed = site.trim();
    let with_scheme = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
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
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("site URL missing host: {site}"),
        )
    })?;
    Ok(format!("https://{host}"))
}

pub fn host_label(origin: &str) -> String {
    Url::parse(origin)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| origin.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_https_scheme_when_missing() {
        assert_eq!(
            normalize_site_origin("example.com").unwrap(),
            "https://example.com"
        );
    }

    #[test]
    fn strips_path_and_keeps_host() {
        assert_eq!(
            normalize_site_origin("https://example.com/about").unwrap(),
            "https://example.com"
        );
    }

    #[test]
    fn invalid_url_rejected() {
        assert!(normalize_site_origin("not a url!!!").is_err());
    }

    #[test]
    fn host_label_from_origin() {
        assert_eq!(host_label("https://www.example.com"), "www.example.com");
    }
}
