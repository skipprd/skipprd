#[derive(Debug, Clone, Default)]
pub struct CspAnalysis {
    pub has_unsafe_inline: bool,
    pub has_unsafe_eval: bool,
    pub has_wildcard_default: bool,
    pub has_frame_ancestors: bool,
    pub frame_ancestors_none: bool,
    pub frame_ancestors_restricted: bool,
}

pub fn analyze_csp(raw: Option<&str>) -> CspAnalysis {
    let Some(csp) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return CspAnalysis::default();
    };

    let mut out = CspAnalysis::default();
    for directive in csp.split(';') {
        let mut parts = directive
            .split_whitespace()
            .map(|p| p.trim().to_ascii_lowercase());
        let Some(name) = parts.next() else {
            continue;
        };
        let values: Vec<String> = parts.collect();
        if values
            .iter()
            .any(|v| v == "'unsafe-inline'" || v == "unsafe-inline")
        {
            out.has_unsafe_inline = true;
        }
        if values
            .iter()
            .any(|v| v == "'unsafe-eval'" || v == "unsafe-eval")
        {
            out.has_unsafe_eval = true;
        }
        if name == "default-src" && values.iter().any(|v| v == "*") {
            out.has_wildcard_default = true;
        }
        if name == "frame-ancestors" {
            out.has_frame_ancestors = true;
            out.frame_ancestors_none = values.iter().any(|v| v == "'none'" || v == "none");
            out.frame_ancestors_restricted = !values.is_empty()
                && !values
                    .iter()
                    .any(|v| v == "*" || v == "http:" || v == "https:");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_unsafe_directives() {
        let csp = "default-src *; script-src 'unsafe-inline' 'unsafe-eval'";
        let a = analyze_csp(Some(csp));
        assert!(a.has_unsafe_inline);
        assert!(a.has_unsafe_eval);
        assert!(a.has_wildcard_default);
    }

    #[test]
    fn frame_ancestors_none() {
        let a = analyze_csp(Some("frame-ancestors 'none'"));
        assert!(a.has_frame_ancestors);
        assert!(a.frame_ancestors_none);
        assert!(a.frame_ancestors_restricted);
    }

    #[test]
    fn frame_ancestors_wildcard_is_not_restricted() {
        let a = analyze_csp(Some("default-src 'self'; frame-ancestors *"));
        assert!(a.has_frame_ancestors);
        assert!(!a.frame_ancestors_restricted);
    }
}
