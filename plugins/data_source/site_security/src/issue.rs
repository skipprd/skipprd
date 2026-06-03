use serde_json::{json, Value};

use crate::config::DataSourceSiteSecurityPluginConfig;
use crate::worker::{SecurityHeaders, StorageEntryWire, WorkerJobResult};

pub const NAVIGATION_TIMEOUT: &str = "NAVIGATION_TIMEOUT";
pub const HTTP_ERROR: &str = "HTTP_ERROR";
pub const COOKIES_PRESENT: &str = "COOKIES_PRESENT";
pub const LOCAL_STORAGE_KEYS: &str = "LOCAL_STORAGE_KEYS";
pub const SESSION_STORAGE_KEYS: &str = "SESSION_STORAGE_KEYS";
pub const PII_IN_COOKIE: &str = "PII_IN_COOKIE";
pub const PII_IN_LOCAL_STORAGE: &str = "PII_IN_LOCAL_STORAGE";
pub const PII_IN_SESSION_STORAGE: &str = "PII_IN_SESSION_STORAGE";
pub const MISSING_CSP: &str = "MISSING_CSP";
pub const MISSING_HSTS: &str = "MISSING_HSTS";
pub const MISSING_X_FRAME_OPTIONS: &str = "MISSING_X_FRAME_OPTIONS";
pub const MISSING_X_CONTENT_TYPE_OPTIONS: &str = "MISSING_X_CONTENT_TYPE_OPTIONS";
pub const THIRD_PARTY_SCRIPTS_HIGH: &str = "THIRD_PARTY_SCRIPTS_HIGH";

fn check_row(
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

fn apply_header_checks(
    site: &str,
    page_url: &str,
    run_date: &str,
    headers: &SecurityHeaders,
    is_https: bool,
    rows: &mut Vec<Value>,
) {
    let section = "headers";
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
    }
    rows.push(check_row(
        site,
        page_url,
        run_date,
        MISSING_X_FRAME_OPTIONS,
        section,
        if headers.has_x_frame_options {
            "pass"
        } else {
            "warn"
        },
        if headers.has_x_frame_options {
            "info"
        } else {
            "warning"
        },
        if headers.has_x_frame_options {
            "X-Frame-Options or frame-ancestors policy present"
        } else {
            "No X-Frame-Options header detected"
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
    let cookie_count = result.cookie_count.unwrap_or(result.cookies.len() as u32);
    rows.push(check_row(
        site,
        page_url,
        run_date,
        COOKIES_PRESENT,
        storage_section,
        if cookie_count == 0 { "pass" } else { "warn" },
        "info",
        &format!("{cookie_count} document cookie(s)"),
    ));
    rows.push(check_row(
        site,
        page_url,
        run_date,
        PII_IN_COOKIE,
        storage_section,
        if has_pii(&result.cookies) { "fail" } else { "pass" },
        if has_pii(&result.cookies) {
            "warning"
        } else {
            "info"
        },
        if has_pii(&result.cookies) {
            "Heuristic PII or sensitive names in cookies"
        } else {
            "No PII heuristics matched in cookie names/values"
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

    if let Some(headers) = &result.headers {
        let is_https = result
            .final_url
            .as_deref()
            .unwrap_or(page_url)
            .starts_with("https://");
        apply_header_checks(site, page_url, run_date, headers, is_https, &mut rows);
    }

    rows
}
