use serde_json::{json, Value};

use crate::config::DataSourceSiteSecurityPluginConfig;
use crate::csp::analyze_csp;
use crate::secrets::scan_body;
use crate::worker::{
    DomMetricsWire, JarCookieWire, SecurityHeaders, StorageEntryWire, WorkerJobResult,
};

pub const NAVIGATION_TIMEOUT: &str = "NAVIGATION_TIMEOUT";
pub const HTTP_ERROR: &str = "HTTP_ERROR";
pub const COOKIES_PRESENT: &str = "COOKIES_PRESENT";
pub const LOCAL_STORAGE_KEYS: &str = "LOCAL_STORAGE_KEYS";
pub const SESSION_STORAGE_KEYS: &str = "SESSION_STORAGE_KEYS";
pub const PII_IN_COOKIE: &str = "PII_IN_COOKIE";
pub const PII_IN_LOCAL_STORAGE: &str = "PII_IN_LOCAL_STORAGE";
pub const PII_IN_SESSION_STORAGE: &str = "PII_IN_SESSION_STORAGE";
pub const MISSING_CSP: &str = "MISSING_CSP";
pub const CSP_UNSAFE_INLINE: &str = "CSP_UNSAFE_INLINE";
pub const CSP_UNSAFE_EVAL: &str = "CSP_UNSAFE_EVAL";
pub const CSP_WILDCARD_DEFAULT: &str = "CSP_WILDCARD_DEFAULT";
pub const MISSING_HSTS: &str = "MISSING_HSTS";
pub const HSTS_SHORT_MAX_AGE: &str = "HSTS_SHORT_MAX_AGE";
pub const HSTS_INCLUDE_SUBDOMAINS_MISSING: &str = "HSTS_INCLUDE_SUBDOMAINS_MISSING";
pub const HSTS_PRELOAD_MISSING: &str = "HSTS_PRELOAD_MISSING";
pub const MISSING_X_FRAME_OPTIONS: &str = "MISSING_X_FRAME_OPTIONS";
pub const MISSING_X_CONTENT_TYPE_OPTIONS: &str = "MISSING_X_CONTENT_TYPE_OPTIONS";
pub const MISSING_REFERRER_POLICY: &str = "MISSING_REFERRER_POLICY";
pub const MISSING_PERMISSIONS_POLICY: &str = "MISSING_PERMISSIONS_POLICY";
pub const MISSING_COOP: &str = "MISSING_COOP";
pub const MISSING_COEP: &str = "MISSING_COEP";
pub const MISSING_CORP: &str = "MISSING_CORP";
pub const LEGACY_XSS_PROTECTION: &str = "LEGACY_XSS_PROTECTION";
pub const COOKIE_MISSING_SECURE: &str = "COOKIE_MISSING_SECURE";
pub const COOKIE_MISSING_HTTPONLY: &str = "COOKIE_MISSING_HTTPONLY";
pub const COOKIE_SAMESITE_NONE: &str = "COOKIE_SAMESITE_NONE";
pub const COOKIE_SAMESITE_WEAK: &str = "COOKIE_SAMESITE_WEAK";
pub const COOKIE_HOST_PREFIX_INVALID: &str = "COOKIE_HOST_PREFIX_INVALID";
pub const COOKIE_BROAD_DOMAIN: &str = "COOKIE_BROAD_DOMAIN";
pub const THIRD_PARTY_SCRIPTS_HIGH: &str = "THIRD_PARTY_SCRIPTS_HIGH";
pub const SCRIPT_HOST_BLOCKLIST: &str = "SCRIPT_HOST_BLOCKLIST";
pub const SRI_MISSING_ON_EXTERNAL: &str = "SRI_MISSING_ON_EXTERNAL";
pub const INLINE_SCRIPTS_PRESENT: &str = "INLINE_SCRIPTS_PRESENT";
pub const MIXED_CONTENT: &str = "MIXED_CONTENT";
pub const INSECURE_FORM_ACTION: &str = "INSECURE_FORM_ACTION";
pub const SECRETS_IN_PAGE: &str = "SECRETS_IN_PAGE";
pub const HTTP_REDIRECT_TO_HTTPS: &str = "HTTP_REDIRECT_TO_HTTPS";
pub const TLS_CERT_INVALID: &str = "TLS_CERT_INVALID";
pub const SECURITY_TXT_MISSING: &str = "SECURITY_TXT_MISSING";

const BLOCKED_SCRIPT_HOSTS: &[&str] = &[
    "coinhive.com",
    "coin-hive.com",
    "jqueryscdn.com",
    "bootstrapcdn.net",
];

pub fn check_row(
    site: &str,
    page_url: &str,
    run_date: &str,
    issue_code: &str,
    section: &str,
    status: &str,
    severity: &str,
    message: &str,
) -> Value {
    json!({
        "site": site,
        "page_url": page_url,
        "run_date": run_date,
        "issue_code": issue_code,
        "section": section,
        "status": status,
        "severity": severity,
        "message": message,
    })
}

fn has_pii(entries: &[StorageEntryWire]) -> bool {
    entries.iter().any(|e| !e.pii_hints.is_empty())
}

fn hsts_max_age(hsts: Option<&str>) -> Option<u64> {
    let raw = hsts?;
    for part in raw.split(';') {
        let part = part.trim().to_ascii_lowercase();
        if let Some(rest) = part.strip_prefix("max-age=") {
            return rest.trim().parse().ok();
        }
    }
    None
}

fn hsts_has_directive(hsts: Option<&str>, directive: &str) -> bool {
    let Some(raw) = hsts else {
        return false;
    };
    raw.split(';')
        .map(|part| part.trim().to_ascii_lowercase())
        .any(|part| part == directive.to_ascii_lowercase())
}

fn samesite_is_strict_or_lax(same_site: &str) -> bool {
    same_site.eq_ignore_ascii_case("strict") || same_site.eq_ignore_ascii_case("lax")
}

fn apply_header_checks(
    site: &str,
    page_url: &str,
    run_date: &str,
    headers: &SecurityHeaders,
    is_https: bool,
    rows: &mut Vec<Value>,
) {
    let section = "headers";
    let csp_analysis = analyze_csp(headers.csp.as_deref());

    rows.push(check_row(
        site,
        page_url,
        run_date,
        MISSING_CSP,
        section,
        if headers.has_csp { "pass" } else { "warn" },
        if headers.has_csp { "info" } else { "warning" },
        if headers.has_csp {
            "Content-Security-Policy present"
        } else {
            "No Content-Security-Policy response header"
        },
    ));

    if headers.has_csp {
        rows.push(check_row(
            site,
            page_url,
            run_date,
            CSP_UNSAFE_INLINE,
            section,
            if csp_analysis.has_unsafe_inline {
                "warn"
            } else {
                "pass"
            },
            "warning",
            if csp_analysis.has_unsafe_inline {
                "CSP allows unsafe-inline"
            } else {
                "CSP does not allow unsafe-inline"
            },
        ));
        rows.push(check_row(
            site,
            page_url,
            run_date,
            CSP_UNSAFE_EVAL,
            section,
            if csp_analysis.has_unsafe_eval {
                "warn"
            } else {
                "pass"
            },
            "warning",
            if csp_analysis.has_unsafe_eval {
                "CSP allows unsafe-eval"
            } else {
                "CSP does not allow unsafe-eval"
            },
        ));
        rows.push(check_row(
            site,
            page_url,
            run_date,
            CSP_WILDCARD_DEFAULT,
            section,
            if csp_analysis.has_wildcard_default {
                "warn"
            } else {
                "pass"
            },
            "warning",
            if csp_analysis.has_wildcard_default {
                "CSP default-src is overly permissive"
            } else {
                "CSP default-src is not a bare wildcard"
            },
        ));
    }

    if is_https {
        rows.push(check_row(
            site,
            page_url,
            run_date,
            MISSING_HSTS,
            section,
            if headers.has_hsts { "pass" } else { "warn" },
            if headers.has_hsts { "info" } else { "warning" },
            if headers.has_hsts {
                "Strict-Transport-Security present"
            } else {
                "HTTPS page without Strict-Transport-Security"
            },
        ));
        if headers.has_hsts {
            let max_age = hsts_max_age(headers.hsts.as_deref());
            let has_include_subdomains =
                hsts_has_directive(headers.hsts.as_deref(), "includesubdomains");
            rows.push(check_row(
                site,
                page_url,
                run_date,
                HSTS_SHORT_MAX_AGE,
                section,
                if max_age.is_some_and(|a| a >= 15_552_000) {
                    "pass"
                } else {
                    "warn"
                },
                "warning",
                &format!(
                    "HSTS max-age is {}",
                    max_age
                        .map(|a| a.to_string())
                        .unwrap_or_else(|| "unknown".into())
                ),
            ));
            rows.push(check_row(
                site,
                page_url,
                run_date,
                HSTS_INCLUDE_SUBDOMAINS_MISSING,
                section,
                if has_include_subdomains {
                    "pass"
                } else {
                    "warn"
                },
                "info",
                if has_include_subdomains {
                    "HSTS includeSubDomains directive present"
                } else {
                    "HSTS missing includeSubDomains"
                },
            ));
            let has_preload = hsts_has_directive(headers.hsts.as_deref(), "preload");
            let preload_eligible =
                max_age.is_some_and(|a| a >= 31_536_000) && has_include_subdomains && has_preload;
            rows.push(check_row(
                site,
                page_url,
                run_date,
                HSTS_PRELOAD_MISSING,
                section,
                if preload_eligible { "pass" } else { "warn" },
                "info",
                if preload_eligible {
                    "HSTS satisfies preload directives"
                } else {
                    "HSTS does not satisfy preload directives (max-age >= 31536000, includeSubDomains, preload)"
                },
            ));
        }
    }

    let frame_ok = headers.has_x_frame_options || csp_analysis.frame_ancestors_restricted;
    rows.push(check_row(
        site,
        page_url,
        run_date,
        MISSING_X_FRAME_OPTIONS,
        section,
        if frame_ok { "pass" } else { "warn" },
        if frame_ok { "info" } else { "warning" },
        if frame_ok {
            "Clickjacking mitigation via X-Frame-Options or CSP frame-ancestors"
        } else {
            "No X-Frame-Options or CSP frame-ancestors detected"
        },
    ));

    rows.push(check_row(
        site,
        page_url,
        run_date,
        MISSING_X_CONTENT_TYPE_OPTIONS,
        section,
        if headers.has_x_content_type_options {
            "pass"
        } else {
            "warn"
        },
        if headers.has_x_content_type_options {
            "info"
        } else {
            "warning"
        },
        if headers.has_x_content_type_options {
            "X-Content-Type-Options present"
        } else {
            "No X-Content-Type-Options header"
        },
    ));

    rows.push(check_row(
        site,
        page_url,
        run_date,
        MISSING_REFERRER_POLICY,
        section,
        if headers.referrer_policy.is_some() {
            "pass"
        } else {
            "warn"
        },
        "info",
        if headers.referrer_policy.is_some() {
            "Referrer-Policy present"
        } else {
            "No Referrer-Policy header"
        },
    ));

    rows.push(check_row(
        site,
        page_url,
        run_date,
        MISSING_PERMISSIONS_POLICY,
        section,
        if headers.permissions_policy.is_some() {
            "pass"
        } else {
            "warn"
        },
        "info",
        if headers.permissions_policy.is_some() {
            "Permissions-Policy present"
        } else {
            "No Permissions-Policy header"
        },
    ));

    rows.push(check_row(
        site,
        page_url,
        run_date,
        MISSING_COOP,
        section,
        if headers.cross_origin_opener_policy.is_some() {
            "pass"
        } else {
            "warn"
        },
        "info",
        if headers.cross_origin_opener_policy.is_some() {
            "Cross-Origin-Opener-Policy present"
        } else {
            "No Cross-Origin-Opener-Policy header"
        },
    ));

    rows.push(check_row(
        site,
        page_url,
        run_date,
        MISSING_COEP,
        section,
        if headers.cross_origin_embedder_policy.is_some() {
            "pass"
        } else {
            "warn"
        },
        "info",
        if headers.cross_origin_embedder_policy.is_some() {
            "Cross-Origin-Embedder-Policy present"
        } else {
            "No Cross-Origin-Embedder-Policy header"
        },
    ));

    rows.push(check_row(
        site,
        page_url,
        run_date,
        MISSING_CORP,
        section,
        if headers.cross_origin_resource_policy.is_some() {
            "pass"
        } else {
            "warn"
        },
        "info",
        if headers.cross_origin_resource_policy.is_some() {
            "Cross-Origin-Resource-Policy present"
        } else {
            "No Cross-Origin-Resource-Policy header"
        },
    ));

    if headers.x_xss_protection.is_some() {
        rows.push(check_row(
            site,
            page_url,
            run_date,
            LEGACY_XSS_PROTECTION,
            section,
            "warn",
            "info",
            "Legacy X-XSS-Protection header present (deprecated)",
        ));
    }
}

fn apply_cookie_checks(
    site: &str,
    page_url: &str,
    run_date: &str,
    jar: &[JarCookieWire],
    is_https: bool,
    page_host: &str,
    rows: &mut Vec<Value>,
) {
    let section = "cookies";
    if jar.is_empty() {
        return;
    }
    let missing_secure = jar.iter().any(|c| is_https && !c.secure);
    rows.push(check_row(
        site,
        page_url,
        run_date,
        COOKIE_MISSING_SECURE,
        section,
        if missing_secure { "fail" } else { "pass" },
        if missing_secure { "warning" } else { "info" },
        if missing_secure {
            "One or more cookies missing Secure on HTTPS"
        } else {
            "All cookies have Secure on HTTPS"
        },
    ));

    let missing_httponly = jar.iter().any(|c| !c.http_only);
    rows.push(check_row(
        site,
        page_url,
        run_date,
        COOKIE_MISSING_HTTPONLY,
        section,
        if missing_httponly { "warn" } else { "pass" },
        if missing_httponly { "warning" } else { "info" },
        if missing_httponly {
            "One or more cookies accessible to JavaScript (no HttpOnly)"
        } else {
            "All cookies are HttpOnly"
        },
    ));

    let samesite_none = jar.iter().any(|c| c.same_site.eq_ignore_ascii_case("none"));
    rows.push(check_row(
        site,
        page_url,
        run_date,
        COOKIE_SAMESITE_NONE,
        section,
        if samesite_none { "warn" } else { "pass" },
        if samesite_none { "warning" } else { "info" },
        if samesite_none {
            "Cookie(s) with SameSite=None"
        } else {
            "No SameSite=None cookies"
        },
    ));

    let weak_samesite = jar.iter().any(|c| !samesite_is_strict_or_lax(&c.same_site));
    rows.push(check_row(
        site,
        page_url,
        run_date,
        COOKIE_SAMESITE_WEAK,
        section,
        if weak_samesite { "warn" } else { "pass" },
        if weak_samesite { "warning" } else { "info" },
        if weak_samesite {
            "Cookie(s) without SameSite Strict or Lax"
        } else {
            "All cookies use SameSite Strict or Lax"
        },
    ));

    let bad_host_prefix = jar.iter().any(|c| {
        (c.entry_name.starts_with("__Host-") && (!c.secure || c.path != "/"))
            || (c.entry_name.starts_with("__Secure-") && !c.secure)
    });
    rows.push(check_row(
        site,
        page_url,
        run_date,
        COOKIE_HOST_PREFIX_INVALID,
        section,
        if bad_host_prefix { "fail" } else { "pass" },
        if bad_host_prefix { "warning" } else { "info" },
        if bad_host_prefix {
            "__Host- or __Secure- cookie prefix requirements not met"
        } else {
            "Cookie host prefixes satisfy requirements"
        },
    ));

    let broad_domain = jar.iter().any(|c| {
        c.domain.starts_with('.') && page_host.ends_with(c.domain.trim_start_matches('.'))
    });
    rows.push(check_row(
        site,
        page_url,
        run_date,
        COOKIE_BROAD_DOMAIN,
        section,
        if broad_domain { "warn" } else { "pass" },
        if broad_domain { "warning" } else { "info" },
        if broad_domain {
            "Cookie Domain scoped to parent domain"
        } else {
            "No overly broad cookie Domain attribute"
        },
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hsts_directives_case_insensitively() {
        let hsts = Some("max-age=31536000; includeSubDomains; preload");
        assert_eq!(hsts_max_age(hsts), Some(31_536_000));
        assert!(hsts_has_directive(hsts, "includesubdomains"));
        assert!(hsts_has_directive(hsts, "preload"));
    }

    #[test]
    fn hsts_unknown_max_age_is_not_accepted() {
        assert_eq!(hsts_max_age(Some("max-age=abc; includeSubDomains")), None);
    }

    #[test]
    fn samesite_requires_strict_or_lax() {
        assert!(samesite_is_strict_or_lax("Strict"));
        assert!(samesite_is_strict_or_lax("Lax"));
        assert!(!samesite_is_strict_or_lax("None"));
        assert!(!samesite_is_strict_or_lax(""));
    }
}

fn apply_dom_checks(
    site: &str,
    page_url: &str,
    run_date: &str,
    dom: &DomMetricsWire,
    mixed: &[String],
    is_https: bool,
    rows: &mut Vec<Value>,
) {
    let section = "scripts";
    rows.push(check_row(
        site,
        page_url,
        run_date,
        INLINE_SCRIPTS_PRESENT,
        section,
        if dom.inline_script_count == 0 {
            "pass"
        } else {
            "warn"
        },
        "info",
        &format!("{} inline script tag(s)", dom.inline_script_count),
    ));
    rows.push(check_row(
        site,
        page_url,
        run_date,
        SRI_MISSING_ON_EXTERNAL,
        section,
        if dom.scripts_without_sri == 0 {
            "pass"
        } else {
            "warn"
        },
        "warning",
        &format!(
            "{} external script(s) without integrity attribute",
            dom.scripts_without_sri
        ),
    ));

    if is_https && !mixed.is_empty() {
        rows.push(check_row(
            site,
            page_url,
            run_date,
            MIXED_CONTENT,
            section,
            "fail",
            "warning",
            &format!("{} passive mixed-content request(s)", mixed.len()),
        ));
    } else if is_https {
        rows.push(check_row(
            site,
            page_url,
            run_date,
            MIXED_CONTENT,
            section,
            "pass",
            "info",
            "No passive mixed-content requests observed",
        ));
    }

    if dom.insecure_form_count > 0 {
        rows.push(check_row(
            site,
            page_url,
            run_date,
            INSECURE_FORM_ACTION,
            section,
            "fail",
            "warning",
            &format!("{} form(s) submit to http://", dom.insecure_form_count),
        ));
    }
}

fn apply_script_blocklist(
    site: &str,
    page_url: &str,
    run_date: &str,
    result: &WorkerJobResult,
    rows: &mut Vec<Value>,
) {
    let hits: Vec<_> = result
        .scripts
        .iter()
        .filter(|s| {
            BLOCKED_SCRIPT_HOSTS
                .iter()
                .any(|bad| s.script_host.eq_ignore_ascii_case(bad))
        })
        .collect();
    let blocklist_msg = if hits.is_empty() {
        "No known-bad script hosts".to_string()
    } else {
        format!("{} script(s) from blocklisted host(s)", hits.len())
    };
    rows.push(check_row(
        site,
        page_url,
        run_date,
        SCRIPT_HOST_BLOCKLIST,
        "scripts",
        if hits.is_empty() { "pass" } else { "fail" },
        if hits.is_empty() { "info" } else { "warning" },
        &blocklist_msg,
    ));
}

pub fn origin_checks(site: &str, run_date: &str, probe: &crate::tls::TlsProbeResult) -> Vec<Value> {
    let page_url = site;
    vec![
        check_row(
            site,
            page_url,
            run_date,
            HTTP_REDIRECT_TO_HTTPS,
            "origin",
            if probe.http_redirects_to_https {
                "pass"
            } else {
                "warn"
            },
            "warning",
            if probe.http_redirects_to_https {
                "HTTP redirects to HTTPS"
            } else {
                "HTTP does not redirect to HTTPS"
            },
        ),
        check_row(
            site,
            page_url,
            run_date,
            TLS_CERT_INVALID,
            "origin",
            if probe.https_reachable && probe.cert_valid {
                "pass"
            } else {
                "fail"
            },
            "error",
            if probe.https_reachable {
                "HTTPS reachable with valid certificate"
            } else {
                "HTTPS unreachable or certificate error"
            },
        ),
        check_row(
            site,
            page_url,
            run_date,
            SECURITY_TXT_MISSING,
            "origin",
            if probe.security_txt_found {
                "pass"
            } else {
                "warn"
            },
            "info",
            if probe.security_txt_found {
                "security.txt found"
            } else {
                "No /.well-known/security.txt"
            },
        ),
    ]
}

pub fn map_issues(
    site: &str,
    page_url: &str,
    run_date: &str,
    result: &WorkerJobResult,
    config: &DataSourceSiteSecurityPluginConfig,
) -> Vec<Value> {
    let mut rows = Vec::new();

    if !result.ok {
        let code = result
            .error
            .as_ref()
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_str())
            .unwrap_or(NAVIGATION_TIMEOUT);
        rows.push(check_row(
            site,
            page_url,
            run_date,
            code,
            "run",
            "fail",
            "error",
            result
                .error
                .as_ref()
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("security scan failed"),
        ));
        return rows;
    }

    let status = result.status.unwrap_or(0);
    if status >= 400 {
        rows.push(check_row(
            site,
            page_url,
            run_date,
            HTTP_ERROR,
            "http",
            "fail",
            "error",
            &format!("HTTP {status}"),
        ));
    } else {
        rows.push(check_row(
            site,
            page_url,
            run_date,
            HTTP_ERROR,
            "http",
            "pass",
            "info",
            &format!("HTTP {status}"),
        ));
    }

    let storage_section = "storage";
    let jar = &result.jar_cookies;
    let cookie_count = result
        .cookie_count
        .unwrap_or(jar.len() as u32)
        .max(result.cookies.len() as u32);

    rows.push(check_row(
        site,
        page_url,
        run_date,
        COOKIES_PRESENT,
        storage_section,
        if cookie_count == 0 { "pass" } else { "warn" },
        "info",
        &format!("{cookie_count} cookie(s) in jar"),
    ));
    rows.push(check_row(
        site,
        page_url,
        run_date,
        PII_IN_COOKIE,
        storage_section,
        if has_pii(&result.cookies) || jar.iter().any(|c| !c.pii_hints.is_empty()) {
            "fail"
        } else {
            "pass"
        },
        if has_pii(&result.cookies) {
            "warning"
        } else {
            "info"
        },
        if has_pii(&result.cookies) || jar.iter().any(|c| !c.pii_hints.is_empty()) {
            "Heuristic PII or sensitive names in cookies"
        } else {
            "No PII heuristics matched in cookies"
        },
    ));

    let ls_count = result
        .local_storage_key_count
        .unwrap_or(result.local_storage.len() as u32);
    rows.push(check_row(
        site,
        page_url,
        run_date,
        LOCAL_STORAGE_KEYS,
        storage_section,
        if ls_count == 0 { "pass" } else { "warn" },
        "info",
        &format!("{ls_count} localStorage key(s)"),
    ));
    rows.push(check_row(
        site,
        page_url,
        run_date,
        PII_IN_LOCAL_STORAGE,
        storage_section,
        if has_pii(&result.local_storage) {
            "fail"
        } else {
            "pass"
        },
        if has_pii(&result.local_storage) {
            "warning"
        } else {
            "info"
        },
        if has_pii(&result.local_storage) {
            "Heuristic PII in localStorage"
        } else {
            "No PII heuristics in localStorage"
        },
    ));

    let ss_count = result
        .session_storage_key_count
        .unwrap_or(result.session_storage.len() as u32);
    rows.push(check_row(
        site,
        page_url,
        run_date,
        SESSION_STORAGE_KEYS,
        storage_section,
        if ss_count == 0 { "pass" } else { "warn" },
        "info",
        &format!("{ss_count} sessionStorage key(s)"),
    ));
    rows.push(check_row(
        site,
        page_url,
        run_date,
        PII_IN_SESSION_STORAGE,
        storage_section,
        if has_pii(&result.session_storage) {
            "fail"
        } else {
            "pass"
        },
        if has_pii(&result.session_storage) {
            "warning"
        } else {
            "info"
        },
        if has_pii(&result.session_storage) {
            "Heuristic PII in sessionStorage"
        } else {
            "No PII heuristics in sessionStorage"
        },
    ));

    let tp_count = result
        .third_party_script_count
        .unwrap_or(result.scripts.iter().filter(|s| s.is_third_party).count() as u32);
    rows.push(check_row(
        site,
        page_url,
        run_date,
        THIRD_PARTY_SCRIPTS_HIGH,
        "scripts",
        if tp_count <= config.max_third_party_scripts {
            "pass"
        } else {
            "warn"
        },
        if tp_count <= config.max_third_party_scripts {
            "info"
        } else {
            "warning"
        },
        &format!("{tp_count} third-party script(s)"),
    ));

    apply_script_blocklist(site, page_url, run_date, result, &mut rows);

    let final_url = result.final_url.as_deref().unwrap_or(page_url);
    let is_https = final_url.starts_with("https://");
    let page_host = final_url
        .strip_prefix("https://")
        .or_else(|| final_url.strip_prefix("http://"))
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("");

    if let Some(headers) = &result.headers {
        apply_header_checks(site, page_url, run_date, headers, is_https, &mut rows);
    }

    if !jar.is_empty() {
        apply_cookie_checks(
            site, page_url, run_date, jar, is_https, page_host, &mut rows,
        );
    }

    if let Some(dom) = &result.dom {
        apply_dom_checks(
            site,
            page_url,
            run_date,
            dom,
            &result.mixed_content_urls,
            is_https,
            &mut rows,
        );
    }

    let secrets = scan_body(result.body_snippet.as_deref());
    if secrets.aws_key || secrets.github_token || secrets.generic_api {
        rows.push(check_row(
            site,
            page_url,
            run_date,
            SECRETS_IN_PAGE,
            "content",
            "fail",
            "warning",
            "Heuristic secret patterns in page HTML",
        ));
    } else {
        rows.push(check_row(
            site,
            page_url,
            run_date,
            SECRETS_IN_PAGE,
            "content",
            "pass",
            "info",
            "No heuristic secret patterns in page HTML",
        ));
    }

    rows
}
