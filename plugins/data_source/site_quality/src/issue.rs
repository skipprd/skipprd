use serde_json::{json, Value};

use crate::worker::WorkerJobResult;

pub const CLS_POOR: &str = "CLS_POOR";
pub const LCP_SLOW: &str = "LCP_SLOW";
pub const TTFB_SLOW: &str = "TTFB_SLOW";
pub const INP_SLOW: &str = "INP_SLOW";
pub const MISSING_VIEWPORT: &str = "MISSING_VIEWPORT";
pub const HORIZONTAL_SCROLL: &str = "HORIZONTAL_SCROLL";
pub const TEXT_TOO_SMALL: &str = "TEXT_TOO_SMALL";
pub const TAP_TARGETS: &str = "TAP_TARGETS";
pub const LH_PERFORMANCE_LOW: &str = "LH_PERFORMANCE_LOW";
pub const AXE_CRITICAL: &str = "AXE_CRITICAL";
pub const NAVIGATION_TIMEOUT: &str = "NAVIGATION_TIMEOUT";
pub const HTTP_ERROR: &str = "HTTP_ERROR";
pub const SOCIAL_PREVIEW_METADATA: &str = "SOCIAL_PREVIEW_METADATA";

/// Google Core Web Vitals "good" boundaries (lab).
const CLS_THRESHOLD: f64 = 0.1;
const LCP_SLOW_MS: f64 = 2500.0;
const TTFB_SLOW_MS: f64 = 800.0;
const INP_SLOW_MS: f64 = 200.0;
const LH_PERF_LOW: f64 = 50.0;

#[derive(Debug, Clone, Copy)]
pub struct IssueThresholds {
    pub cls_poor: f64,
    pub lcp_slow_ms: f64,
    pub ttfb_slow_ms: f64,
    pub inp_slow_ms: f64,
    pub lh_performance_low: f64,
}

impl Default for IssueThresholds {
    fn default() -> Self {
        Self {
            cls_poor: CLS_THRESHOLD,
            lcp_slow_ms: LCP_SLOW_MS,
            ttfb_slow_ms: TTFB_SLOW_MS,
            inp_slow_ms: INP_SLOW_MS,
            lh_performance_low: LH_PERF_LOW,
        }
    }
}

/// Emits one row per scorecard check (pass, warn, or fail) for traffic-light dashboards.
pub fn map_issues(
    site: &str,
    canonical_url: &str,
    run_date: &str,
    device_profile: &str,
    result: &WorkerJobResult,
    thresholds: &IssueThresholds,
    lighthouse_enabled: bool,
    axe_enabled: bool,
) -> Vec<Value> {
    let mut rows = Vec::new();
    let page_url = canonical_url;

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
            device_profile,
            code,
            "run",
            "fail",
            "error",
            result
                .error
                .as_ref()
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("page lab failed"),
        ));
        return rows;
    }

    let status = result.status.unwrap_or(0);
    if status >= 400 {
        rows.push(check_row(
            site,
            page_url,
            run_date,
            device_profile,
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
            device_profile,
            HTTP_ERROR,
            "http",
            "pass",
            "info",
            &format!("HTTP {status}"),
        ));
    }

    if let Some(social) = &result.social_preview {
        if !social.missing_fields.is_empty() {
            rows.push(check_row(
                site,
                page_url,
                run_date,
                device_profile,
                SOCIAL_PREVIEW_METADATA,
                "metadata",
                "fail",
                "warning",
                format!(
                    "missing social preview metadata: {}",
                    social.missing_fields.join(", ")
                ),
            ));
        } else if !social.card_missing_fields.is_empty() {
            rows.push(check_row(
                site,
                page_url,
                run_date,
                device_profile,
                SOCIAL_PREVIEW_METADATA,
                "metadata",
                "warn",
                "warning",
                format!(
                    "social preview card metadata incomplete: {}",
                    social.card_missing_fields.join(", ")
                ),
            ));
        } else {
            rows.push(check_row(
                site,
                page_url,
                run_date,
                device_profile,
                SOCIAL_PREVIEW_METADATA,
                "metadata",
                "pass",
                "info",
                "social preview metadata complete",
            ));
        }
    } else {
        rows.push(check_row(
            site,
            page_url,
            run_date,
            device_profile,
            SOCIAL_PREVIEW_METADATA,
            "metadata",
            "fail",
            "warning",
            "social preview metadata unavailable",
        ));
    }

    if let Some(vitals) = &result.web_vitals {
        let cls = vitals.cls.unwrap_or(0.0);
        if cls > thresholds.cls_poor {
            rows.push(check_row(
                site,
                page_url,
                run_date,
                device_profile,
                CLS_POOR,
                "vitals",
                "fail",
                "warning",
                &format!("CLS {cls:.3} exceeds {:.3}", thresholds.cls_poor),
            ));
        } else {
            rows.push(check_row(
                site,
                page_url,
                run_date,
                device_profile,
                CLS_POOR,
                "vitals",
                "pass",
                "info",
                &format!("CLS {cls:.3} within threshold",),
            ));
        }

        let lcp = vitals.lcp.unwrap_or(0.0);
        if lcp > thresholds.lcp_slow_ms {
            rows.push(check_row(
                site,
                page_url,
                run_date,
                device_profile,
                LCP_SLOW,
                "vitals",
                "fail",
                "warning",
                &format!("LCP {lcp:.0}ms exceeds {:.0}ms", thresholds.lcp_slow_ms),
            ));
        } else {
            rows.push(check_row(
                site,
                page_url,
                run_date,
                device_profile,
                LCP_SLOW,
                "vitals",
                "pass",
                "info",
                &format!("LCP {lcp:.0}ms within threshold"),
            ));
        }

        if let Some(ttfb) = vitals.ttfb {
            if ttfb > thresholds.ttfb_slow_ms {
                rows.push(check_row(
                    site,
                    page_url,
                    run_date,
                    device_profile,
                    TTFB_SLOW,
                    "vitals",
                    "fail",
                    "warning",
                    &format!("TTFB {ttfb:.0}ms exceeds {:.0}ms", thresholds.ttfb_slow_ms),
                ));
            } else {
                rows.push(check_row(
                    site,
                    page_url,
                    run_date,
                    device_profile,
                    TTFB_SLOW,
                    "vitals",
                    "pass",
                    "info",
                    &format!("TTFB {ttfb:.0}ms within threshold"),
                ));
            }
        }

        if let Some(inp) = vitals.inp {
            if inp > thresholds.inp_slow_ms {
                rows.push(check_row(
                    site,
                    page_url,
                    run_date,
                    device_profile,
                    INP_SLOW,
                    "vitals",
                    "fail",
                    "warning",
                    &format!("INP {inp:.0}ms exceeds {:.0}ms", thresholds.inp_slow_ms),
                ));
            } else {
                rows.push(check_row(
                    site,
                    page_url,
                    run_date,
                    device_profile,
                    INP_SLOW,
                    "vitals",
                    "pass",
                    "info",
                    &format!("INP {inp:.0}ms within threshold"),
                ));
            }
        } else {
            rows.push(check_row(
                site,
                page_url,
                run_date,
                device_profile,
                INP_SLOW,
                "vitals",
                "pass",
                "info",
                "INP not measured in lab run (no interaction timing)",
            ));
        }
    }

    if device_profile == "mobile" {
        if let Some(heuristics) = &result.mobile_heuristics {
            let viewport_ok = heuristics.viewport_meta_ok.unwrap_or(false);
            rows.push(check_row(
                site,
                page_url,
                run_date,
                device_profile,
                MISSING_VIEWPORT,
                "mobile",
                if viewport_ok { "pass" } else { "fail" },
                if viewport_ok { "info" } else { "warning" },
                if viewport_ok {
                    "viewport meta tag present"
                } else {
                    "viewport meta tag missing or invalid"
                },
            ));

            let hscroll = heuristics.horizontal_scroll.unwrap_or(false);
            rows.push(check_row(
                site,
                page_url,
                run_date,
                device_profile,
                HORIZONTAL_SCROLL,
                "mobile",
                if hscroll { "fail" } else { "pass" },
                if hscroll { "warning" } else { "info" },
                if hscroll {
                    "horizontal scroll detected on mobile viewport"
                } else {
                    "no horizontal scroll on mobile viewport"
                },
            ));

            let small = heuristics.text_too_small_count.unwrap_or(0);
            rows.push(check_row(
                site,
                page_url,
                run_date,
                device_profile,
                TEXT_TOO_SMALL,
                "mobile",
                if small > 0 { "warn" } else { "pass" },
                if small > 0 { "warning" } else { "info" },
                if small > 0 {
                    format!("{small} elements with font size below 12px")
                } else {
                    "no undersized text elements".into()
                },
            ));

            let tap = heuristics.tap_target_issues.unwrap_or(0);
            rows.push(check_row(
                site,
                page_url,
                run_date,
                device_profile,
                TAP_TARGETS,
                "mobile",
                if tap > 0 { "warn" } else { "pass" },
                if tap > 0 { "warning" } else { "info" },
                if tap > 0 {
                    format!("{tap} tap targets smaller than 48×48px")
                } else {
                    "tap targets meet minimum size".into()
                },
            ));
        }
    }

    if lighthouse_enabled {
        if let Some(lh) = &result.lighthouse {
            let perf = lh.performance.unwrap_or(0.0);
            if perf < thresholds.lh_performance_low {
                rows.push(check_row(
                    site,
                    page_url,
                    run_date,
                    device_profile,
                    LH_PERFORMANCE_LOW,
                    "lighthouse",
                    "fail",
                    "warning",
                    &format!(
                        "Lighthouse performance {perf:.0} below {:.0}",
                        thresholds.lh_performance_low
                    ),
                ));
            } else {
                rows.push(check_row(
                    site,
                    page_url,
                    run_date,
                    device_profile,
                    LH_PERFORMANCE_LOW,
                    "lighthouse",
                    "pass",
                    "info",
                    &format!("Lighthouse performance {perf:.0}"),
                ));
            }
        }
    }

    if axe_enabled {
        let critical = result
            .axe_violations
            .as_ref()
            .map(|v| v.iter().any(|x| x.impact.as_deref() == Some("critical")))
            .unwrap_or(false);
        rows.push(check_row(
            site,
            page_url,
            run_date,
            device_profile,
            AXE_CRITICAL,
            "accessibility",
            if critical { "fail" } else { "pass" },
            if critical { "error" } else { "info" },
            if critical {
                "axe reported critical accessibility violations"
            } else {
                "no critical axe violations"
            },
        ));
    }

    rows
}

fn check_row(
    site: &str,
    page_url: &str,
    run_date: &str,
    device_profile: &str,
    issue_code: &str,
    section: &str,
    status: &str,
    severity: &str,
    message: impl AsRef<str>,
) -> Value {
    json!({
        "site": site,
        "page_url": page_url,
        "run_date": run_date,
        "device_profile": device_profile,
        "issue_code": issue_code,
        "section": section,
        "status": status,
        "severity": severity,
        "message": message.as_ref(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker::{MobileHeuristics, SocialPreview, WebVitals, WorkerJobResult};

    fn complete_social_preview() -> SocialPreview {
        SocialPreview {
            title: Some("Example page".into()),
            description: Some("Useful summary".into()),
            image: Some("https://example.com/social.png".into()),
            url: Some("https://example.com/".into()),
            card: Some("summary_large_image".into()),
            card_title: None,
            card_description: None,
            card_image: None,
            title_present: Some(true),
            description_present: Some(true),
            image_present: Some(true),
            url_present: Some(true),
            card_present: Some(true),
            card_title_present: Some(true),
            card_description_present: Some(true),
            card_image_present: Some(true),
            missing_fields: Vec::new(),
            card_missing_fields: Vec::new(),
            complete: Some(true),
        }
    }

    #[test]
    fn scorecard_includes_pass_and_fail_rows() {
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
            social_preview: Some(complete_social_preview()),
            lighthouse: None,
            axe_violations: None,
            error: None,
            skip_heavy_audits: None,
        };
        let rows = map_issues(
            "https://example.com",
            "https://example.com/",
            "2024-06-01",
            "mobile",
            &result,
            &IssueThresholds::default(),
            false,
            true,
        );
        let statuses: Vec<_> = rows
            .iter()
            .filter_map(|r| r.get("status").and_then(|s| s.as_str()))
            .collect();
        assert!(statuses.contains(&"pass"));
        assert!(statuses.contains(&"fail"));
        assert!(rows.iter().any(|r| {
            r.get("issue_code").and_then(|c| c.as_str()) == Some(AXE_CRITICAL)
                && r.get("status").and_then(|s| s.as_str()) == Some("pass")
        }));
        assert!(rows.iter().any(|r| {
            r.get("issue_code").and_then(|c| c.as_str()) == Some(TTFB_SLOW)
                && r.get("status").and_then(|s| s.as_str()) == Some("pass")
        }));
    }

    #[test]
    fn ttfb_and_inp_fail_above_threshold() {
        let result = WorkerJobResult {
            job_id: "j1".into(),
            ok: true,
            final_url: Some("https://example.com/".into()),
            status: Some(200),
            redirect_count: Some(0),
            timings_ms: None,
            web_vitals: Some(WebVitals {
                lcp: Some(1000.0),
                inp: Some(350.0),
                cls: Some(0.05),
                fcp: Some(500.0),
                ttfb: Some(1200.0),
            }),
            render_hash: None,
            mobile_heuristics: None,
            social_preview: Some(SocialPreview {
                missing_fields: vec!["image".into()],
                complete: Some(false),
                ..complete_social_preview()
            }),
            lighthouse: None,
            axe_violations: None,
            error: None,
            skip_heavy_audits: None,
        };
        let rows = map_issues(
            "https://example.com",
            "https://example.com/",
            "2024-06-01",
            "desktop",
            &result,
            &IssueThresholds::default(),
            false,
            false,
        );
        assert!(rows.iter().any(|r| {
            r.get("issue_code").and_then(|c| c.as_str()) == Some(TTFB_SLOW)
                && r.get("status").and_then(|s| s.as_str()) == Some("fail")
        }));
        assert!(rows.iter().any(|r| {
            r.get("issue_code").and_then(|c| c.as_str()) == Some(INP_SLOW)
                && r.get("status").and_then(|s| s.as_str()) == Some("fail")
        }));
        assert!(rows.iter().any(|r| {
            r.get("issue_code").and_then(|c| c.as_str()) == Some(SOCIAL_PREVIEW_METADATA)
                && r.get("status").and_then(|s| s.as_str()) == Some("fail")
        }));
    }
}
