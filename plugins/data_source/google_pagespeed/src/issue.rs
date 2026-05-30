use serde_json::{json, Map, Value};

pub const FIELD_LCP_SLOW: &str = "FIELD_LCP_SLOW";
pub const FIELD_CLS_POOR: &str = "FIELD_CLS_POOR";
pub const LAB_PERFORMANCE_LOW: &str = "LAB_PERFORMANCE_LOW";
pub const NO_FIELD_DATA: &str = "NO_FIELD_DATA";
pub const API_QUOTA_EXCEEDED: &str = "API_QUOTA_EXCEEDED";
pub const INVALID_URL: &str = "INVALID_URL";

pub fn issues_for_page(
    site: &str,
    canonical_url: &str,
    run_date: &str,
    strategy: &str,
    page: &Map<String, Value>,
) -> Vec<Value> {
    let mut issues = Vec::new();
    let field_available = page
        .get("field_data_available")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if !field_available {
        issues.push(issue_row(
            site,
            canonical_url,
            run_date,
            strategy,
            NO_FIELD_DATA,
            "CrUX field data not returned for this URL",
        ));
    } else {
        if page.get("field_lcp_category").and_then(|v| v.as_str()) == Some("SLOW") {
            issues.push(issue_row(
                site,
                canonical_url,
                run_date,
                strategy,
                FIELD_LCP_SLOW,
                "Field LCP category is SLOW",
            ));
        }
        if page.get("field_cls_category").and_then(|v| v.as_str()) == Some("SLOW") {
            issues.push(issue_row(
                site,
                canonical_url,
                run_date,
                strategy,
                FIELD_CLS_POOR,
                "Field CLS category is SLOW",
            ));
        }
    }

    if let Some(score) = page.get("lh_performance").and_then(|v| v.as_f64()) {
        if score < 50.0 {
            issues.push(issue_row(
                site,
                canonical_url,
                run_date,
                strategy,
                LAB_PERFORMANCE_LOW,
                "Lighthouse performance score below 50",
            ));
        }
    }

    issues
}

pub fn issue_row(
    site: &str,
    canonical_url: &str,
    run_date: &str,
    strategy: &str,
    issue_code: &str,
    message: &str,
) -> Value {
    json!({
        "site": site,
        "canonical_url": canonical_url,
        "run_date": run_date,
        "strategy": strategy,
        "issue_code": issue_code,
        "message": message,
    })
}
