#[derive(Debug, Clone, Default)]
pub struct CspAnalysis {
    pub has_unsafe_inline: bool,
    pub has_unsafe_eval: bool,
    pub has_wildcard_default: bool,
    pub has_frame_ancestors: bool,
    pub frame_ancestors_none: bool,
}

pub fn analyze_csp(raw: Option<&str>) -> CspAnalysis {
    let Some(csp) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return CspAnalysis::default();
    };
    let lower = csp.to_ascii_lowercase();
    let mut out = CspAnalysis {
        has_unsafe_inline: lower.contains("'unsafe-inline'") || lower.contains(" unsafe-inline"),
        has_unsafe_eval: lower.contains("'unsafe-eval'") || lower.contains(" unsafe-eval"),
        has_wildcard_default: lower.contains("default-src *") || lower.contains("default-src ' *"),
        has_frame_ancestors: lower.contains("frame-ancestors"),
        frame_ancestors_none: false,
    };
    if out.has_frame_ancestors {
        out.frame_ancestors_none = lower.contains("frame-ancestors 'none'")
            || lower.contains("frame-ancestors 'none'")
            || lower.contains("frame-ancestors none");
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
    }
}
