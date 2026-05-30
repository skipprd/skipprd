use serde_json::{json, Map, Value};

use crate::config::Strategy;
use crate::issue;
use crate::streams::{
    NAMESPACE_AUDIT_DAILY, NAMESPACE_FIELD_ORIGIN_DAILY, NAMESPACE_ISSUE, NAMESPACE_PAGE_DAILY,
};

#[derive(Debug, Default)]
pub struct ParsedRun {
    pub page_daily: Vec<Value>,
    pub field_origin_daily: Vec<Value>,
    pub audit_daily: Vec<Value>,
    pub issues: Vec<Value>,
}

pub fn parse_pagespeed_response(
    body: &Value,
    site: &str,
    canonical_url: &str,
    run_date: &str,
    strategy: &Strategy,
    top_audits: u32,
) -> ParsedRun {
    let strategy_str = strategy.as_api_str();
    let mut out = ParsedRun::default();

    let lh = body.get("lighthouseResult");
    let mut page = Map::new();
    page.insert("site".into(), Value::String(site.to_string()));
    page.insert(
        "canonical_url".into(),
        Value::String(canonical_url.to_string()),
    );
    page.insert("run_date".into(), Value::String(run_date.to_string()));
    page.insert(
        "strategy".into(),
        Value::String(strategy_str.to_string()),
    );

    if let Some(ts) = body.get("analysisUTCTimestamp").and_then(|v| v.as_str()) {
        page.insert("analysis_timestamp".into(), Value::String(ts.to_string()));
    }
    if let Some(final_url) = lh
        .and_then(|l| l.get("finalUrl"))
        .and_then(|v| v.as_str())
    {
        page.insert("final_url".into(), Value::String(final_url.to_string()));
    }

    if let Some(categories) = lh.and_then(|l| l.get("categories")).and_then(|c| c.as_object()) {
        for (key, col) in [
            ("performance", "lh_performance"),
            ("accessibility", "lh_accessibility"),
            ("best-practices", "lh_best_practices"),
            ("seo", "lh_seo"),
            ("pwa", "lh_pwa"),
        ] {
            if let Some(score) = categories.get(key).and_then(category_score_0_100) {
                page.insert(col.into(), json!(score));
            }
        }
    }

    if let Some(audits) = lh.and_then(|l| l.get("audits")).and_then(|a| a.as_object()) {
        insert_lab_vital(&mut page, audits, "largest-contentful-paint", "lcp_ms");
        insert_lab_vital(&mut page, audits, "cumulative-layout-shift", "cls");
        insert_lab_vital(&mut page, audits, "interaction-to-next-paint", "inp_ms");
        insert_lab_vital(&mut page, audits, "experimental-interaction-to-next-paint", "inp_ms");
        insert_lab_vital(&mut page, audits, "first-contentful-paint", "fcp_ms");
        insert_lab_vital(&mut page, audits, "total-blocking-time", "tbt_ms");
        insert_lab_vital(&mut page, audits, "speed-index", "speed_index_ms");

        let failing = select_top_failing_audits(audits, top_audits);
        for audit in failing {
            let mut row = Map::new();
            row.insert("site".into(), Value::String(site.to_string()));
            row.insert(
                "canonical_url".into(),
                Value::String(canonical_url.to_string()),
            );
            row.insert("run_date".into(), Value::String(run_date.to_string()));
            row.insert(
                "strategy".into(),
                Value::String(strategy_str.to_string()),
            );
            if let Some(id) = audit.get("id").and_then(|v| v.as_str()) {
                row.insert("audit_id".into(), Value::String(id.to_string()));
            }
            if let Some(score) = audit.get("score") {
                row.insert("score".into(), score.clone());
            }
            if let Some(dv) = audit.get("displayValue").and_then(|v| v.as_str()) {
                row.insert("display_value".into(), Value::String(dv.to_string()));
            }
            if let Some(title) = audit.get("title").and_then(|v| v.as_str()) {
                row.insert("title".into(), Value::String(title.to_string()));
            }
            out.audit_daily.push(Value::Object(row));
        }
    }

    let field_url = body.get("loadingExperience");
    let field_available = field_url.is_some();
    page.insert("field_data_available".into(), json!(field_available));

    if let Some(fe) = field_url {
        if let Some(cat) = fe.get("overall_category").and_then(|v| v.as_str()) {
            page.insert(
                "field_overall_category".into(),
                Value::String(cat.to_string()),
            );
        }
        if let Some(metrics) = fe.get("metrics").and_then(|m| m.as_object()) {
            insert_field_metric_category(&mut page, metrics, "LARGEST_CONTENTFUL_PAINT_MS", "field_lcp_category");
            insert_field_metric_category(&mut page, metrics, "CUMULATIVE_LAYOUT_SHIFT_SCORE", "field_cls_category");
            insert_field_metric_category(&mut page, metrics, "INTERACTION_TO_NEXT_PAINT", "field_inp_category");
            insert_field_metric_category(&mut page, metrics, "EXPERIMENTAL_INTERACTION_TO_NEXT_PAINT", "field_inp_category");
            insert_field_metric_category(&mut page, metrics, "FIRST_CONTENTFUL_PAINT_MS", "field_fcp_category");
            insert_field_metric_category(&mut page, metrics, "EXPERIMENTAL_TIME_TO_FIRST_BYTE", "field_ttfb_category");
            insert_field_metric_category(&mut page, metrics, "TIME_TO_FIRST_BYTE_MS", "field_ttfb_category");
        }
    }

    out.issues
        .extend(issue::issues_for_page(site, canonical_url, run_date, strategy_str, &page));
    out.page_daily.push(Value::Object(page));

    if let Some(origin) = body.get("originLoadingExperience") {
        let mut row = Map::new();
        row.insert("site".into(), Value::String(site.to_string()));
        row.insert("run_date".into(), Value::String(run_date.to_string()));
        if let Some(host) = origin_host_from_site(site) {
            row.insert("origin".into(), Value::String(host));
        }
        if let Some(cat) = origin.get("overall_category").and_then(|v| v.as_str()) {
            row.insert("overall_category".into(), Value::String(cat.to_string()));
        }
        if let Some(metrics) = origin.get("metrics").and_then(|m| m.as_object()) {
            insert_field_metric_category(&mut row, metrics, "LARGEST_CONTENTFUL_PAINT_MS", "lcp_category");
            insert_field_metric_category(&mut row, metrics, "CUMULATIVE_LAYOUT_SHIFT_SCORE", "cls_category");
            insert_field_metric_category(&mut row, metrics, "INTERACTION_TO_NEXT_PAINT", "inp_category");
            insert_field_metric_category(&mut row, metrics, "FIRST_CONTENTFUL_PAINT_MS", "fcp_category");
            insert_field_metric_category(&mut row, metrics, "EXPERIMENTAL_TIME_TO_FIRST_BYTE", "ttfb_category");
        }
        out.field_origin_daily.push(Value::Object(row));
    }

    out
}

pub fn parse_error_page_row(
    site: &str,
    canonical_url: &str,
    run_date: &str,
    strategy: &Strategy,
    error_code: &str,
) -> ParsedRun {
    let strategy_str = strategy.as_api_str();
    let mut page = Map::new();
    page.insert("site".into(), Value::String(site.to_string()));
    page.insert(
        "canonical_url".into(),
        Value::String(canonical_url.to_string()),
    );
    page.insert("run_date".into(), Value::String(run_date.to_string()));
    page.insert(
        "strategy".into(),
        Value::String(strategy_str.to_string()),
    );
    page.insert("field_data_available".into(), json!(false));
    page.insert("error_code".into(), Value::String(error_code.to_string()));

    let mut issues = issue::issues_for_page(site, canonical_url, run_date, strategy_str, &page);
    if error_code == issue::INVALID_URL {
        issues.push(issue::issue_row(
            site,
            canonical_url,
            run_date,
            strategy_str,
            issue::INVALID_URL,
            "PageSpeed API rejected URL",
        ));
    } else if error_code == issue::API_QUOTA_EXCEEDED {
        issues.push(issue::issue_row(
            site,
            canonical_url,
            run_date,
            strategy_str,
            issue::API_QUOTA_EXCEEDED,
            "PageSpeed API quota exceeded",
        ));
    }

    ParsedRun {
        page_daily: vec![Value::Object(page)],
        field_origin_daily: Vec::new(),
        audit_daily: Vec::new(),
        issues,
    }
}

fn category_score_0_100(cat: &Value) -> Option<f64> {
    let score = cat.get("score")?.as_f64()?;
    Some((score * 100.0).round())
}

fn insert_lab_vital(page: &mut Map<String, Value>, audits: &Map<String, Value>, audit_id: &str, col: &str) {
    if page.contains_key(col) {
        return;
    }
    let Some(audit) = audits.get(audit_id) else {
        return;
    };
    if let Some(n) = audit.get("numericValue").and_then(|v| v.as_f64()) {
        page.insert(col.into(), json!(n));
    }
}

fn insert_field_metric_category(
    row: &mut Map<String, Value>,
    metrics: &Map<String, Value>,
    metric_key: &str,
    col: &str,
) {
    if row.contains_key(col) {
        return;
    }
    if let Some(cat) = metrics
        .get(metric_key)
        .and_then(|m| m.get("category"))
        .and_then(|v| v.as_str())
    {
        row.insert(col.into(), Value::String(cat.to_string()));
    }
}

fn select_top_failing_audits(
    audits: &Map<String, Value>,
    cap: u32,
) -> Vec<Map<String, Value>> {
    let mut failing: Vec<(f64, Map<String, Value>)> = audits
        .iter()
        .filter_map(|(id, audit)| {
            let score = audit.get("score")?.as_f64()?;
            if score >= 0.9 {
                return None;
            }
            let mut m = audit.as_object()?.clone();
            m.insert("id".into(), Value::String(id.clone()));
            Some((score, m))
        })
        .collect();
    failing.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    failing
        .into_iter()
        .take(cap as usize)
        .map(|(_, m)| m)
        .collect()
}

fn origin_host_from_site(site: &str) -> Option<String> {
    url::Url::parse(site)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
}

pub fn namespace_for_audit() -> &'static str {
    NAMESPACE_AUDIT_DAILY
}

pub fn namespace_for_page() -> &'static str {
    NAMESPACE_PAGE_DAILY
}

pub fn namespace_for_field_origin() -> &'static str {
    NAMESPACE_FIELD_ORIGIN_DAILY
}

pub fn namespace_for_issue() -> &'static str {
    NAMESPACE_ISSUE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Strategy;
    use std::path::PathBuf;

    fn fixture_dir() -> PathBuf {
        std::env::var("SKIPPR_GOOGLE_PAGESPEED_FIXTURE_DIR")
            .ok()
            .filter(|d| !d.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures")))
    }

    fn load_fixture(name: &str) -> Value {
        let path = fixture_dir().join(name);
        let bytes = std::fs::read(&path).expect("read fixture");
        serde_json::from_slice(&bytes).expect("parse fixture")
    }

    #[test]
    fn parse_mobile_fixture() {
        let body = load_fixture("run_pagespeed_mobile.json");
        let parsed = parse_pagespeed_response(
            &body,
            "https://example.com/",
            "https://example.com/",
            "2024-01-15",
            &Strategy::Mobile,
            15,
        );
        assert_eq!(parsed.page_daily.len(), 1);
        let page = parsed.page_daily[0].as_object().unwrap();
        assert_eq!(page.get("lh_performance").and_then(|v| v.as_f64()), Some(85.0));
        assert!(page.get("lcp_ms").and_then(|v| v.as_f64()).is_some());
        assert!(!parsed.audit_daily.is_empty());
        assert!(page.get("field_data_available").and_then(|v| v.as_bool()) == Some(true));
    }

    #[test]
    fn parse_no_field_data() {
        let body = load_fixture("run_pagespeed_no_field.json");
        let parsed = parse_pagespeed_response(
            &body,
            "https://example.com/",
            "https://example.com/no-field",
            "2024-01-15",
            &Strategy::Mobile,
            15,
        );
        let page = parsed.page_daily[0].as_object().unwrap();
        assert_eq!(
            page.get("field_data_available").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert!(
            parsed
                .issues
                .iter()
                .any(|i| i.get("issue_code").and_then(|v| v.as_str()) == Some(issue::NO_FIELD_DATA))
        );
    }

    #[test]
    fn parse_origin_field() {
        let body = load_fixture("run_pagespeed_mobile.json");
        let parsed = parse_pagespeed_response(
            &body,
            "https://example.com/",
            "https://example.com/",
            "2024-01-15",
            &Strategy::Mobile,
            15,
        );
        assert_eq!(parsed.field_origin_daily.len(), 1);
        let origin = parsed.field_origin_daily[0].as_object().unwrap();
        assert!(origin.get("overall_category").is_some());
        assert_eq!(origin.get("origin").and_then(|v| v.as_str()), Some("example.com"));
    }

    #[test]
    fn issue_thresholds_field_lcp_slow() {
        let mut body = load_fixture("run_pagespeed_mobile.json");
        body["loadingExperience"]["metrics"]["LARGEST_CONTENTFUL_PAINT_MS"]["category"] =
            json!("SLOW");
        let parsed = parse_pagespeed_response(
            &body,
            "https://example.com/",
            "https://example.com/",
            "2024-01-15",
            &Strategy::Mobile,
            5,
        );
        assert!(
            parsed.issues.iter().any(|i| {
                i.get("issue_code").and_then(|v| v.as_str()) == Some(issue::FIELD_LCP_SLOW)
            })
        );
    }
}
