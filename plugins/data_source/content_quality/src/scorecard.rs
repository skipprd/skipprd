use serde_json::{json, Value};

use crate::checkpoint::PageScores;
use crate::html::ParsedPage;

const SEO_WARN: f64 = 0.55;
const SEO_FAIL: f64 = 0.4;
const AIO_WARN: f64 = 0.55;
const AIO_FAIL: f64 = 0.4;
const MIN_BLOCK_WORDS: usize = 20;

pub fn page_check_rows(
    site: &str,
    run_date: &str,
    page_url: &str,
    page: &ParsedPage,
    scores: &PageScores,
    block_scores: &[Value],
) -> Vec<Value> {
    let mut rows = Vec::new();

    rows.push(score_check(
        site,
        run_date,
        page_url,
        "LOW_CONTENT_QUALITY",
        "content_seo",
        scores.seo_content_score,
        SEO_WARN,
        SEO_FAIL,
        "SEO content quality score from block helpfulness rollup",
    ));

    rows.push(score_check(
        site,
        run_date,
        page_url,
        "LOW_AIO_READINESS",
        "content_aio",
        scores.aio_score,
        AIO_WARN,
        AIO_FAIL,
        "AIO readiness score from block extractability rollup",
    ));

    rows.push(score_check(
        site,
        run_date,
        page_url,
        "LOW_EEAT_PROXY",
        "content_seo",
        scores.eeat_proxy_score,
        SEO_WARN,
        SEO_FAIL,
        "E-E-A-T proxy score from block trust signals",
    ));

    let no_blocks = page.blocks.is_empty();
    let block_msg = if no_blocks {
        "no extractable content blocks found".to_string()
    } else {
        format!("{} content blocks extracted", page.blocks.len())
    };
    rows.push(check_row(
        site,
        run_date,
        page_url,
        "NO_CONTENT_BLOCKS",
        "content_seo",
        if no_blocks { "fail" } else { "pass" },
        if no_blocks { "high" } else { "info" },
        block_msg,
    ));

    let thin = page.blocks.iter().any(|b| b.word_count < MIN_BLOCK_WORDS);
    rows.push(check_row(
        site,
        run_date,
        page_url,
        "THIN_CONTENT_BLOCK",
        "content_seo",
        if thin { "warn" } else { "pass" },
        if thin { "medium" } else { "info" },
        if thin {
            "one or more blocks have very low word count"
        } else {
            "all blocks meet minimum word count threshold"
        },
    ));

    let low_citation = block_scores.iter().any(|s| {
        s.get("citation_worthiness_score")
            .and_then(|v| v.as_f64())
            .unwrap_or(1.0)
            < 0.45
    });
    if !block_scores.is_empty() {
        rows.push(check_row(
            site,
            run_date,
            page_url,
            "LOW_CITATION_WORTHINESS",
            "content_aio",
            if low_citation { "warn" } else { "pass" },
            if low_citation { "medium" } else { "info" },
            if low_citation {
                "at least one block has low citation-worthiness"
            } else {
                "blocks meet citation-worthiness threshold"
            },
        ));
    }

    let missing_answer = block_scores
        .iter()
        .any(|s| s.get("is_self_contained").and_then(|v| v.as_bool()) == Some(false));
    if !block_scores.is_empty() {
        rows.push(check_row(
            site,
            run_date,
            page_url,
            "MISSING_SELF_CONTAINED_ANSWER",
            "content_aio",
            if missing_answer { "warn" } else { "pass" },
            if missing_answer { "medium" } else { "info" },
            if missing_answer {
                "some blocks are not self-contained for AI citation"
            } else {
                "blocks appear self-contained for direct answers"
            },
        ));
    }

    rows
}

fn score_check(
    site: &str,
    run_date: &str,
    page_url: &str,
    issue_code: &str,
    section: &str,
    score: f64,
    warn_below: f64,
    fail_below: f64,
    message: &str,
) -> Value {
    let (status, severity) = if score >= warn_below {
        ("pass", "info")
    } else if score >= fail_below {
        ("warn", "medium")
    } else {
        ("fail", "high")
    };
    check_row(
        site,
        run_date,
        page_url,
        issue_code,
        section,
        status,
        severity,
        &format!("{message} (score {:.2})", score),
    )
}

fn check_row(
    site: &str,
    run_date: &str,
    page_url: &str,
    issue_code: &str,
    section: &str,
    status: &str,
    severity: &str,
    message: impl AsRef<str>,
) -> Value {
    json!({
        "site": site,
        "run_date": run_date,
        "page_url": page_url,
        "issue_code": issue_code,
        "section": section,
        "status": status,
        "severity": severity,
        "message": message.as_ref(),
    })
}
