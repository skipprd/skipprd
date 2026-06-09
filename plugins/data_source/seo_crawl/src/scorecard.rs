use serde_json::{json, Value};

use crate::html::{heading_hierarchy_ok, repetitive_token_ratio, ParsedPage};

pub fn page_check_rows(
    site: &str,
    crawl_date: &str,
    page_url: &str,
    page: &ParsedPage,
    http_status: u16,
) -> Vec<Value> {
    let mut rows = Vec::new();

    let title_ok = page.title.as_ref().is_some_and(|t| !t.is_empty());
    rows.push(check_row(
        site,
        crawl_date,
        page_url,
        "TITLE",
        "metadata",
        if title_ok { "pass" } else { "fail" },
        if title_ok { "info" } else { "high" },
        if title_ok {
            "title element present"
        } else {
            "page is missing a title element"
        },
    ));

    let title_len = page.title.as_ref().map(|t| t.chars().count()).unwrap_or(0);
    let title_len_ok = (30..=65).contains(&title_len);
    if title_ok && !title_len_ok {
        rows.push(check_row(
            site,
            crawl_date,
            page_url,
            "TITLE_LENGTH",
            "metadata",
            "warn",
            "info",
            &format!("title length is {title_len} characters (target ~30–65)"),
        ));
    }

    let meta_ok = page
        .meta_description
        .as_ref()
        .is_some_and(|d| !d.is_empty());
    rows.push(check_row(
        site,
        crawl_date,
        page_url,
        "META_DESCRIPTION",
        "metadata",
        if meta_ok { "pass" } else { "fail" },
        if meta_ok { "info" } else { "medium" },
        if meta_ok {
            "meta description present"
        } else {
            "page is missing meta description"
        },
    ));

    let meta_len = page
        .meta_description
        .as_ref()
        .map(|d| d.chars().count())
        .unwrap_or(0);
    let meta_len_ok = (70..=160).contains(&meta_len);
    if meta_ok && !meta_len_ok {
        rows.push(check_row(
            site,
            crawl_date,
            page_url,
            "META_LENGTH",
            "metadata",
            "warn",
            "info",
            &format!("meta description length is {meta_len} characters (target ~70–160)"),
        ));
    }

    let h1_ok = page.h1_count == 1;
    rows.push(check_row(
        site,
        crawl_date,
        page_url,
        "H1",
        "metadata",
        if h1_ok { "pass" } else { "fail" },
        if h1_ok { "info" } else { "medium" },
        if page.h1_count == 0 {
            "page is missing H1"
        } else if page.h1_count > 1 {
            "multiple H1 elements found"
        } else {
            "single H1 present"
        },
    ));

    let multiple_h1_ok = multiple_h1_structure_ok(page);
    rows.push(check_row(
        site,
        crawl_date,
        page_url,
        "MULTIPLE_H1",
        "structure",
        if multiple_h1_ok { "pass" } else { "warn" },
        if multiple_h1_ok { "info" } else { "medium" },
        if page.h1_count <= 1 {
            format!("{} H1 element(s) on page", page.h1_count)
        } else if page.h1_structured_count == page.h1_count {
            format!(
                "{} H1 elements found within crawlable structural landmarks",
                page.h1_count
            )
        } else {
            format!(
                "{} H1 elements found; {} are inside clear structural landmarks",
                page.h1_count, page.h1_structured_count
            )
        },
    ));

    let hierarchy_ok = heading_hierarchy_ok(&page.heading_outline);
    rows.push(check_row(
        site,
        crawl_date,
        page_url,
        "HEADING_HIERARCHY",
        "structure",
        if hierarchy_ok { "pass" } else { "warn" },
        if hierarchy_ok { "info" } else { "medium" },
        if hierarchy_ok {
            "heading levels do not skip (e.g. H2 → H4)"
        } else {
            "heading hierarchy skips levels"
        },
    ));

    let alt_ok = page.img_count == 0 || page.img_missing_alt == 0;
    rows.push(check_row(
        site,
        crawl_date,
        page_url,
        "MISSING_ALT",
        "technical",
        if alt_ok { "pass" } else { "fail" },
        if alt_ok { "info" } else { "medium" },
        if page.img_count == 0 {
            "no images on page"
        } else if page.img_missing_alt == 0 {
            "all images have alt attributes"
        } else {
            "one or more images missing alt text"
        },
    ));

    let static_links_ok = !page
        .issues
        .iter()
        .any(|i| i.issue_code == "SPA_SHELL_NO_STATIC_LINKS");
    rows.push(check_row(
        site,
        crawl_date,
        page_url,
        "STATIC_INTERNAL_LINKS",
        "crawl",
        if static_links_ok { "pass" } else { "fail" },
        if static_links_ok { "info" } else { "medium" },
        if static_links_ok {
            "static HTML exposes internal links"
        } else {
            "JS SPA shell with no crawlable static internal links"
        },
    ));

    let structured_ok = page.structured_data_count > 0;
    if structured_ok || structured_data_recommended(page_url) {
        rows.push(check_row(
            site,
            crawl_date,
            page_url,
            "STRUCTURED_DATA",
            "metadata",
            if structured_ok { "pass" } else { "warn" },
            if structured_ok { "info" } else { "low" },
            if structured_ok {
                format!("{} JSON-LD script(s) found", page.structured_data_count)
            } else {
                "no JSON-LD structured data found on a content or landing URL".into()
            },
        ));
    }

    let rep_ratio = repetitive_token_ratio(&page.main_text);
    let stuffing = rep_ratio > 0.08;
    rows.push(check_row(
        site,
        crawl_date,
        page_url,
        "REPETITIVE_PHRASE",
        "structure",
        if !stuffing { "pass" } else { "warn" },
        if !stuffing { "info" } else { "medium" },
        &format!(
            "top token repetition ratio {:.1}% on main text",
            rep_ratio * 100.0
        ),
    ));

    let http_ok = http_status < 400;
    rows.push(check_row(
        site,
        crawl_date,
        page_url,
        "HTTP_STATUS",
        "http",
        if http_ok { "pass" } else { "fail" },
        if http_ok { "info" } else { "high" },
        &format!("HTTP {http_status}"),
    ));

    rows
}

pub fn site_check_rows(
    site: &str,
    crawl_date: &str,
    robots_found: bool,
    sitemap_url_count: usize,
) -> Vec<Value> {
    let site_url = site;
    vec![
        check_row(
            site,
            crawl_date,
            site_url,
            "ROBOTS_TXT",
            "site",
            if robots_found { "pass" } else { "fail" },
            if robots_found { "info" } else { "medium" },
            if robots_found {
                "robots.txt fetched"
            } else {
                "robots.txt not found or not fetchable"
            },
        ),
        check_row(
            site,
            crawl_date,
            site_url,
            "SITEMAP_URLS",
            "site",
            if sitemap_url_count > 0 {
                "pass"
            } else {
                "warn"
            },
            if sitemap_url_count > 0 {
                "info"
            } else {
                "medium"
            },
            if sitemap_url_count > 0 {
                format!("{sitemap_url_count} sitemap URL entries discovered")
            } else {
                "no sitemap URLs discovered".into()
            },
        ),
    ]
}

fn multiple_h1_structure_ok(page: &ParsedPage) -> bool {
    if page.h1_count <= 1 {
        return true;
    }
    page.h1_structured_count == page.h1_count && heading_hierarchy_ok(&page.heading_outline)
}

fn structured_data_recommended(page_url: &str) -> bool {
    let path = page_url
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('/').map(|(_, path)| format!("/{path}")))
        .unwrap_or_else(|| "/".to_string())
        .to_lowercase();
    path == "/"
        || path.starts_with("/blog")
        || path.starts_with("/docs")
        || path.starts_with("/guide")
        || path.starts_with("/product")
        || path.starts_with("/pricing")
        || path.starts_with("/features")
}

fn check_row(
    site: &str,
    crawl_date: &str,
    page_url: &str,
    issue_code: &str,
    section: &str,
    status: &str,
    severity: &str,
    message: impl AsRef<str>,
) -> Value {
    json!({
        "site": site,
        "crawl_date": crawl_date,
        "page_url": page_url,
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
    use crate::html::{HeadingEntry, IssueRow, LinkEdge, ParsedLink};
    use serde_json::json;

    fn page_with_h1s(h1_count: u32, h1_structured_count: u32) -> ParsedPage {
        ParsedPage {
            canonical_url: "https://example.com/blog/post".into(),
            title: Some("A useful article title for testing".into()),
            meta_description: Some(
                "A useful meta description that is long enough for the scorecard test.".into(),
            ),
            h1: Some("Primary topic".into()),
            h1_count,
            h1_structured_count,
            heading_outline: vec![
                HeadingEntry {
                    level: 1,
                    text: "Primary topic".into(),
                },
                HeadingEntry {
                    level: 2,
                    text: "Supporting section".into(),
                },
            ],
            img_count: 0,
            img_missing_alt: 0,
            head: json!({}),
            http_headers: json!({}),
            links: Vec::<ParsedLink>::new(),
            internal_links: Vec::<LinkEdge>::new(),
            main_text: "This is test body copy with enough words to avoid repetition checks."
                .into(),
            content_hash: "sha256:test".into(),
            structured_data_count: 0,
            has_faq_schema: false,
            technical_score: 1.0,
            issues: Vec::<IssueRow>::new(),
        }
    }

    #[test]
    fn seo_crawl_does_not_emit_aio_structure_checks() {
        let rows = page_check_rows(
            "https://example.com",
            "2026-06-09",
            "https://example.com/blog/post",
            &page_with_h1s(1, 1),
            200,
        );
        let codes: Vec<String> = rows
            .iter()
            .filter_map(|r| {
                r.get("issue_code")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .collect();
        assert!(!codes.iter().any(|c| c == "FAQ_SCHEMA"));
        assert!(!codes.iter().any(|c| c == "AIO_ANSWER_HEADINGS"));
    }

    #[test]
    fn multiple_h1_passes_when_all_h1s_have_clear_structure() {
        let rows = page_check_rows(
            "https://example.com",
            "2026-06-09",
            "https://example.com/blog/post",
            &page_with_h1s(2, 2),
            200,
        );
        let row = rows
            .iter()
            .find(|r| r.get("issue_code").and_then(|v| v.as_str()) == Some("MULTIPLE_H1"))
            .expect("multiple h1 row");
        assert_eq!(row.get("status").and_then(|v| v.as_str()), Some("pass"));
    }

    #[test]
    fn multiple_h1_warns_when_structure_is_ambiguous() {
        let rows = page_check_rows(
            "https://example.com",
            "2026-06-09",
            "https://example.com/blog/post",
            &page_with_h1s(2, 1),
            200,
        );
        let row = rows
            .iter()
            .find(|r| r.get("issue_code").and_then(|v| v.as_str()) == Some("MULTIPLE_H1"))
            .expect("multiple h1 row");
        assert_eq!(row.get("status").and_then(|v| v.as_str()), Some("warn"));
    }
}
