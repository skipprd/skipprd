use serde_json::{json, Value};

use crate::worker::WorkerJobResult;

pub const CLS_POOR: &str = "CLS_POOR";
pub const LCP_SLOW: &str = "LCP_SLOW";
pub const MISSING_VIEWPORT: &str = "MISSING_VIEWPORT";
pub const HORIZONTAL_SCROLL: &str = "HORIZONTAL_SCROLL";
pub const LH_PERFORMANCE_LOW: &str = "LH_PERFORMANCE_LOW";
pub const AXE_CRITICAL: &str = "AXE_CRITICAL";
pub const NAVIGATION_TIMEOUT: &str = "NAVIGATION_TIMEOUT";
pub const HTTP_ERROR: &str = "HTTP_ERROR";

const CLS_THRESHOLD: f64 = 0.1;
const LCP_SLOW_MS: f64 = 2500.0;
const LH_PERF_LOW: f64 = 50.0;

#[derive(Debug, Clone, Copy)]
pub struct IssueThresholds {
    pub cls_poor: f64,
    pub lcp_slow_ms: f64,
    pub lh_performance_low: f64,
}

impl Default for IssueThresholds {
    fn default() -> Self {
        Self {
            cls_poor: CLS_THRESHOLD,
            lcp_slow_ms: LCP_SLOW_MS,
            lh_performance_low: LH_PERF_LOW,
        }
    }
}

pub fn map_issues(
    site: &str,
    canonical_url: &str,
    run_date: &str,
    device_profile: &str,
    result: &WorkerJobResult,
    thresholds: &IssueThresholds,
) -> Vec<Value> {
    let mut issues = Vec::new();
    let page_url = canonical_url;

    if !result.ok {
        let code = result
            .error
            .as_ref()
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_str())
            .unwrap_or(NAVIGATION_TIMEOUT);
        issues.push(issue_row(
            site,
            page_url,
            run_date,
            device_profile,
            code,
            "error",
            result
                .error
                .as_ref()
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("page lab failed"),
        ));
        return issues;
    }

    if let Some(status) = result.status {
        if status >= 400 {
            issues.push(issue_row(
                site,
                page_url,
                run_date,
                device_profile,
                HTTP_ERROR,
                "error",
                &format!("HTTP {status}"),
            ));
        }
    }

    if let Some(vitals) = &result.web_vitals {
        if vitals.cls.unwrap_or(0.0) > thresholds.cls_poor {
            issues.push(issue_row(
                site,
                page_url,
                run_date,
                device_profile,
                CLS_POOR,
                "warning",
                &format!("CLS {:.3} exceeds {}", vitals.cls.unwrap_or(0.0), thresholds.cls_poor),
            ));
        }
        if vitals.lcp.unwrap_or(0.0) > thresholds.lcp_slow_ms {
            issues.push(issue_row(
                site,
                page_url,
                run_date,
                device_profile,
                LCP_SLOW,
                "warning",
                &format!(
                    "LCP {:.0}ms exceeds {:.0}ms",
                    vitals.lcp.unwrap_or(0.0),
                    thresholds.lcp_slow_ms
                ),
            ));
        }
    }

    if let Some(heuristics) = &result.mobile_heuristics {
        if device_profile == "mobile" {
            if heuristics.viewport_meta_ok == Some(false) {
                issues.push(issue_row(
                    site,
                    page_url,
                    run_date,
                    device_profile,
                    MISSING_VIEWPORT,
                    "warning",
                    "viewport meta tag missing or invalid",
                ));
            }
            if heuristics.horizontal_scroll == Some(true) {
                issues.push(issue_row(
                    site,
                    page_url,
                    run_date,
                    device_profile,
                    HORIZONTAL_SCROLL,
                    "warning",
                    "horizontal scroll detected on mobile viewport",
                ));
            }
        }
    }

    if let Some(lh) = &result.lighthouse {
        if lh.performance.unwrap_or(100.0) < thresholds.lh_performance_low {
            issues.push(issue_row(
                site,
                page_url,
                run_date,
                device_profile,
                LH_PERFORMANCE_LOW,
                "warning",
                &format!(
                    "Lighthouse performance {:.0} below {:.0}",
                    lh.performance.unwrap_or(0.0),
                    thresholds.lh_performance_low
                ),
            ));
        }
    }

    if let Some(violations) = &result.axe_violations {
        for v in violations {
            if v.impact.as_deref() == Some("critical") {
                issues.push(issue_row(
                    site,
                    page_url,
                    run_date,
                    device_profile,
                    AXE_CRITICAL,
                    "error",
                    v.help.as_deref().unwrap_or(&v.id),
                ));
            }
        }
    }

    issues
}

fn issue_row(
    site: &str,
    page_url: &str,
    run_date: &str,
    device_profile: &str,
    issue_code: &str,
    severity: &str,
    message: &str,
) -> Value {
    json!({
        "site": site,
        "page_url": page_url,
        "run_date": run_date,
        "device_profile": device_profile,
        "issue_code": issue_code,
        "severity": severity,
        "message": message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker::{MobileHeuristics, WebVitals, WorkerJobResult};

    #[test]
    fn issue_mapping_thresholds() {
        let result = WorkerJobResult {
            job_id: "j1".into(),
            ok: true,
            final_url: Some("https://example.com/".into()),
            status: Some(200),
            redirect_count: Some(0),
            timings_ms: None,
            web_vitals: Some(WebVitals {
                lcp: Some(3000.0),
                inp: Some(100.0),
                cls: Some(0.15),
                fcp: Some(900.0),
                ttfb: Some(200.0),
            }),
            render_hash: Some("sha256:abc".into()),
            mobile_heuristics: Some(MobileHeuristics {
                viewport_meta_ok: Some(false),
                horizontal_scroll: Some(true),
                text_too_small_count: Some(0),
                tap_target_issues: Some(0),
            }),
            lighthouse: None,
            axe_violations: None,
            error: None,
            skip_heavy_audits: None,
        };
        let issues = map_issues(
            "https://example.com",
            "https://example.com/",
            "2024-06-01",
            "mobile",
            &result,
            &IssueThresholds::default(),
        );
        let codes: Vec<_> = issues
            .iter()
            .filter_map(|r| r.get("issue_code").and_then(|c| c.as_str()))
            .collect();
        assert!(codes.contains(&CLS_POOR));
        assert!(codes.contains(&LCP_SLOW));
        assert!(codes.contains(&MISSING_VIEWPORT));
        assert!(codes.contains(&HORIZONTAL_SCROLL));
    }
}
