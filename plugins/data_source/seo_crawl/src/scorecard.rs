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
    let title_len_ok = (30..=65).contains(&title_len) || title_len == 0;
    rows.push(check_row(
        site,
        crawl_date,
        page_url,
        "TITLE_LENGTH",
        "metadata",
        if title_len_ok || !title_ok {
            "pass"
        } else if title_len < 30 {
            "warn"
        } else {
            "warn"
        },
        "info",
        &format!("title length is {title_len} characters (target ~30–65)"),
    ));

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
    let meta_len_ok = (70..=160).contains(&meta_len) || !meta_ok;
    rows.push(check_row(
        site,
        crawl_date,
        page_url,
        "META_LENGTH",
        "metadata",
        if meta_len_ok {
            "pass"
        } else {
            "warn"
        },
        "info",
        &format!("meta description length is {meta_len} characters (target ~70–160)"),
    ));

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

    rows.push(check_row(
        site,
        crawl_date,
        page_url,
        "MULTIPLE_H1",
        "structure",
        if page.h1_count <= 1 { "pass" } else { "fail" },
        if page.h1_count <= 1 { "info" } else { "medium" },
        &format!("{} H1 element(s) on page", page.h1_count),
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

    if page.img_missing_alt > 0 {
        rows.push(check_row(
            site,
            crawl_date,
            page_url,
            "IMG_ALT_EMPTY",
            "technical",
            "fail",
            "medium",
            &format!(
                "{} of {} images missing alt text",
                page.img_missing_alt, page.img_count
            ),
        ));
    }

    let static_links_ok = !page.issues.iter().any(|i| i.issue_code == "SPA_SHELL_NO_STATIC_LINKS");
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
            "no JSON-LD structured data found".into()
        },
    ));

    rows.push(check_row(
        site,
        crawl_date,
        page_url,
        "FAQ_SCHEMA",
        "aio_structure",
        if page.has_faq_schema { "pass" } else { "warn" },
        "low",
        if page.has_faq_schema {
            "FAQPage or similar schema detected"
        } else {
            "no FAQ structured data detected"
        },
    ));

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

    let question_headings = page
        .heading_outline
        .iter()
        .filter(|h| h.text.contains('?'))
        .count();
    rows.push(check_row(
        site,
        crawl_date,
        page_url,
        "AIO_ANSWER_HEADINGS",
        "aio_structure",
        if question_headings > 0 || page.has_faq_schema {
            "pass"
        } else {
            "warn"
        },
        "low",
        if question_headings > 0 {
            format!("{question_headings} question-style heading(s) for direct answers")
        } else {
            "no question-style headings detected for AIO answer blocks".into()
        },
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
            if sitemap_url_count > 0 { "pass" } else { "warn" },
            if sitemap_url_count > 0 { "info" } else { "medium" },
            if sitemap_url_count > 0 {
                format!("{sitemap_url_count} sitemap URL entries discovered")
            } else {
                "no sitemap URLs discovered".into()
            },
        ),
    ]
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
