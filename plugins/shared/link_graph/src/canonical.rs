use regex::Regex;
use std::sync::OnceLock;
use url::Url;

pub const CANONICALIZATION_VERSION: &str = "v1";

pub type CanonicalizationVersion = &'static str;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalUrl {
    pub canonical: String,
    pub scheme: String,
    pub host: String,
    pub path: String,
    pub version: CanonicalizationVersion,
}

fn tracking_params() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(utm_.*|gclid|fbclid|msclkid|mc_eid|mc_cid)$").expect("tracking param regex")
    })
}

/// Canonicalize a URL per web-presence-corpus architecture v1 rules.
pub fn canonicalize_url(raw: &str) -> Option<CanonicalUrl> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut url = Url::parse(trimmed).ok()?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return None;
    }
    url.set_fragment(None);
    if let Some(port) = url.port() {
        let default = match url.scheme() {
            "http" => 80,
            "https" => 443,
            _ => return None,
        };
        if port == default {
            let _ = url.set_port(None);
        }
    }
    let scheme = url.scheme().to_ascii_lowercase();
    let host = url.host_str()?.to_ascii_lowercase();
    let mut path = url.path().to_string();
    if path.is_empty() {
        path = "/".to_string();
    }
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| !tracking_params().is_match(k))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let query = if pairs.is_empty() {
        String::new()
    } else {
        pairs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&")
    };
    let canonical = if query.is_empty() {
        format!("{scheme}://{host}{path}")
    } else {
        format!("{scheme}://{host}{path}?{query}")
    };
    Some(CanonicalUrl {
        canonical,
        scheme,
        host: host.clone(),
        path,
        version: CANONICALIZATION_VERSION,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_fragment_and_tracking_params() {
        let c = canonicalize_url("HTTPS://Skippr.IO/path?utm_source=x&gclid=1#frag").unwrap();
        assert_eq!(c.canonical, "https://skippr.io/path");
        assert_eq!(c.version, CANONICALIZATION_VERSION);
    }

    #[test]
    fn normalizes_empty_path() {
        let c = canonicalize_url("http://example.com").unwrap();
        assert_eq!(c.path, "/");
    }

    #[test]
    fn preserves_deep_path_case() {
        let c = canonicalize_url("https://example.com/Case/Path/").unwrap();
        assert_eq!(c.path, "/Case/Path/");
    }
}
