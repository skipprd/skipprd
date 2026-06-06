use serde_json::{json, Map, Value};

use crate::config::Strategy;
use crate::issue;
use crate::streams::{
    NAMESPACE_AUDIT_DAILY, NAMESPACE_CHECK_DAILY, NAMESPACE_FIELD_ORIGIN_DAILY,
    NAMESPACE_PAGE_DAILY, NAMESPACE_SITE_RUN_DAILY,
};

pub struct ParsedPageSpeed {
    pub page_rows: Vec<Value>,
    pub audit_rows: Vec<Value>,
    pub issue_rows: Vec<Value>,
    pub field_origin_row: Option<Value>,
}

pub fn parse_pagespeed_response(
    body: &Value,
    site: &str,
    requested_url: &str,
    run_date: &str,
    strategy: &Strategy,
    top_audits: u32,
) -> ParsedPageSpeed {
    let lh = body.pointer("/lighthouseResult").unwrap_or(body);
    let categories = lh.pointer("/categories").and_then(|v| v.as_object());
    let score = |cat: &str| {
        categories
            .and_then(|c| c.get(cat))
            .and_then(|v| v.get("score"))
            .and_then(|v| v.as_f64())
            .map(|s| (s * 100.0).round())
    };
    let audits = lh.pointer("/audits").and_then(|v| v.as_object());
    let audit_metric = |id: &str| {
        audits
            .and_then(|a| a.get(id))
            .and_then(|v| v.get("numericValue"))
            .and_then(|v| v.as_f64())
    };
    let field = body.get("loadingExperience");
    let field_available = field.is_some();
    let field_category = |metric: &str| {
        field
            .and_then(|f| f.get("metrics"))
            .and_then(|m| m.get(metric))
            .and_then(|m| m.get("category"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let final_url = lh
        .get("finalUrl")
        .or_else(|| body.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or(requested_url)
        .to_string();
    let mut page = Map::new();
    page.insert("site".into(), json!(site));
    page.insert("run_date".into(), json!(run_date));
    page.insert("canonical_url".into(), json!(requested_url));
    page.insert("strategy".into(), json!(strategy.as_api_str()));
    page.insert("final_url".into(), json!(final_url));
    page.insert("lh_performance".into(), json!(score("performance")));
    page.insert("lh_accessibility".into(), json!(score("accessibility")));
    page.insert("lh_best_practices".into(), json!(score("best-practices")));
    page.insert("lh_seo".into(), json!(score("seo")));
    page.insert(
        "lcp_ms".into(),
        json!(audit_metric("largest-contentful-paint")),
    );
    page.insert("cls".into(), json!(audit_metric("cumulative-layout-shift")));
    page.insert(
        "inp_ms".into(),
        json!(audit_metric("interaction-to-next-paint")),
    );
    page.insert(
        "fcp_ms".into(),
        json!(audit_metric("first-contentful-paint")),
    );
    page.insert("tbt_ms".into(), json!(audit_metric("total-blocking-time")));
    page.insert("speed_index_ms".into(), json!(audit_metric("speed-index")));
    page.insert(
        "analysis_timestamp".into(),
        body.get("analysisUTCTimestamp")
            .cloned()
            .unwrap_or(Value::Null),
    );
    page.insert("field_data_available".into(), json!(field_available));
    page.insert(
        "field_overall_category".into(),
        field
            .and_then(|f| f.get("overall_category"))
            .cloned()
            .unwrap_or(Value::Null),
    );
    page.insert(
        "field_lcp_category".into(),
        json!(field_category("LARGEST_CONTENTFUL_PAINT_MS")),
    );
    page.insert(
        "field_cls_category".into(),
        json!(field_category("CUMULATIVE_LAYOUT_SHIFT_SCORE")),
    );
    page.insert(
        "field_inp_category".into(),
        json!(field_category("INTERACTION_TO_NEXT_PAINT")),
    );
    page.insert(
        "field_fcp_category".into(),
        json!(field_category("FIRST_CONTENTFUL_PAINT_MS")),
    );
    page.insert(
        "field_ttfb_category".into(),
        json!(field_category("EXPERIMENTAL_TIME_TO_FIRST_BYTE")),
    );
    let page_row = Value::Object(page.clone());
    let issue_rows =
        issue::issues_for_page(site, requested_url, run_date, strategy.as_api_str(), &page);
    let audit_rows = top_failing_audits(lh, site, requested_url, run_date, strategy, top_audits);
    let field_origin_row = parse_origin_field(body, site, run_date);
    ParsedPageSpeed {
        page_rows: vec![page_row],
        audit_rows,
        issue_rows,
        field_origin_row,
    }
}

fn top_failing_audits(
    lh: &Value,
    site: &str,
    url: &str,
    run_date: &str,
    strategy: &Strategy,
    cap: u32,
) -> Vec<Value> {
    let Some(audits) = lh.pointer("/audits").and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for (audit_id, audit) in audits {
        let score = audit.get("score").and_then(|v| v.as_f64());
        if score.is_some_and(|s| s >= 0.9) {
            continue;
        }
        rows.push(json!({
            "site": site,
            "run_date": run_date,
            "canonical_url": url,
            "strategy": strategy.as_api_str(),
            "audit_id": audit_id,
            "score": score,
            "display_value": audit.get("displayValue"),
            "title": audit.get("title"),
        }));
        if rows.len() >= cap as usize {
            break;
        }
    }
    rows
}

fn parse_origin_field(body: &Value, site: &str, run_date: &str) -> Option<Value> {
    let origin = body.get("originLoadingExperience")?;
    Some(json!({
        "site": site,
        "run_date": run_date,
        "origin_host": site,
        "overall_category": origin.get("overall_category"),
        "metrics": origin.get("metrics"),
    }))
}

pub fn site_run_daily_row(
    site: &str,
    run_date: &str,
    urls_tested: u32,
    field_available_pct: f64,
    median_lh_performance_mobile: Option<f64>,
) -> Value {
    json!({
        "site": site,
        "run_date": run_date,
        "urls_tested": urls_tested,
        "field_data_available_pct": field_available_pct,
        "median_lh_performance_mobile": median_lh_performance_mobile,
    })
}

pub const PARSED_NAMESPACES: &[&str] = &[
    NAMESPACE_PAGE_DAILY,
    NAMESPACE_FIELD_ORIGIN_DAILY,
    NAMESPACE_AUDIT_DAILY,
    NAMESPACE_CHECK_DAILY,
    NAMESPACE_SITE_RUN_DAILY,
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Strategy;

    #[test]
    fn parses_mobile_fixture_categories() {
        let body: Value =
            serde_json::from_str(include_str!("../fixtures/run_pagespeed_mobile.json"))
                .expect("fixture");
        let parsed = parse_pagespeed_response(
            &body,
            "https://example.com/",
            "https://example.com/",
            "2026-05-30",
            &Strategy::Mobile,
            5,
        );
        assert_eq!(parsed.page_rows.len(), 1);
        assert!(parsed.page_rows[0]["lh_performance"].is_number());
    }
}
