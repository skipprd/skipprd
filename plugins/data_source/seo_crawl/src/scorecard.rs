use serde_json::{json, Value};

use crate::html::ParsedPage;

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

    let h1_ok = page.h1.as_ref().is_some_and(|h| !h.is_empty());
    rows.push(check_row(
        site,
        crawl_date,
        page_url,
        "H1",
        "metadata",
        if h1_ok { "pass" } else { "fail" },
        if h1_ok { "info" } else { "medium" },
        if h1_ok {
            "H1 present"
        } else {
            "page is missing H1"
        },
    ));

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
